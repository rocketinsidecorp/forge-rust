//! Request handling: routing by `Host`, maintenance and HTTPS redirects,
//! request body limits, header rewriting and forwarding to the target, and
//! the JSON access log.
//!
//! forge-proxy is the internet edge. Inbound `Forwarded`, `X-Forwarded-*`
//! and `X-Real-IP` headers are dropped and `X-Forwarded-For` is set to the
//! connecting address alone, so a client cannot spoof its address or extend
//! the chain; applications trust it only from the forge network
//! (`FORGE_HTTP_TRUSTED_PROXIES`).
//!
//! Each request opens its own connection to the target and closes it when
//! the response ends: no connection pool outlives a switch, and nothing is
//! polled by a background task. The upstream connection is driven by the
//! request handler until the response headers arrive and then by the
//! response body the client reads.

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use http::header::{self, HeaderMap, HeaderName, HeaderValue};
use http::{Method, Request, Response, StatusCode, Version};
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt as _, Empty, Full};
use hyper::body::{Body as _, Frame, Incoming, SizeHint};
use hyper_util::rt::TokioIo;
use serde_json::json;
use tokio::sync::watch;
use tokio::time::{Instant, Sleep};

use crate::log::text;
use crate::server::{CONNECT_TIMEOUT, Lease, Server, Service};

/// Errors carried by bodies.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// The body of every response and forwarded request.
pub type ProxyBody = UnsyncBoxBody<Bytes, BoxError>;

const MAINTENANCE_PAGE: &str = "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\n<title>Down for maintenance</title>\n<style>body{font-family:system-ui,sans-serif;max-width:32rem;margin:15vh auto;padding:0 1rem;color:#222}</style>\n</head><body><h1>Down for maintenance</h1><p>We are performing scheduled maintenance and will be back shortly.</p></body></html>\n";

/// Hop-by-hop headers (RFC 9110 §7.6.1), never forwarded.
const HOP_BY_HOP: [&str; 9] = [
    "connection",
    "proxy-connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

fn boxed<B>(body: B) -> ProxyBody
where
    B: hyper::body::Body<Data = Bytes> + Send + 'static,
    B::Error: Into<BoxError>,
{
    body.map_err(Into::into).boxed_unsync()
}

fn empty() -> ProxyBody {
    boxed(Empty::<Bytes>::new())
}

/// Go's `http.Error`: plain text, `nosniff`, message and newline.
fn plain(status: StatusCode, message: &str) -> Response<ProxyBody> {
    let mut response = Response::new(boxed(Full::new(Bytes::from(format!("{message}\n")))));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response
}

fn maintenance() -> Response<ProxyBody> {
    let mut response = Response::new(boxed(Full::new(Bytes::from_static(
        MAINTENANCE_PAGE.as_bytes(),
    ))));
    *response.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(header::RETRY_AFTER, HeaderValue::from_static("60"));
    response
}

/// What the access log records about one request.
struct AccessRecord {
    server: Server,
    started: Instant,
    service: String,
    host: String,
    method: String,
    path: String,
    client_ip: String,
    proto: &'static str,
    tls: bool,
    status: u16,
    target: String,
    bytes: u64,
}

impl AccessRecord {
    fn emit(&self) {
        // The query string is not logged: it can carry tokens.
        #[allow(clippy::cast_precision_loss)]
        let duration_ms = self.started.elapsed().as_micros() as f64 / 1000.0;
        self.server.logger().info(
            "request",
            &[
                ("service", text(&self.service)),
                ("host", text(&self.host)),
                ("method", text(&self.method)),
                ("path", text(&self.path)),
                ("status", json!(self.status)),
                ("bytes", json!(self.bytes)),
                ("duration_ms", json!(duration_ms)),
                ("client_ip", text(&self.client_ip)),
                ("proto", text(self.proto)),
                ("tls", json!(self.tls)),
                ("target", text(&self.target)),
            ],
        );
    }
}

fn proto(version: Version) -> &'static str {
    match version {
        Version::HTTP_09 => "HTTP/0.9",
        Version::HTTP_10 => "HTTP/1.0",
        Version::HTTP_2 => "HTTP/2.0",
        Version::HTTP_3 => "HTTP/3.0",
        _ => "HTTP/1.1",
    }
}

/// The request's host: the `Host` header, else the absolute URI's.
fn request_host<B>(request: &Request<B>) -> String {
    request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
        .or_else(|| request.uri().authority().map(|a| a.as_str().to_owned()))
        .unwrap_or_default()
}

