//! HTTPS and ACME over real sockets. The certificates come from a test CA
//! made with the `openssl` command (the tests are skipped, with a notice,
//! where it is missing); the ACME server is a small RFC 8555 implementation
//! in this file that verifies every JWS signature and nonce and validates
//! the HTTP-01 challenge through the proxy's plain listener.

mod support;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use bytes::Bytes;
use forge_proxy::tls::{AcmeSettings, Tls};
use http::{Request, Response, StatusCode};
use http_body_util::{BodyExt as _, Full};
use hyper_util::rt::TokioIo;
use rustls::pki_types::pem::PemObject as _;
use rustls::pki_types::{CertificateDer, ServerName};
use serde_json::{Value, json};
use support::{TEST_HOST, backend, fast_request, front, get, limits};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

fn openssl(dir: &Path, args: &[&str]) -> bool {
    Command::new("openssl")
        .args(args)
        .current_dir(dir)
        .output()
        .is_ok_and(|output| output.status.success())
}

/// A throwaway CA in `dir` (ca.key, ca.pem); `false` without openssl.
fn test_ca(dir: &Path) -> bool {
    if !openssl(dir, &["version"]) {
        eprintln!("skipped: the openssl command is not installed");
        return false;
    }
    assert!(openssl(
        dir,
        &[
            "ecparam",
            "-name",
            "prime256v1",
            "-genkey",
            "-noout",
            "-out",
            "ca.key"
        ]
    ));
    assert!(openssl(
        dir,
        &[
            "req",
            "-x509",
            "-new",
            "-key",
            "ca.key",
            "-subj",
            "/CN=Forge Test CA",
            "-days",
            "100",
            "-addext",
            "basicConstraints=critical,CA:TRUE",
            "-addext",
            "keyUsage=critical,keyCertSign,cRLSign",
            "-out",
            "ca.pem",
        ]
    ));
    true
}

/// Signs `csr` (DER) with the test CA; returns the leaf PEM.
fn sign(dir: &Path, csr: &[u8]) -> Option<String> {
    std::fs::write(dir.join("request.der"), csr).ok()?;
    if !openssl(
        dir,
        &[
            "req",
            "-inform",
            "DER",
            "-in",
            "request.der",
            "-verify",
            "-noout",
        ],
    ) {
        return None;
    }
    if !openssl(
        dir,
        &[
            "x509",
            "-req",
            "-inform",
            "DER",
            "-in",
            "request.der",
            "-CA",
            "ca.pem",
            "-CAkey",
            "ca.key",
            "-CAcreateserial",
            "-days",
            "90",
            "-copy_extensions",
            "copyall",
            "-out",
            "leaf.pem",
        ],
    ) {
        return None;
    }
    std::fs::read_to_string(dir.join("leaf.pem")).ok()
}

fn client_config(dir: &Path) -> Arc<rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    for certificate in CertificateDer::pem_file_iter(dir.join("ca.pem")).expect("ca") {
        roots.add(certificate.expect("certificate")).expect("root");
    }
    Arc::new(
        rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .expect("versions")
        .with_root_certificates(roots)
        .with_no_client_auth(),
    )
}

/// One HTTPS GET with SNI `name`.
async fn https_get(
    config: Arc<rustls::ClientConfig>,
    addr: std::net::SocketAddr,
    name: &str,
) -> Result<(u16, String), String> {
    let tcp = tokio::net::TcpStream::connect(addr)
        .await
        .map_err(|e| e.to_string())?;
    let stream = tokio_rustls::TlsConnector::from(config)
        .connect(
            ServerName::try_from(name.to_owned()).map_err(|e| e.to_string())?,
            tcp,
        )
        .await
        .map_err(|e| e.to_string())?;
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .map_err(|e| e.to_string())?;
    tokio::spawn(connection);
    let request = Request::get("/")
        .header("host", name)
        .body(Full::new(Bytes::new()))
        .map_err(|e| e.to_string())?;
    let response = sender
        .send_request(request)
        .await
        .map_err(|e| e.to_string())?;
    let status = response.status().as_u16();
    let body = response
        .into_body()
        .collect()
        .await
        .map_err(|e| e.to_string())?
        .to_bytes();
    Ok((status, String::from_utf8_lossy(&body).into_owned()))
}

