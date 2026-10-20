//! OTLP/HTTP export with the JSON encoding over Forge's own hyper client.
//!
//! One connection per export attempt (HTTP/1.1, TLS through rustls with the
//! webpki roots when the endpoint is `https`), bounded retries with backoff
//! and jitter on network errors and HTTP 429/502/503/504, `Retry-After`
//! honored, no redirects followed, optional gzip, header values marked
//! sensitive. Errors carry the signal path and the HTTP status or the
//! transport cause, never the endpoint, the headers or a response body.

pub(crate) mod encode;

use std::{
    fmt,
    io::Write as _,
    sync::Arc,
    time::{Duration, SystemTime},
};

use bytes::Bytes;
use forge_config::{EndpointScheme, OtlpCompression, TelemetryConfig, endpoint_scheme};
use http::{HeaderMap, HeaderName, HeaderValue, Method, Request, StatusCode, header};
use http_body_util::{BodyExt, Full, Limited};
use hyper_util::rt::TokioIo;
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpStream,
    time::Instant,
};

/// Attempts per export, the first included.
pub const MAX_ATTEMPTS: u32 = 5;
const INITIAL_BACKOFF: Duration = Duration::from_millis(500);
const MAX_BACKOFF: Duration = Duration::from_secs(5);
const MAX_RETRY_AFTER: Duration = Duration::from_secs(30);
const MAX_RESPONSE_BYTES: usize = 64 << 10;

/// The OTLP signal paths.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Signal {
    /// `/v1/traces`.
    Traces,
    /// `/v1/metrics`.
    Metrics,
    /// `/v1/logs`.
    Logs,
}

impl Signal {
    /// The path under the endpoint.
    #[must_use]
    pub const fn path(self) -> &'static str {
        match self {
            Self::Traces => "/v1/traces",
            Self::Metrics => "/v1/metrics",
            Self::Logs => "/v1/logs",
        }
    }

    /// The `signal` label of `forge.telemetry.export.errors`.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Traces => "traces",
            Self::Metrics => "metrics",
            Self::Logs => "logs",
        }
    }
}

/// A failed export. It never contains the endpoint, headers or a body.
#[derive(Debug, Error)]
pub enum ExportError {
    /// The collector answered with a non-2xx status (after retries for the
    /// retryable ones).
    #[error("OTLP export to {path} failed with HTTP {status}")]
    Status {
        /// Signal path.
        path: &'static str,
        /// Final status code.
        status: u16,
    },
    /// The connection, TLS or HTTP exchange failed.
    #[error("OTLP export to {path} failed: {cause}")]
    Transport {
        /// Signal path.
        path: &'static str,
        /// Transport cause, without the URL.
        cause: String,
    },
    /// `FORGE_TELEMETRY_OTLP_TIMEOUT` elapsed.
    #[error("OTLP export to {path} timed out")]
    Timeout {
        /// Signal path.
        path: &'static str,
    },
}

impl ExportError {
    /// A low-cardinality code for logs and span status.
    #[must_use]
    pub fn error_type(&self) -> String {
        match self {
            Self::Status { status, .. } => status.to_string(),
            Self::Transport { .. } => "transport".to_owned(),
            Self::Timeout { .. } => "timeout".to_owned(),
        }
    }
}

/// The OTLP endpoint could not be used.
#[derive(Debug, Error)]
pub enum OtlpConfigError {
    /// Not an `http(s)://host[:port][/path]` URL.
    #[error(
        "telemetry: the OTLP endpoint must be an http:// or https:// URL without credentials, query or fragment"
    )]
    Endpoint,
    /// A header name or value is not valid HTTP.
    #[error("telemetry: an OTLP header is not a valid HTTP header")]
    Header,
    /// The TLS configuration could not be built.
    #[error("telemetry: TLS setup for the OTLP exporter failed")]
    Tls,
}

/// The OTLP/HTTP JSON exporter shared by traces, metrics and logs.
pub struct OtlpClient {
    https: bool,
    host: String,
    port: u16,
    authority: HeaderValue,
    path_prefix: String,
    headers: HeaderMap,
    gzip: bool,
    timeout: Duration,
    tls: Option<tokio_rustls::TlsConnector>,
    initial_backoff: Duration,
}

impl fmt::Debug for OtlpClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OtlpClient")
            .field("https", &self.https)
            .field("headers", &self.headers.len())
            .field("gzip", &self.gzip)
            .finish_non_exhaustive()
    }
}