/// The status and body length of a finished early response, for the log.
fn early(record: &mut AccessRecord, response: Response<ProxyBody>) -> Response<ProxyBody> {
    record.status = response.status().as_u16();
    record.bytes = response.body().size_hint().exact().unwrap_or(0);
    record.emit();
    response
}

impl Server {
    /// Answers one request that arrived from `peer` (over TLS when `tls`).
    /// An upgraded connection it opens is closed after `idle_timeout`
    /// without data in either direction.
    pub async fn handle(
        &self,
        request: Request<Incoming>,
        peer: SocketAddr,
        tls: bool,
        idle_timeout: Duration,
    ) -> Response<ProxyBody> {
        let host = request_host(&request);
        if !tls
            && let Some(token) = crate::tls::challenge_token(request.uri().path())
            && let Some(answer) = self.challenge(token)
        {
            return self.answer_challenge(&host, answer, peer, &request);
        }
        let service = self.route(&host);
        let mut record = AccessRecord {
            server: self.clone(),
            started: Instant::now(),
            service: service.as_ref().map(|s| s.name.clone()).unwrap_or_default(),
            host: host.clone(),
            method: request.method().as_str().to_owned(),
            path: request.uri().path().to_owned(),
            client_ip: peer.ip().to_string(),
            proto: proto(request.version()),
            tls,
            status: 200,
            target: String::new(),
            bytes: 0,
        };
        let Some(service) = service else {
            return early(&mut record, plain(StatusCode::NOT_FOUND, "unknown host"));
        };
        if service.maintenance() {
            return early(&mut record, maintenance());
        }
        let settings = service.settings();
        if settings.tls && !tls {
            let path_and_query = request
                .uri()
                .path_and_query()
                .map_or("/", |value| value.as_str());
            let location = format!(
                "https://{}{}",
                crate::server::strip_port(&host),
                path_and_query
            );
            let mut response = plain(StatusCode::PERMANENT_REDIRECT, "Permanent Redirect");
            if let Ok(value) = HeaderValue::from_str(&location) {
                response.headers_mut().insert(header::LOCATION, value);
            }
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/html; charset=utf-8"),
            );
            return early(&mut record, response);
        }
        if request.method() == Method::CONNECT {
            // The proxy never opens tunnels to arbitrary destinations.
            return early(
                &mut record,
                plain(StatusCode::METHOD_NOT_ALLOWED, "method not allowed"),
            );
        }
        let max_body = u64::try_from(settings.max_body_bytes).unwrap_or(0);
        let buffer = u64::try_from(settings.buffer_bytes).unwrap_or(0);
        let content_length = declared_length(request.headers());
        if content_length.is_some_and(|length| length > max_body) {
            return early(
                &mut record,
                plain(StatusCode::PAYLOAD_TOO_LARGE, "request body too large"),
            );
        }
        let (mut parts, body) = request.into_parts();
        let flags = Arc::new(BodyFlags::default());
        let (sent_tx, sent_rx) = watch::channel(false);
        let outbound = if body.is_end_stream() {
            let _ = sent_tx.send(true);
            Outbound::Empty
        } else {
            match self
                .prepare_body(body, content_length, buffer, max_body, &flags, sent_tx)
                .await
            {
                Ok(outbound) => outbound,
                Err(response) => return early(&mut record, *response),
            }
        };
        if let Outbound::Buffered(bytes) = &outbound {
            // Forwarded with its exact length.
            parts.headers.remove(header::TRANSFER_ENCODING);
            parts
                .headers
                .insert(header::CONTENT_LENGTH, HeaderValue::from(bytes.len()));
        }
        let lease = match acquire(&service) {
            Ok(lease) => lease,
            Err(response) => return early(&mut record, *response),
        };
        record.target = lease.addr().to_owned();
        let downstream_upgrade = parts.extensions.remove::<hyper::upgrade::OnUpgrade>();
        let request = Request::from_parts(parts, outbound.into_body());
        self.forward(
            request,
            downstream_upgrade,
            peer,
            tls,
            &host,
            lease,
            settings.response_timeout.to_std(),
            idle_timeout,
            flags,
            sent_rx,
            record,
        )
        .await
    }

    /// Answers an ACME HTTP-01 challenge (only for TLS hosts), like
    /// autocert's HTTP handler: challenge paths never reach applications.
    fn answer_challenge(
        &self,
        host: &str,
        answer: Option<String>,
        peer: SocketAddr,
        request: &Request<Incoming>,
    ) -> Response<ProxyBody> {
        let name = crate::server::strip_port(host).to_ascii_lowercase();
        let response = if !self.tls_host(&name) {
            plain(
                StatusCode::FORBIDDEN,
                &format!(
                    "host {} is not configured for TLS",
                    crate::settings::go_quote(&name)
                ),
            )
        } else if let Some(answer) = answer {
            let mut response = Response::new(boxed(Full::new(Bytes::from(answer))));
            response
                .headers_mut()
                .insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
            response
        } else {
            plain(
                StatusCode::NOT_FOUND,
                "acme/autocert: certificate cache miss",
            )
        };
        let mut record = AccessRecord {
            server: self.clone(),
            started: Instant::now(),
            service: String::new(),
            host: host.to_owned(),
            method: request.method().as_str().to_owned(),
            path: request.uri().path().to_owned(),
            client_ip: peer.ip().to_string(),
            proto: proto(request.version()),
            tls: false,
            status: 200,
            target: String::new(),
            bytes: 0,
        };
        early(&mut record, response)
    }

    /// Protects the application from slow clients. Bodies of at most
    /// `buffer` bytes, with or without `Content-Length`, are read completely
    /// within the body timeout before the application is contacted (408
    /// otherwise). Larger bodies are streamed, and every read must receive
    /// data within the body timeout; a stalled upload gets 408.
    async fn prepare_body(
        &self,
        mut body: Incoming,
        content_length: Option<u64>,
        buffer: u64,
        max_body: u64,
        flags: &Arc<BodyFlags>,
        sent: watch::Sender<bool>,
    ) -> Result<Outbound, Box<Response<ProxyBody>>> {
        let timeout = self.body_timeout();
        if content_length.is_some_and(|length| length > buffer) {
            return Ok(Outbound::Streamed(StreamedBody::new(
                None,
                body,
                timeout,
                max_body,
                Arc::clone(flags),
                sent,
            )));
        }
        let mut collected = BytesMut::new();
        let deadline = Instant::now() + timeout;
        loop {
            let frame = match tokio::time::timeout_at(deadline, body.frame()).await {
                Err(_) => {
                    return Err(Box::new(plain(
                        StatusCode::REQUEST_TIMEOUT,
                        "request body not received in time",
                    )));
                }
                Ok(None) => break,
                Ok(Some(Err(_))) => {
                    return Err(Box::new(plain(
                        StatusCode::REQUEST_TIMEOUT,
                        "request body not received in time",
                    )));
                }
                Ok(Some(Ok(frame))) => frame,
            };
            if let Ok(data) = frame.into_data() {
                collected.extend_from_slice(&data);
                let seen = collected.len() as u64;
                if seen > max_body {
                    return Err(Box::new(plain(
                        StatusCode::PAYLOAD_TOO_LARGE,
                        "request body too large",
                    )));
                }
                if seen > buffer {
                    // Unknown length beyond the buffer: stream the rest.
                    return Ok(Outbound::Streamed(StreamedBody::new(
                        Some(collected.freeze()),
                        body,
                        timeout,
                        max_body,
                        Arc::clone(flags),
                        sent,
                    )));
                }
            }
        }
        let _ = sent.send(true);
        Ok(Outbound::Buffered(collected.freeze()))
    }

    #[allow(clippy::too_many_arguments)]
    async fn forward(
        &self,
        mut request: Request<ProxyBody>,
        downstream_upgrade: Option<hyper::upgrade::OnUpgrade>,
        peer: SocketAddr,
        tls: bool,
        host: &str,
        lease: Lease,
        response_timeout: Duration,
        idle_timeout: Duration,
        flags: Arc<BodyFlags>,
        mut sent: watch::Receiver<bool>,
        mut record: AccessRecord,
    ) -> Response<ProxyBody> {
        let upgrade = rewrite_request(&mut request, peer, tls, host);
        let failure = |record: &mut AccessRecord, flags: &BodyFlags, error: &str| {
            let response = if flags.too_large.load(Ordering::SeqCst) {
                plain(StatusCode::PAYLOAD_TOO_LARGE, "request body too large")
            } else if flags.expired.load(Ordering::SeqCst) {
                plain(
                    StatusCode::REQUEST_TIMEOUT,
                    "request body not received in time",
                )
            } else {
                self.logger().warn(
                    "upstream error",
                    &[("target", text(&record.target)), ("error", text(error))],
                );
                plain(StatusCode::BAD_GATEWAY, "bad gateway")
            };
            early(record, response)
        };
        let addr = lease.addr().to_owned();
        let stream = match tokio::time::timeout(
            CONNECT_TIMEOUT,
            tokio::net::TcpStream::connect(&addr),
        )
        .await
        {
            Ok(Ok(stream)) => stream,
            Ok(Err(error)) => {
                return failure(&mut record, &flags, &format!("dial tcp {addr}: {error}"));
            }
            Err(_) => {
                return failure(
                    &mut record,
                    &flags,
                    &format!("dial tcp {addr}: i/o timeout"),
                );
            }
        };
        let _ = stream.set_nodelay(true);
        let (mut sender, connection) =
            match hyper::client::conn::http1::handshake::<_, ProxyBody>(TokioIo::new(stream)).await
            {
                Ok(pair) => pair,
                Err(error) => return failure(&mut record, &flags, &error.to_string()),
            };
        let mut connection: Pin<Box<UpgradeableConnection>> = Box::pin(connection.with_upgrades());
        let response_future = sender.send_request(request);
        // Without a handle the connection closes once this exchange ends.
        drop(sender);
        tokio::pin!(response_future);
        // Response headers are due `response_timeout` after the request
        // body was sent.
        let deadline = async {
            let _ = sent.wait_for(|done| *done).await;
            tokio::time::sleep(response_timeout).await;
        };
        tokio::pin!(deadline);
        let mut connection_done = false;
        let response = loop {
            tokio::select! {
                result = &mut response_future => break result,
                _ = &mut connection, if !connection_done => connection_done = true,
                () = &mut deadline => {
                    return failure(&mut record, &flags, "timeout awaiting response headers");
                }
            }
        };
        let mut response = match response {
            Ok(response) => response,
            Err(error) => return failure(&mut record, &flags, &error.to_string()),
        };
        if response.status() == StatusCode::SWITCHING_PROTOCOLS {
            return self.tunnel(
                response,
                upgrade,
                downstream_upgrade,
                (!connection_done).then_some(connection),
                lease,
                idle_timeout,
                record,
            );
        }
        remove_hop_by_hop(response.headers_mut(), None);
        record.status = response.status().as_u16();
        let (parts, body) = response.into_parts();
        let body = UpstreamBody {
            body,
            connection: (!connection_done).then_some(connection),
            lease: Some(lease),
            record: Some(record),
        };
        Response::from_parts(parts, body.boxed_unsync())
    }

    /// Completes a protocol switch (WebSocket): the client gets the 101 and
    /// both connections are joined until either side closes, or no data
    /// moved in either direction for `idle_timeout`. The lease is held for
    /// the whole tunnel, so a drain waits for it.
    #[allow(clippy::too_many_arguments)]
    fn tunnel(
        &self,
        mut response: Response<Incoming>,
        requested: Option<String>,
        downstream: Option<hyper::upgrade::OnUpgrade>,
        connection: Option<Pin<Box<UpgradeableConnection>>>,
        lease: Lease,
        idle_timeout: Duration,
        mut record: AccessRecord,
    ) -> Response<ProxyBody> {
        let offered = upgrade_type(response.headers());
        let (Some(requested), Some(downstream)) = (requested, downstream) else {
            return early(&mut record, plain(StatusCode::BAD_GATEWAY, "bad gateway"));
        };
        if !offered
            .as_deref()
            .is_some_and(|offered| offered.eq_ignore_ascii_case(&requested))
        {
            self.logger().warn(
                "upstream error",
                &[
                    ("target", text(&record.target)),
                    (
                        "error",
                        text(format!(
                            "backend tried to switch protocol {:?} when {requested:?} was requested",
                            offered.unwrap_or_default()
                        )),
                    ),
                ],
            );
            return early(&mut record, plain(StatusCode::BAD_GATEWAY, "bad gateway"));
        }
        let Ok(permit) = Arc::clone(&self.inner.tunnel_permits).try_acquire_owned() else {
            return early(
                &mut record,
                plain(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "too many upgraded connections",
                ),
            );
        };
        let upstream = hyper::upgrade::on(&mut response);
        let (mut parts, _) = response.into_parts();
        remove_hop_by_hop(&mut parts.headers, Some(&requested));
        record.status = StatusCode::SWITCHING_PROTOCOLS.as_u16();
        let shutdown = self.inner.shutdown.clone();
        self.inner.tunnels.spawn(async move {
            let activity = Activity::new();
            // The connection hands its stream over to the upgrade; it may
            // already have done so.
            let upstream = async {
                match connection {
                    Some(connection) => tokio::join!(upstream, connection).0,
                    None => upstream.await,
                }
            };
            let joined = async {
                let (upstream, downstream) = tokio::join!(upstream, downstream);
                if let (Ok(upstream), Ok(downstream)) = (upstream, downstream) {
                    let mut upstream = TokioIo::new(upstream);
                    // Every byte of the tunnel crosses the client side.
                    let mut downstream = Watched {
                        inner: TokioIo::new(downstream),
                        activity: &activity,
                    };
                    let _ = tokio::io::copy_bidirectional(&mut downstream, &mut upstream).await;
                }
            };
            tokio::select! {
                () = joined => {}
                () = activity.idle_for(idle_timeout) => {}
                () = shutdown.cancelled() => {}
            }
            record.bytes = activity.to_client.load(Ordering::Relaxed);
            record.emit();
            drop(lease);
            drop(permit);
        });
        Response::from_parts(parts, empty())
    }
}