async fn serve_tls(
    server: forge_proxy::Server,
    tls: Arc<Tls>,
    stop: CancellationToken,
) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(
        async move { forge_proxy::serve_tls(server, listener, &tls, limits(), stop).await },
    );
    addr
}

#[tokio::test]
async fn certificates_are_served_only_for_tls_hosts() {
    let dir = tempfile::tempdir().expect("tempdir");
    if !test_ca(dir.path()) {
        return;
    }
    assert!(openssl(
        dir.path(),
        &[
            "ecparam",
            "-name",
            "prime256v1",
            "-genkey",
            "-noout",
            "-out",
            "leaf.key"
        ]
    ));
    assert!(openssl(
        dir.path(),
        &[
            "req",
            "-new",
            "-key",
            "leaf.key",
            "-subj",
            "/CN=app.example.com",
            "-addext",
            "subjectAltName=DNS:app.example.com",
            "-outform",
            "DER",
            "-out",
            "leaf.der"
        ]
    ));
    let csr = std::fs::read(dir.path().join("leaf.der")).expect("csr");
    let leaf = sign(dir.path(), &csr).expect("signed");
    let mut pem = std::fs::read_to_string(dir.path().join("leaf.key")).expect("key");
    pem.push_str(&leaf);

    let app = backend("app", None).await;
    let front = front(None).await;
    let mut request = fast_request(&app.addr);
    request.tls = true;
    front.server.deploy(request).await.expect("deploy");
    let tls = Arc::new(
        Tls::new(
            front.server.clone(),
            AcmeSettings {
                cache: dir.path().join("acme"),
                email: String::new(),
                directory: "http://127.0.0.1:1/directory".to_owned(),
            },
        )
        .expect("tls"),
    );
    // A key that does not match the certificate is refused.
    let mut mismatched = std::fs::read_to_string(dir.path().join("ca.key")).expect("ca key");
    mismatched.push_str(&leaf);
    assert!(tls.install(TEST_HOST, mismatched.as_bytes()).is_err());
    tls.install(TEST_HOST, pem.as_bytes()).expect("install");
    let addr = serve_tls(front.server.clone(), Arc::clone(&tls), front.stop.clone()).await;
    let config = client_config(dir.path());
    assert_eq!(
        https_get(Arc::clone(&config), addr, TEST_HOST).await,
        Ok((200, "app".to_owned()))
    );
    // No certificate for a host that is not a TLS service: no handshake.
    assert!(
        https_get(Arc::clone(&config), addr, "other.example.com")
            .await
            .is_err()
    );
    // The plain listener still redirects.
    assert_eq!(get(front.addr, TEST_HOST, "/").await.0.as_u16(), 308);
}

#[derive(Default)]
struct Ca {
    nonces: HashSet<String>,
    counter: u64,
    accounts: HashMap<String, Value>,
    orders: u64,
    authorized: bool,
    certificate: Option<String>,
    failures: Vec<String>,
}

struct AcmeServer {
    base: String,
    dir: PathBuf,
    proxy: std::net::SocketAddr,
    state: Mutex<Ca>,
}

fn b64(data: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(data)
}

impl AcmeServer {
    fn nonce(&self) -> String {
        let mut state = self.state.lock().expect("lock");
        state.counter += 1;
        let nonce = format!("nonce-{}", state.counter);
        state.nonces.insert(nonce.clone());
        nonce
    }

