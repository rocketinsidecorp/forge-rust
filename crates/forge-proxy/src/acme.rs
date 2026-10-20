//! An ACME (RFC 8555) client for the HTTP-01 challenge, enough to obtain a
//! certificate for one DNS name from Let's Encrypt.
//!
//! Requests are JWS-signed with the account's P-256 key (ES256, `ring`);
//! the certificate key is a separate P-256 key generated per certificate.
//! Every request runs over its own HTTPS connection verified against the
//! webpki root store; plain HTTP is accepted only for a loopback directory
//! (tests and local ACME servers).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use bytes::Bytes;
use http::header::{self, HeaderMap, HeaderValue};
use http::{Method, Request, StatusCode};
use http_body_util::{BodyExt as _, Full};
use hyper_util::rt::TokioIo;
use ring::rand::SystemRandom;
use ring::signature::{
    ECDSA_P256_SHA256_ASN1_SIGNING, ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair as _,
};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite};

/// Let's Encrypt's production directory.
pub const LETS_ENCRYPT: &str = "https://acme-v02.api.letsencrypt.org/directory";

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_RESPONSE: usize = 1 << 20;
const POLL_BUDGET: Duration = Duration::from_secs(120);

/// Pending HTTP-01 key authorizations, by token, served on
/// `/.well-known/acme-challenge/TOKEN`.
pub type Challenges = Arc<Mutex<HashMap<String, String>>>;

struct Response {
    status: StatusCode,
    headers: HeaderMap,
    body: Bytes,
}

impl Response {
    fn header(&self, name: header::HeaderName) -> Option<String> {
        self.headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    }

    fn json(&self) -> Result<Value, String> {
        serde_json::from_slice(&self.body)
            .map_err(|error| format!("acme: malformed response: {error}"))
    }

    fn retry_after(&self) -> Duration {
        self.header(header::RETRY_AFTER)
            .and_then(|value| value.trim().parse::<u64>().ok())
            .map_or(Duration::from_secs(1), |seconds| {
                Duration::from_secs(seconds.clamp(1, 10))
            })
    }
}

struct Directory {
    new_nonce: String,
    new_account: String,
    new_order: String,
}

/// One ACME account at one directory.
pub struct Client {
    directory_url: String,
    key: EcdsaKeyPair,
    jwk: Value,
    thumbprint: String,
    email: String,
    directory: Option<Directory>,
    account: Option<String>,
    nonce: Option<String>,
    tls: tokio_rustls::TlsConnector,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Client")
            .field("directory_url", &self.directory_url)
            .finish_non_exhaustive()
    }
}

/// A URL's parts, restricted to what ACME needs.
struct Endpoint {
    https: bool,
    host: String,
    port: u16,
    authority: String,
    path: String,
}

fn endpoint(url: &str) -> Result<Endpoint, String> {
    let parsed = url::Url::parse(url).map_err(|_| format!("acme: invalid URL {url:?}"))?;
    let https = match parsed.scheme() {
        "https" => true,
        "http" => false,
        _ => return Err(format!("acme: unsupported URL {url:?}")),
    };
    let host = parsed
        .host_str()
        .ok_or_else(|| format!("acme: URL without host {url:?}"))?
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_owned();
    if !https {
        let loopback = host == "localhost"
            || host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|address| address.is_loopback());
        if !loopback {
            return Err(format!(
                "acme: {url:?} is not HTTPS; plain HTTP is accepted only on loopback"
            ));
        }
    }
    let port = parsed
        .port_or_known_default()
        .ok_or_else(|| format!("acme: URL without port {url:?}"))?;
    let authority = match parsed.port() {
        Some(port) => format!("{}:{port}", parsed.host_str().unwrap_or_default()),
        None => parsed.host_str().unwrap_or_default().to_owned(),
    };
    let mut path = parsed.path().to_owned();
    if let Some(query) = parsed.query() {
        path.push('?');
        path.push_str(query);
    }
    Ok(Endpoint {
        https,
        host,
        port,
        authority,
        path,
    })
}

fn b64(data: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(data)
}

/// The problem document of an error response, as text.
fn problem(response: &Response) -> String {
    match response.json() {
        Ok(document) => format!(
            "acme: {} ({}): {}",
            document["type"].as_str().unwrap_or("error"),
            response.status.as_u16(),
            document["detail"].as_str().unwrap_or("no detail")
        ),
        Err(_) => format!("acme: HTTP {}", response.status.as_u16()),
    }
}