/// When a tunnel last carried data, and how much it sent to the client.
struct Activity {
    started: Instant,
    /// Milliseconds after `started`.
    last: AtomicU64,
    to_client: AtomicU64,
}

impl Activity {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            last: AtomicU64::new(0),
            to_client: AtomicU64::new(0),
        }
    }

    fn touch(&self) {
        let elapsed = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.last.fetch_max(elapsed, Ordering::Relaxed);
    }

    /// Resolves once no data moved for `idle`.
    async fn idle_for(&self, idle: Duration) {
        loop {
            let last = self.started + Duration::from_millis(self.last.load(Ordering::Relaxed));
            let deadline = last + idle;
            if deadline <= Instant::now() {
                return;
            }
            tokio::time::sleep_until(deadline).await;
        }
    }
}

/// The client side of a tunnel, recording its traffic in an [`Activity`].
struct Watched<'a, T> {
    inner: T,
    activity: &'a Activity,
}

impl<T: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for Watched<'_, T> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        let result = Pin::new(&mut this.inner).poll_read(cx, buf);
        if matches!(result, Poll::Ready(Ok(()))) && buf.filled().len() > before {
            this.activity.touch();
        }
        result
    }
}

impl<T: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for Watched<'_, T> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        let result = Pin::new(&mut this.inner).poll_write(cx, buf);
        if let Poll::Ready(Ok(written)) = result
            && written > 0
        {
            this.activity.touch();
            this.activity
                .to_client
                .fetch_add(written as u64, Ordering::Relaxed);
        }
        result
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