    /// Verifies the JWS and returns its payload (`Null` for POST-as-GET)
    /// and the account key.
    fn verify(&self, url: &str, body: &[u8]) -> Result<(Value, Value), String> {
        let jws: Value = serde_json::from_slice(body).map_err(|e| e.to_string())?;
        let protected_b64 = jws["protected"].as_str().ok_or("no protected")?;
        let payload_b64 = jws["payload"].as_str().ok_or("no payload")?;
        let signature = URL_SAFE_NO_PAD
            .decode(jws["signature"].as_str().ok_or("no signature")?)
            .map_err(|e| e.to_string())?;
        let protected: Value = serde_json::from_slice(
            &URL_SAFE_NO_PAD
                .decode(protected_b64)
                .map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        if protected["alg"] != "ES256" || protected["url"] != url {
            return Err(format!("bad protected header {protected}"));
        }
        let nonce = protected["nonce"].as_str().ok_or("no nonce")?.to_owned();
        let mut state = self.state.lock().expect("lock");
        if !state.nonces.remove(&nonce) {
            return Err(format!("replayed or unknown nonce {nonce}"));
        }
        let jwk = if let Some(kid) = protected["kid"].as_str() {
            state.accounts.get(kid).cloned().ok_or("unknown account")?
        } else {
            protected["jwk"].clone()
        };
        drop(state);
        let x = URL_SAFE_NO_PAD
            .decode(jwk["x"].as_str().ok_or("no x")?)
            .map_err(|e| e.to_string())?;
        let y = URL_SAFE_NO_PAD
            .decode(jwk["y"].as_str().ok_or("no y")?)
            .map_err(|e| e.to_string())?;
        let mut point = vec![4_u8];
        point.extend_from_slice(&x);
        point.extend_from_slice(&y);
        ring::signature::UnparsedPublicKey::new(&ring::signature::ECDSA_P256_SHA256_FIXED, point)
            .verify(
                format!("{protected_b64}.{payload_b64}").as_bytes(),
                &signature,
            )
            .map_err(|_| "bad signature".to_owned())?;
        let payload = if payload_b64.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(
                &URL_SAFE_NO_PAD
                    .decode(payload_b64)
                    .map_err(|e| e.to_string())?,
            )
            .map_err(|e| e.to_string())?
        };
        Ok((payload, jwk))
    }