impl Client {
    /// A client for `directory_url` with the account key `pkcs8` (P-256).
    ///
    /// # Errors
    ///
    /// The key is not a P-256 PKCS#8 key, or the TLS configuration failed.
    pub fn new(directory_url: &str, account_pkcs8: &[u8], email: &str) -> Result<Self, String> {
        let key = EcdsaKeyPair::from_pkcs8(
            &ECDSA_P256_SHA256_FIXED_SIGNING,
            account_pkcs8,
            &SystemRandom::new(),
        )
        .map_err(|_| "acme: the account key is not a P-256 key".to_owned())?;
        let point = key.public_key().as_ref();
        if point.len() != 65 || point[0] != 4 {
            return Err("acme: unexpected public key encoding".to_owned());
        }
        let x = b64(&point[1..33]);
        let y = b64(&point[33..65]);
        // RFC 7638: members in lexicographic order, no whitespace.
        let canonical = format!(r#"{{"crv":"P-256","kty":"EC","x":"{x}","y":"{y}"}}"#);
        let thumbprint =
            b64(ring::digest::digest(&ring::digest::SHA256, canonical.as_bytes()).as_ref());
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let config = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|error| format!("acme: tls: {error}"))?
        .with_root_certificates(roots)
        .with_no_client_auth();
        endpoint(directory_url)?;
        Ok(Self {
            directory_url: directory_url.to_owned(),
            key,
            jwk: json!({"crv": "P-256", "kty": "EC", "x": x, "y": y}),
            thumbprint,
            email: email.to_owned(),
            directory: None,
            account: None,
            nonce: None,
            tls: tokio_rustls::TlsConnector::from(Arc::new(config)),
        })
    }

    /// The key authorization of `token` (token "." JWK thumbprint).
    #[must_use]
    pub fn key_authorization(&self, token: &str) -> String {
        format!("{token}.{}", self.thumbprint)
    }

    async fn http(
        &self,
        method: Method,
        url: &str,
        body: Option<Vec<u8>>,
    ) -> Result<Response, String> {
        let target = endpoint(url)?;
        let exchange = async {
            let tcp = tokio::net::TcpStream::connect((target.host.as_str(), target.port))
                .await
                .map_err(|error| format!("acme: connect {}: {error}", target.authority))?;
            if target.https {
                let name = rustls::pki_types::ServerName::try_from(target.host.clone())
                    .map_err(|_| "acme: invalid server name".to_owned())?;
                let stream = self
                    .tls
                    .connect(name, tcp)
                    .await
                    .map_err(|error| format!("acme: tls {}: {error}", target.authority))?;
                exchange(stream, &target, method, body).await
            } else {
                exchange(tcp, &target, method, body).await
            }
        };
        tokio::time::timeout(REQUEST_TIMEOUT, exchange)
            .await
            .map_err(|_| format!("acme: {url} timed out"))?
    }