/// The upstream connection's driver (hyper's upgradeable connection
/// future, whose type is not nameable).
type UpgradeableConnection = dyn Future<Output = hyper::Result<()>> + Send;

/// A lease on the current target, retrying when it was retired after it
/// was loaded (the successor is already in place then).
fn acquire(service: &Service) -> Result<Lease, Box<Response<ProxyBody>>> {
    loop {
        let Some(target) = service.target() else {
            return Err(Box::new(plain(
                StatusCode::SERVICE_UNAVAILABLE,
                "service has no target",
            )));
        };
        if let Some(lease) = target.acquire() {
            return Ok(lease);
        }
        let current = service.target();
        if current.is_none_or(|current| Arc::ptr_eq(&current, &target)) {
            // Retired without a successor: the service was removed after
            // this request was routed.
            return Err(Box::new(plain(
                StatusCode::SERVICE_UNAVAILABLE,
                "service has no target",
            )));
        }
    }
}

fn declared_length(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse().ok())
}

/// The `Upgrade` token of a message whose `Connection` lists `upgrade`.
fn upgrade_type(headers: &HeaderMap) -> Option<String> {
    let connection_upgrade = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|token| token.trim().eq_ignore_ascii_case("upgrade"));
    if !connection_upgrade {
        return None;
    }
    headers
        .get(header::UPGRADE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

/// Removes hop-by-hop headers and those named by `Connection`; for a switch
/// to `upgrade`, keeps `Connection: Upgrade` and `Upgrade`.
fn remove_hop_by_hop(headers: &mut HeaderMap, upgrade: Option<&str>) {
    let listed: Vec<HeaderName> = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|token| HeaderName::from_bytes(token.trim().as_bytes()).ok())
        .collect();
    for name in listed {
        headers.remove(name);
    }
    for name in HOP_BY_HOP {
        headers.remove(name);
    }
    if let Some(upgrade) = upgrade
        && let Ok(value) = HeaderValue::from_str(upgrade)
    {
        headers.insert(header::CONNECTION, HeaderValue::from_static("Upgrade"));
        headers.insert(header::UPGRADE, value);
    }
}