    async fn handle(
        self: Arc<Self>,
        request: Request<hyper::body::Incoming>,
    ) -> Response<Full<Bytes>> {
        let path = request.uri().path().to_owned();
        let url = format!("{}{path}", self.base);
        let method = request.method().clone();
        let body = request
            .into_body()
            .collect()
            .await
            .map(|b| b.to_bytes())
            .unwrap_or_default();
        let reply = |status: u16, body: Value, location: Option<String>| {
            let mut response = Response::new(Full::new(Bytes::from(body.to_string())));
            *response.status_mut() = StatusCode::from_u16(status).expect("status");
            let headers = response.headers_mut();
            headers.insert("replay-nonce", self.nonce().parse().expect("nonce"));
            if let Some(location) = location {
                headers.insert("location", location.parse().expect("location"));
            }
            response
        };
        if path == "/directory" {
            let base = &self.base;
            return reply(
                200,
                json!({"newNonce": format!("{base}/nonce"), "newAccount": format!("{base}/account"), "newOrder": format!("{base}/order")}),
                None,
            );
        }
        if path == "/nonce" {
            return reply(200, Value::Null, None);
        }
        if method != http::Method::POST {
            return reply(405, json!({"type": "malformed"}), None);
        }
        let (payload, jwk) = match self.verify(&url, &body) {
            Ok(verified) => verified,
            Err(error) => {
                self.state
                    .lock()
                    .expect("lock")
                    .failures
                    .push(error.clone());
                return reply(
                    400,
                    json!({"type": "urn:ietf:params:acme:error:malformed", "detail": error}),
                    None,
                );
            }
        };
        let base = self.base.clone();
        match path.as_str() {
            "/account" => {
                if payload["termsOfServiceAgreed"] != true {
                    return reply(
                        400,
                        json!({"type": "malformed", "detail": "terms not agreed"}),
                        None,
                    );
                }
                let kid = format!("{base}/acct/1");
                self.state
                    .lock()
                    .expect("lock")
                    .accounts
                    .insert(kid.clone(), jwk);
                reply(201, json!({"status": "valid"}), Some(kid))
            }
            "/order" => {
                assert_eq!(payload["identifiers"][0]["value"], TEST_HOST);
                self.state.lock().expect("lock").orders += 1;
                reply(
                    201,
                    json!({"status": "pending", "authorizations": [format!("{base}/authz/1")], "finalize": format!("{base}/finalize/1")}),
                    Some(format!("{base}/order/1")),
                )
            }
            "/authz/1" => {
                let status = if self.state.lock().expect("lock").authorized {
                    "valid"
                } else {
                    "pending"
                };
                reply(
                    200,
                    json!({"status": status, "identifier": {"type": "dns", "value": TEST_HOST},
                    "challenges": [{"type": "http-01", "url": format!("{base}/chall/1"), "token": "tok-123_abc", "status": status}]}),
                    None,
                )
            }
            "/chall/1" => {
                // Validate through the proxy's plain listener, as the CA would.
                let canonical = format!(
                    r#"{{"crv":"P-256","kty":"EC","x":"{}","y":"{}"}}"#,
                    jwk["x"].as_str().unwrap_or(""),
                    jwk["y"].as_str().unwrap_or("")
                );
                let thumbprint =
                    b64(ring::digest::digest(&ring::digest::SHA256, canonical.as_bytes()).as_ref());
                let (code, _, body) = get(
                    self.proxy,
                    TEST_HOST,
                    "/.well-known/acme-challenge/tok-123_abc",
                )
                .await;
                let valid = code.as_u16() == 200 && body == format!("tok-123_abc.{thumbprint}");
                if !valid {
                    self.state
                        .lock()
                        .expect("lock")
                        .failures
                        .push(format!("challenge answer {code} {body:?}"));
                }
                self.state.lock().expect("lock").authorized = valid;
                reply(
                    200,
                    json!({"type": "http-01", "status": if valid { "valid" } else { "invalid" }, "token": "tok-123_abc"}),
                    None,
                )
            }
            "/finalize/1" => {
                let csr = URL_SAFE_NO_PAD
                    .decode(payload["csr"].as_str().unwrap_or(""))
                    .unwrap_or_default();
                match sign(&self.dir, &csr) {
                    Some(leaf) => {
                        let ca = std::fs::read_to_string(self.dir.join("ca.pem")).expect("ca");
                        self.state.lock().expect("lock").certificate = Some(format!("{leaf}{ca}"));
                        reply(200, json!({"status": "processing"}), None)
                    }
                    None => {
                        self.state
                            .lock()
                            .expect("lock")
                            .failures
                            .push("CSR rejected by openssl".to_owned());
                        reply(
                            400,
                            json!({"type": "urn:ietf:params:acme:error:badCSR", "detail": "bad CSR"}),
                            None,
                        )
                    }
                }
            }
            "/order/1" => {
                let ready = self.state.lock().expect("lock").certificate.is_some();
                if ready {
                    reply(
                        200,
                        json!({"status": "valid", "certificate": format!("{base}/cert/1")}),
                        None,
                    )
                } else {
                    reply(200, json!({"status": "processing"}), None)
                }
            }
            "/cert/1" => {
                let chain = self
                    .state
                    .lock()
                    .expect("lock")
                    .certificate
                    .clone()
                    .unwrap_or_default();
                let mut response = reply(200, Value::Null, None);
                *response.body_mut() = Full::new(Bytes::from(chain));
                response
            }
            _ => reply(404, json!({"type": "malformed", "detail": "unknown"}), None),
        }
    }
}