impl OtlpClient {
    /// An exporter for the validated `FORGE_TELEMETRY_OTLP_*` settings. It
    /// does not connect until the first export.
    pub fn from_config(config: &TelemetryConfig) -> Result<Self, OtlpConfigError> {
        let endpoint = config
            .otlp_endpoint
            .as_deref()
            .ok_or(OtlpConfigError::Endpoint)?;
        let mut headers = HeaderMap::new();
        for entry in &config.otlp_headers {
            let name = HeaderName::from_bytes(entry.name.as_bytes())
                .map_err(|_| OtlpConfigError::Header)?;
            let mut value =
                HeaderValue::from_str(entry.value.expose()).map_err(|_| OtlpConfigError::Header)?;
            value.set_sensitive(true);
            headers.insert(name, value);
        }
        Self::new(
            endpoint,
            headers,
            config.otlp_compression == OtlpCompression::Gzip,
            config.otlp_timeout,
        )
    }

    /// An exporter for `endpoint` (the collector base URL).
    pub fn new(
        endpoint: &str,
        headers: HeaderMap,
        gzip: bool,
        timeout: Duration,
    ) -> Result<Self, OtlpConfigError> {
        let scheme = endpoint_scheme(endpoint).ok_or(OtlpConfigError::Endpoint)?;
        let https = scheme == EndpointScheme::Https;
        let rest = endpoint
            .split_once("://")
            .map(|(_, rest)| rest)
            .ok_or(OtlpConfigError::Endpoint)?;
        let (authority, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) if !port.contains(']') => (
                host,
                port.parse::<u16>().map_err(|_| OtlpConfigError::Endpoint)?,
            ),
            _ => (authority, if https { 443 } else { 80 }),
        };
        let host = host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_owned();
        let tls = if https {
            let mut roots = rustls::RootCertStore::empty();
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            let config = rustls::ClientConfig::builder_with_provider(Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .map_err(|_| OtlpConfigError::Tls)?
            .with_root_certificates(roots)
            .with_no_client_auth();
            Some(tokio_rustls::TlsConnector::from(Arc::new(config)))
        } else {
            None
        };
        Ok(Self {
            https,
            authority: HeaderValue::from_str(authority).map_err(|_| OtlpConfigError::Endpoint)?,
            host,
            port,
            path_prefix: path.trim_end_matches('/').to_owned(),
            headers,
            gzip,
            timeout,
            tls,
            initial_backoff: INITIAL_BACKOFF,
        })
    }

    /// Shortens the retry backoff; tests only.
    #[doc(hidden)]
    #[must_use]
    pub fn with_initial_backoff(mut self, backoff: Duration) -> Self {
        self.initial_backoff = backoff;
        self
    }

    /// The export deadline (`FORGE_TELEMETRY_OTLP_TIMEOUT`).
    #[must_use]
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Posts an encoded request body to `signal`, retrying within the
    /// export timeout.
    pub async fn export(&self, signal: Signal, body: Vec<u8>) -> Result<(), ExportError> {
        let path = signal.path();
        let deadline = Instant::now() + self.timeout;
        let (body, encoding) = if self.gzip {
            let mut encoder = flate2::write::GzEncoder::new(
                Vec::with_capacity(body.len() / 4),
                flate2::Compression::default(),
            );
            let compressed = encoder
                .write_all(&body)
                .and_then(|()| encoder.finish())
                .map_err(|error| ExportError::Transport {
                    path,
                    cause: format!("gzip: {error}"),
                })?;
            (Bytes::from(compressed), Some("gzip"))
        } else {
            (Bytes::from(body), None)
        };
        let mut backoff = self.initial_backoff;
        let mut attempt = 1;
        loop {
            let outcome =
                tokio::time::timeout_at(deadline, self.send(path, body.clone(), encoding)).await;
            let (failure, retry_after) = match outcome {
                Err(_elapsed) => return Err(ExportError::Timeout { path }),
                Ok(Ok((status, _))) if status.is_success() => return Ok(()),
                Ok(Ok((status, retry_after))) => {
                    let failure = ExportError::Status {
                        path,
                        status: status.as_u16(),
                    };
                    if !retryable(status) {
                        return Err(failure);
                    }
                    (failure, retry_after)
                }
                Ok(Err(cause)) => (ExportError::Transport { path, cause }, None),
            };
            if attempt >= MAX_ATTEMPTS {
                return Err(failure);
            }
            let wait = retry_after.unwrap_or_else(|| jitter(backoff));
            if Instant::now() + wait > deadline {
                return Err(failure);
            }
            tokio::time::sleep(wait).await;
            backoff = (backoff * 2).min(MAX_BACKOFF);
            attempt += 1;
        }
    }

    /// One attempt: connect, post, read at most 64 KiB of the response.
    async fn send(
        &self,
        path: &'static str,
        body: Bytes,
        encoding: Option<&'static str>,
    ) -> Result<(StatusCode, Option<Duration>), String> {
        let tcp = TcpStream::connect((self.host.as_str(), self.port))
            .await
            .map_err(|error| format!("connect: {}", error.kind()))?;
        let _ = tcp.set_nodelay(true);
        match &self.tls {
            Some(tls) => {
                let name = rustls::pki_types::ServerName::try_from(self.host.clone())
                    .map_err(|_| "invalid TLS server name".to_owned())?;
                let stream = tls
                    .connect(name, tcp)
                    .await
                    .map_err(|error| format!("tls: {error}"))?;
                self.exchange(stream, path, body, encoding).await
            }
            None => self.exchange(tcp, path, body, encoding).await,
        }
    }