/// Rewrites the inbound request for the target: origin-form URI, hop-by-hop
/// headers removed, forwarding headers replaced with the connection's own.
/// Returns the requested upgrade, if any.
fn rewrite_request<B>(
    request: &mut Request<B>,
    peer: SocketAddr,
    tls: bool,
    host: &str,
) -> Option<String> {
    let upgrade = upgrade_type(request.headers());
    let trailers = request
        .headers()
        .get_all(header::TE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|token| token.trim().eq_ignore_ascii_case("trailers"));
    let headers = request.headers_mut();
    remove_hop_by_hop(headers, upgrade.as_deref());
    if trailers {
        headers.insert(header::TE, HeaderValue::from_static("trailers"));
    }
    for name in [
        "forwarded",
        "x-forwarded-for",
        "x-forwarded-host",
        "x-forwarded-proto",
        "x-real-ip",
    ] {
        headers.remove(name);
    }
    if let Ok(value) = HeaderValue::from_str(&peer.ip().to_string()) {
        headers.insert("x-forwarded-for", value);
    }
    if let Ok(value) = HeaderValue::from_str(host) {
        headers.insert("x-forwarded-host", value.clone());
        // The original Host is kept.
        headers.insert(header::HOST, value);
    }
    headers.insert(
        "x-forwarded-proto",
        HeaderValue::from_static(if tls { "https" } else { "http" }),
    );
    let path_and_query = request
        .uri()
        .path_and_query()
        .map_or_else(|| "/".to_owned(), |value| value.as_str().to_owned());
    if let Ok(uri) = path_and_query.parse() {
        *request.uri_mut() = uri;
    }
    *request.version_mut() = Version::HTTP_11;
    upgrade
}