async fn start_acme(
    dir: PathBuf,
    proxy: std::net::SocketAddr,
    stop: CancellationToken,
) -> Arc<AcmeServer> {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    let server = Arc::new(AcmeServer {
        base,
        dir,
        proxy,
        state: Mutex::new(Ca::default()),
    });
    let acme = Arc::clone(&server);
    tokio::spawn(async move {
        loop {
            let accepted = tokio::select! {
                accepted = listener.accept() => accepted,
                () = stop.cancelled() => return,
            };
            let Ok((stream, _)) = accepted else { continue };
            let acme = Arc::clone(&acme);
            tokio::spawn(async move {
                let service = hyper::service::service_fn(move |request| {
                    let acme = Arc::clone(&acme);
                    async move { Ok::<_, std::convert::Infallible>(acme.handle(request).await) }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    server
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acme_obtains_caches_and_reuses_a_certificate() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = tempfile::tempdir().expect("tempdir");
    if !test_ca(dir.path()) {
        return;
    }
    let app = backend("app", None).await;
    let front = front(None).await;
    let stop = CancellationToken::new();
    let acme = start_acme(dir.path().to_owned(), front.addr, stop.clone()).await;
    let cache = dir.path().join("acme");
    let settings = AcmeSettings {
        cache: cache.clone(),
        email: "ops@example.com".to_owned(),
        directory: format!("{}/directory", acme.base),
    };
    let tls = Arc::new(Tls::new(front.server.clone(), settings.clone()).expect("tls"));
    // Before a TLS service exists, challenge paths of unknown hosts are
    // refused rather than passed to an application.
    assert_eq!(
        get(
            front.addr,
            "other.example.com",
            "/.well-known/acme-challenge/x"
        )
        .await
        .0
        .as_u16(),
        403
    );
    let renewal = tokio::spawn({
        let (tls, stop) = (Arc::clone(&tls), stop.clone());
        async move { tls.run(stop).await }
    });
    let mut request = fast_request(&app.addr);
    request.tls = true;
    front.server.deploy(request).await.expect("deploy");
    let certificate = cache.join(TEST_HOST);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while !certificate.exists() {
        let failures = acme.state.lock().expect("lock").failures.clone();
        assert!(
            failures.is_empty(),
            "ACME server rejected the client: {failures:?}"
        );
        assert!(
            tokio::time::Instant::now() < deadline,
            "no certificate obtained"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let mode = |path: &Path| {
        std::fs::metadata(path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777
    };
    assert_eq!(mode(&certificate), 0o600);
    assert_eq!(mode(&cache.join("acme_account+key")), 0o600);
    assert_eq!(mode(&cache), 0o700);
    let addr = serve_tls(front.server.clone(), Arc::clone(&tls), front.stop.clone()).await;
    let config = client_config(dir.path());
    assert_eq!(
        https_get(Arc::clone(&config), addr, TEST_HOST).await,
        Ok((200, "app".to_owned()))
    );
    // Once validated, the challenge is withdrawn.
    assert_eq!(
        get(
            front.addr,
            TEST_HOST,
            "/.well-known/acme-challenge/tok-123_abc"
        )
        .await
        .0
        .as_u16(),
        404
    );
    assert_eq!(acme.state.lock().expect("lock").orders, 1);

    // A new proxy process on the same volume serves the cached certificate
    // without asking the CA again.
    stop.cancel();
    renewal.await.expect("renewal loop");
    let restarted = Arc::new(Tls::new(front.server.clone(), settings).expect("tls"));
    let again = CancellationToken::new();
    let loop_again = tokio::spawn({
        let (tls, again) = (Arc::clone(&restarted), again.clone());
        async move { tls.run(again).await }
    });
    let addr = serve_tls(
        front.server.clone(),
        Arc::clone(&restarted),
        front.stop.clone(),
    )
    .await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if https_get(Arc::clone(&config), addr, TEST_HOST).await == Ok((200, "app".to_owned())) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "cached certificate not served"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        acme.state.lock().expect("lock").orders,
        1,
        "the CA was asked again"
    );
    again.cancel();
    loop_again.await.expect("renewal loop");
}