    async fn exchange<IO>(
        &self,
        io: IO,
        path: &'static str,
        body: Bytes,
        encoding: Option<&'static str>,
    ) -> Result<(StatusCode, Option<Duration>), String>
    where
        IO: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(io))
            .await
            .map_err(|error| format!("http: {error}"))?;
        let mut request = Request::builder()
            .method(Method::POST)
            .uri(format!("{}{path}", self.path_prefix))
            .body(Full::new(body))
            .map_err(|_| "invalid request".to_owned())?;
        let headers = request.headers_mut();
        for (name, value) in &self.headers {
            headers.insert(name, value.clone());
        }
        headers.insert(header::HOST, self.authority.clone());
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        if let Some(encoding) = encoding {
            headers.insert(header::CONTENT_ENCODING, HeaderValue::from_static(encoding));
        }
        let exchange = async {
            let response = sender
                .send_request(request)
                .await
                .map_err(|error| format!("http: {error}"))?;
            let status = response.status();
            let retry_after = response
                .headers()
                .get(header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(parse_retry_after);
            // The body may echo request data; it is read (bounded) and dropped.
            let _ = Limited::new(response.into_body(), MAX_RESPONSE_BYTES)
                .collect()
                .await;
            Ok((status, retry_after))
        };
        // The connection is driven here, not on a detached task, and ends
        // with this attempt.
        tokio::select! {
            result = exchange => result,
            closed = connection => match closed {
                Ok(()) => Err("http: connection closed".to_owned()),
                Err(error) => Err(format!("http: {error}")),
            },
        }
    }
}

fn retryable(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::TOO_MANY_REQUESTS
            | StatusCode::BAD_GATEWAY
            | StatusCode::SERVICE_UNAVAILABLE
            | StatusCode::GATEWAY_TIMEOUT
    )
}

/// `Retry-After` as seconds or an HTTP date, at most 30 seconds.
fn parse_retry_after(value: &str) -> Option<Duration> {
    let value = value.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return (seconds > 0).then(|| Duration::from_secs(seconds).min(MAX_RETRY_AFTER));
    }
    let when = httpdate::parse_http_date(value).ok()?;
    when.duration_since(SystemTime::now())
        .ok()
        .filter(|wait| !wait.is_zero())
        .map(|wait| wait.min(MAX_RETRY_AFTER))
}

/// A random duration in `[d/2, d]`.
fn jitter(duration: Duration) -> Duration {
    let half = duration / 2;
    let mut bytes = [0_u8; 8];
    crate::context::random_bytes(&mut bytes);
    let span = u64::try_from(half.as_nanos())
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    half + Duration::from_nanos(u64::from_be_bytes(bytes) % span)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_are_split() {
        let client = OtlpClient::new(
            "https://otlp.example.com:4443/prefix/",
            HeaderMap::new(),
            false,
            Duration::from_secs(1),
        )
        .expect("valid");
        assert!(client.https && client.tls.is_some());
        assert_eq!(client.host, "otlp.example.com");
        assert_eq!(client.port, 4443);
        assert_eq!(client.path_prefix, "/prefix");
        let plain = OtlpClient::new(
            "http://[::1]",
            HeaderMap::new(),
            false,
            Duration::from_secs(1),
        )
        .expect("valid");
        assert_eq!((plain.host.as_str(), plain.port), ("::1", 80));
        assert_eq!(plain.authority, "[::1]");
        assert!(
            OtlpClient::new(
                "http://u:p@h:1",
                HeaderMap::new(),
                false,
                Duration::from_secs(1)
            )
            .is_err()
        );
    }

    #[test]
    fn retry_after_is_bounded() {
        assert_eq!(parse_retry_after("3"), Some(Duration::from_secs(3)));
        assert_eq!(parse_retry_after("3600"), Some(MAX_RETRY_AFTER));
        assert_eq!(parse_retry_after("0"), None);
        assert_eq!(parse_retry_after("soon"), None);
        let future = httpdate::fmt_http_date(SystemTime::now() + Duration::from_secs(10));
        let parsed = parse_retry_after(&future).expect("date");
        assert!(parsed <= Duration::from_secs(10));
    }

    #[test]
    fn jitter_stays_in_range() {
        for _ in 0..100 {
            let value = jitter(Duration::from_millis(500));
            assert!(value >= Duration::from_millis(250) && value <= Duration::from_millis(500));
        }
    }
}