/// Why a forwarded body ended early.
#[derive(Debug, Default)]
struct BodyFlags {
    expired: AtomicBool,
    too_large: AtomicBool,
}

enum Outbound {
    Empty,
    Buffered(Bytes),
    Streamed(StreamedBody),
}

impl Outbound {
    fn into_body(self) -> ProxyBody {
        match self {
            Self::Empty => empty(),
            Self::Buffered(bytes) => boxed(Full::new(bytes)),
            Self::Streamed(body) => body.boxed_unsync(),
        }
    }
}

#[derive(Debug)]
struct BodyError(&'static str);

impl std::fmt::Display for BodyError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.0)
    }
}

impl std::error::Error for BodyError {}

/// A request body forwarded while it arrives: a buffered prefix, then reads
/// that must each receive data within the timeout, capped at the body
/// limit.
struct StreamedBody {
    prefix: Option<Bytes>,
    inner: Incoming,
    timeout: Duration,
    sleep: Pin<Box<Sleep>>,
    waiting: bool,
    limit: u64,
    seen: u64,
    flags: Arc<BodyFlags>,
    sent: Option<watch::Sender<bool>>,
}

impl StreamedBody {
    fn new(
        prefix: Option<Bytes>,
        inner: Incoming,
        timeout: Duration,
        limit: u64,
        flags: Arc<BodyFlags>,
        sent: watch::Sender<bool>,
    ) -> Self {
        let seen = prefix.as_ref().map_or(0, |bytes| bytes.len() as u64);
        Self {
            prefix,
            inner,
            timeout,
            sleep: Box::pin(tokio::time::sleep(timeout)),
            waiting: false,
            limit,
            seen,
            flags,
            sent: Some(sent),
        }
    }

    fn finished(&mut self) {
        if let Some(sent) = self.sent.take() {
            let _ = sent.send(true);
        }
    }
}