    async fn directory(&mut self) -> Result<(), String> {
        if self.directory.is_some() {
            return Ok(());
        }
        let response = self
            .http(Method::GET, &self.directory_url.clone(), None)
            .await?;
        if !response.status.is_success() {
            return Err(problem(&response));
        }
        let document = response.json()?;
        let field = |name: &str| {
            document[name]
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("acme: directory lacks {name}"))
        };
        self.directory = Some(Directory {
            new_nonce: field("newNonce")?,
            new_account: field("newAccount")?,
            new_order: field("newOrder")?,
        });
        Ok(())
    }

    async fn fresh_nonce(&mut self) -> Result<String, String> {
        if let Some(nonce) = self.nonce.take() {
            return Ok(nonce);
        }
        self.directory().await?;
        let url = self
            .directory
            .as_ref()
            .map(|directory| directory.new_nonce.clone())
            .unwrap_or_default();
        let response = self.http(Method::HEAD, &url, None).await?;
        response
            .header(header::HeaderName::from_static("replay-nonce"))
            .ok_or_else(|| "acme: no Replay-Nonce".to_owned())
    }

    fn sign(
        &self,
        url: &str,
        nonce: &str,
        payload: Option<&Value>,
        use_jwk: bool,
    ) -> Result<Vec<u8>, String> {
        let mut protected = json!({"alg": "ES256", "nonce": nonce, "url": url});
        if use_jwk {
            protected["jwk"] = self.jwk.clone();
        } else {
            protected["kid"] = Value::String(self.account.clone().unwrap_or_default());
        }
        let protected = b64(protected.to_string().as_bytes());
        let payload = payload.map_or_else(String::new, |value| b64(value.to_string().as_bytes()));
        let signature = self
            .key
            .sign(
                &SystemRandom::new(),
                format!("{protected}.{payload}").as_bytes(),
            )
            .map_err(|_| "acme: signing failed".to_owned())?;
        Ok(json!({
            "protected": protected,
            "payload": payload,
            "signature": b64(signature.as_ref()),
        })
        .to_string()
        .into_bytes())
    }

    /// A signed POST (`payload: None` is POST-as-GET), retried on badNonce.
    async fn post(
        &mut self,
        url: &str,
        payload: Option<&Value>,
        use_jwk: bool,
    ) -> Result<Response, String> {
        let mut attempts = 0;
        loop {
            attempts += 1;
            let nonce = self.fresh_nonce().await?;
            let body = self.sign(url, &nonce, payload, use_jwk)?;
            let response = self.http(Method::POST, url, Some(body)).await?;
            self.nonce = response.header(header::HeaderName::from_static("replay-nonce"));
            if response.status.is_success() {
                return Ok(response);
            }
            let bad_nonce = response
                .json()
                .ok()
                .is_some_and(|document| document["type"] == "urn:ietf:params:acme:error:badNonce");
            if bad_nonce && attempts < 3 {
                continue;
            }
            return Err(problem(&response));
        }
    }

    async fn ensure_account(&mut self) -> Result<(), String> {
        if self.account.is_some() {
            return Ok(());
        }
        self.directory().await?;
        let url = self
            .directory
            .as_ref()
            .map(|directory| directory.new_account.clone())
            .unwrap_or_default();
        let mut payload = json!({"termsOfServiceAgreed": true});
        if !self.email.is_empty() {
            payload["contact"] = json!([format!("mailto:{}", self.email)]);
        }
        let response = self.post(&url, Some(&payload), true).await?;
        let location = response
            .header(header::LOCATION)
            .ok_or_else(|| "acme: account without Location".to_owned())?;
        self.account = Some(location);
        Ok(())
    }

    /// Obtains a certificate for `host` and returns its PEM chain, signed
    /// for the PKCS#8 key `certificate_key`. HTTP-01 key authorizations
    /// are published in `challenges` while the CA validates them.
    ///
    /// # Errors
    ///
    /// Any ACME, network or validation failure, with the CA's detail.
    pub async fn issue(
        &mut self,
        host: &str,
        certificate_key: &[u8],
        challenges: &Challenges,
    ) -> Result<String, String> {
        self.ensure_account().await?;
        let new_order = self
            .directory
            .as_ref()
            .map(|directory| directory.new_order.clone())
            .unwrap_or_default();
        let payload = json!({"identifiers": [{"type": "dns", "value": host}]});
        let response = self.post(&new_order, Some(&payload), false).await?;
        let order_url = response
            .header(header::LOCATION)
            .ok_or_else(|| "acme: order without Location".to_owned())?;
        let order = response.json()?;
        let authorizations: Vec<String> = order["authorizations"]
            .as_array()
            .map(|list| {
                list.iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        let finalize = order["finalize"]
            .as_str()
            .ok_or_else(|| "acme: order without finalize URL".to_owned())?
            .to_owned();
        for authorization in authorizations {
            self.authorize(&authorization, challenges).await?;
        }
        let key = EcdsaKeyPair::from_pkcs8(
            &ECDSA_P256_SHA256_ASN1_SIGNING,
            certificate_key,
            &SystemRandom::new(),
        )
        .map_err(|_| "acme: the certificate key is not a P-256 key".to_owned())?;
        let csr = crate::der::certificate_request(host, &key)?;
        self.post(&finalize, Some(&json!({"csr": b64(&csr)})), false)
            .await?;
        let order = self.poll(&order_url, "order").await?;
        let certificate = order["certificate"]
            .as_str()
            .ok_or_else(|| "acme: valid order without certificate URL".to_owned())?
            .to_owned();
        let response = self.post(&certificate, None, false).await?;
        String::from_utf8(response.body.to_vec())
            .map_err(|_| "acme: certificate is not PEM".to_owned())
    }

    async fn authorize(&mut self, url: &str, challenges: &Challenges) -> Result<(), String> {
        let authorization = self.post(url, None, false).await?.json()?;
        if authorization["status"] == "valid" {
            return Ok(());
        }
        let challenge = authorization["challenges"]
            .as_array()
            .and_then(|list| list.iter().find(|c| c["type"] == "http-01"))
            .ok_or_else(|| "acme: no http-01 challenge offered".to_owned())?;
        let token = challenge["token"]
            .as_str()
            .filter(|token| {
                !token.is_empty()
                    && token
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            })
            .ok_or_else(|| "acme: invalid challenge token".to_owned())?
            .to_owned();
        let challenge_url = challenge["url"]
            .as_str()
            .ok_or_else(|| "acme: challenge without URL".to_owned())?
            .to_owned();
        let key_authorization = self.key_authorization(&token);
        lock(challenges).insert(token.clone(), key_authorization);
        let outcome = async {
            self.post(&challenge_url, Some(&json!({})), false).await?;
            self.poll(url, "authorization").await.map(|_| ())
        }
        .await;
        lock(challenges).remove(&token);
        outcome
    }

    /// Polls `url` until its status is `valid`.
    async fn poll(&mut self, url: &str, what: &str) -> Result<Value, String> {
        let deadline = tokio::time::Instant::now() + POLL_BUDGET;
        loop {
            let response = self.post(url, None, false).await?;
            let document = response.json()?;
            match document["status"].as_str() {
                Some("valid") => return Ok(document),
                Some("invalid") => {
                    let detail = document["challenges"]
                        .as_array()
                        .and_then(|list| list.iter().find_map(|c| c["error"]["detail"].as_str()))
                        .or_else(|| document["error"]["detail"].as_str())
                        .unwrap_or("no detail");
                    return Err(format!("acme: {what} is invalid: {detail}"));
                }
                _ => {}
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(format!(
                    "acme: {what} not valid after {}s",
                    POLL_BUDGET.as_secs()
                ));
            }
            tokio::time::sleep(response.retry_after()).await;
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

async fn exchange<IO>(
    io: IO,
    target: &Endpoint,
    method: Method,
    body: Option<Vec<u8>>,
) -> Result<Response, String>
where
    IO: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(io))
        .await
        .map_err(|error| format!("acme: http: {error}"))?;
    let has_body = body.is_some();
    let mut request = Request::builder()
        .method(method)
        .uri(target.path.as_str())
        .body(Full::new(Bytes::from(body.unwrap_or_default())))
        .map_err(|_| "acme: invalid request".to_owned())?;
    let headers = request.headers_mut();
    headers.insert(
        header::HOST,
        HeaderValue::from_str(&target.authority).map_err(|_| "acme: invalid host".to_owned())?,
    );
    headers.insert(header::USER_AGENT, HeaderValue::from_static("forge-proxy"));
    headers.insert(header::CONNECTION, HeaderValue::from_static("close"));
    if has_body {
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/jose+json"),
        );
    }
    let exchange = async {
        let response = sender
            .send_request(request)
            .await
            .map_err(|error| format!("acme: http: {error}"))?;
        let (parts, mut body) = response.into_parts();
        let mut collected = Vec::new();
        while let Some(frame) = body.frame().await {
            let frame = frame.map_err(|error| format!("acme: http: {error}"))?;
            if let Some(data) = frame.data_ref() {
                collected.extend_from_slice(data);
                if collected.len() > MAX_RESPONSE {
                    return Err("acme: response too large".to_owned());
                }
            }
        }
        Ok(Response {
            status: parts.status,
            headers: parts.headers,
            body: Bytes::from(collected),
        })
    };
    tokio::pin!(connection);
    tokio::pin!(exchange);
    tokio::select! {
        result = &mut exchange => result,
        _ = &mut connection => exchange.await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_http_only_on_loopback() {
        assert!(endpoint("http://127.0.0.1:14000/dir").is_ok());
        assert!(endpoint("http://localhost/dir").is_ok());
        assert!(endpoint("https://acme.example.com/dir").is_ok());
        assert!(endpoint("http://acme.example.com/dir").is_err());
        assert!(endpoint("ftp://127.0.0.1/dir").is_err());
    }

    #[test]
    fn thumbprint_follows_rfc7638() {
        let pkcs8 = crate::der::generate_p256().expect("key");
        let client = Client::new("http://127.0.0.1:1/dir", &pkcs8, "").expect("client");
        let x = client.jwk["x"].as_str().expect("x");
        let y = client.jwk["y"].as_str().expect("y");
        let canonical = format!(r#"{{"crv":"P-256","kty":"EC","x":"{x}","y":"{y}"}}"#);
        let expected =
            b64(ring::digest::digest(&ring::digest::SHA256, canonical.as_bytes()).as_ref());
        assert_eq!(client.key_authorization("tok"), format!("tok.{expected}"));
    }
}