impl hyper::body::Body for StreamedBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        let this = self.get_mut();
        if let Some(prefix) = this.prefix.take() {
            return Poll::Ready(Some(Ok(Frame::data(prefix))));
        }
        if !this.waiting {
            this.waiting = true;
            this.sleep.as_mut().reset(Instant::now() + this.timeout);
        }
        match Pin::new(&mut this.inner).poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                this.waiting = false;
                if let Some(data) = frame.data_ref() {
                    this.seen += data.len() as u64;
                    if this.seen > this.limit {
                        this.flags.too_large.store(true, Ordering::SeqCst);
                        this.finished();
                        return Poll::Ready(Some(Err(Box::new(BodyError(
                            "request body too large",
                        )))));
                    }
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(error))) => {
                this.finished();
                Poll::Ready(Some(Err(Box::new(error))))
            }
            Poll::Ready(None) => {
                this.finished();
                Poll::Ready(None)
            }
            Poll::Pending => {
                if this.sleep.as_mut().poll(cx).is_ready() {
                    this.flags.expired.store(true, Ordering::SeqCst);
                    this.finished();
                    return Poll::Ready(Some(Err(Box::new(BodyError(
                        "request body not received in time",
                    )))));
                }
                Poll::Pending
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.prefix.is_none() && self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        let mut hint = self.inner.size_hint();
        if let Some(prefix) = &self.prefix {
            let extra = prefix.len() as u64;
            hint.set_lower(hint.lower() + extra);
            if let Some(upper) = hint.upper() {
                hint.set_upper(upper + extra);
            }
        }
        hint
    }
}

impl Drop for StreamedBody {
    fn drop(&mut self) {
        self.finished();
    }
}

/// The target's response body, which also drives the upstream connection,
/// holds the lease and writes the access log when it ends.
struct UpstreamBody {
    body: Incoming,
    connection: Option<Pin<Box<UpgradeableConnection>>>,
    lease: Option<Lease>,
    record: Option<AccessRecord>,
}

impl hyper::body::Body for UpstreamBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        let this = self.get_mut();
        if let Some(connection) = this.connection.as_mut()
            && connection.as_mut().poll(cx).is_ready()
        {
            this.connection = None;
        }
        match Pin::new(&mut this.body).poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                if let (Some(record), Some(data)) = (this.record.as_mut(), frame.data_ref()) {
                    record.bytes += data.len() as u64;
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(error))) => Poll::Ready(Some(Err(Box::new(error)))),
            Poll::Ready(None) => {
                this.finish();
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.body.size_hint()
    }
}

impl UpstreamBody {
    fn finish(&mut self) {
        self.lease.take();
        if let Some(record) = self.record.take() {
            record.emit();
        }
    }
}

impl Drop for UpstreamBody {
    fn drop(&mut self) {
        self.finish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hop_by_hop_headers_are_removed() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONNECTION,
            HeaderValue::from_static("keep-alive, x-secret"),
        );
        headers.insert("x-secret", HeaderValue::from_static("1"));
        headers.insert(
            header::TRANSFER_ENCODING,
            HeaderValue::from_static("chunked"),
        );
        headers.insert("x-kept", HeaderValue::from_static("1"));
        remove_hop_by_hop(&mut headers, None);
        assert!(headers.get("x-secret").is_none());
        assert!(headers.get(header::CONNECTION).is_none());
        assert!(headers.get(header::TRANSFER_ENCODING).is_none());
        assert!(headers.get("x-kept").is_some());
    }

    #[test]
    fn forwarding_headers_are_replaced() {
        let mut request = Request::get("http://ignored/a/b?c=d")
            .header(header::HOST, "app.example.com")
            .header("x-forwarded-for", "203.0.113.66")
            .header("x-real-ip", "203.0.113.66")
            .header("forwarded", "for=203.0.113.66")
            .header("x-forwarded-proto", "https")
            .body(())
            .expect("valid request");
        let peer: SocketAddr = "198.51.100.7:4321".parse().expect("address");
        let upgrade = rewrite_request(&mut request, peer, false, "app.example.com");
        assert_eq!(upgrade, None);
        let headers = request.headers();
        assert_eq!(headers["x-forwarded-for"], "198.51.100.7");
        assert_eq!(headers["x-forwarded-proto"], "http");
        assert_eq!(headers["x-forwarded-host"], "app.example.com");
        assert_eq!(headers[header::HOST], "app.example.com");
        assert!(headers.get("forwarded").is_none());
        assert!(headers.get("x-real-ip").is_none());
        assert_eq!(request.uri(), "/a/b?c=d");
    }
}
