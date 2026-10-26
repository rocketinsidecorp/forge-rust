//! Accepting client connections: plain HTTP and, when configured, TLS.
//! Connections are bounded, owned by the accept loop and shut down
//! gracefully within a budget. A client that stops reading is disconnected
//! once a write to it makes no progress for the idle timeout; a keep-alive
//! connection may wait the idle timeout for its next request, which then
//! has the header timeout to arrive (Go's `IdleTimeout` and
//! `ReadHeaderTimeout`).

use std::convert::Infallible;
use std::future::Future as _;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use http::StatusCode;
use http_body_util::BodyExt as _;
use hyper::body::{Body, Frame, SizeHint};
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio::time::{Instant, Sleep};
use tokio_util::sync::CancellationToken;

use crate::forward::{BoxError, ProxyBody};
use crate::log::text;
use crate::server::Server;

/// Default of `-max-header-bytes`: 64 KiB of request line and headers.
pub const DEFAULT_MAX_HEADER_BYTES: usize = 64 << 10;
/// Least `-max-header-bytes`: below 4 KiB ordinary cookies would be refused.
pub const MIN_MAX_HEADER_BYTES: usize = 4 << 10;
/// Most `-max-header-bytes`: above 1 MiB (hyper's former buffer size here)
/// the limit protects nothing.
pub const MAX_MAX_HEADER_BYTES: usize = 1 << 20;
/// Default of `-max-connections`, per listener.
pub const DEFAULT_MAX_CONNECTIONS: usize = 10_000;

/// Limits of client connections: time, header size and how many at once.
#[derive(Clone, Copy, Debug)]
pub struct HttpLimits {
    /// Time allowed to read request headers.
    pub read_header_timeout: Duration,
    /// How long a keep-alive connection may wait for the first byte of its
    /// next request (`read_header_timeout` then applies to the rest of the
    /// headers), how long a write to the client may make no progress, and
    /// how long an upgraded (WebSocket) connection may carry no data in
    /// either direction.
    pub idle_timeout: Duration,
    /// Graceful shutdown budget.
    pub shutdown_timeout: Duration,
    /// The largest request line and headers a client may send
    /// (`-max-header-bytes`, [`MIN_MAX_HEADER_BYTES`] to
    /// [`MAX_MAX_HEADER_BYTES`]); more is answered with `431` and the
    /// connection is closed.
    pub max_header_bytes: usize,
    /// The most client connections open at once on a listener
    /// (`-max-connections`, at least 1); further clients wait in the kernel's
    /// listen backlog until one closes.
    pub max_connections: usize,
}

impl HttpLimits {
    /// The permits of a listener: `max_connections`, never more than a
    /// semaphore can hold (which no machine reaches, but a flag could ask
    /// for).
    fn connection_permits(&self) -> usize {
        self.max_connections.clamp(1, Semaphore::MAX_PERMITS)
    }
}

/// Where client connections come from: a listener, or a double in tests.
pub(crate) trait Incoming: Send + Sync + 'static {
    /// The next connection, or why accepting failed.
    fn accept(
        &self,
    ) -> impl std::future::Future<Output = std::io::Result<(tokio::net::TcpStream, SocketAddr)>> + Send;
}

impl Incoming for TcpListener {
    async fn accept(&self) -> std::io::Result<(tokio::net::TcpStream, SocketAddr)> {
        TcpListener::accept(self).await
    }
}

/// Turns an accepted TCP stream into the stream requests are read from.
pub(crate) trait Accept: Send + Sync + 'static {
    /// The stream type.
    type Stream: AsyncRead + AsyncWrite + Unpin + Send + 'static;
    /// Completes the handshake, if any.
    fn accept(
        &self,
        stream: tokio::net::TcpStream,
    ) -> impl std::future::Future<Output = Option<Self::Stream>> + Send;
    /// Whether requests on these streams arrived over TLS.
    fn tls(&self) -> bool;
}

/// Plain HTTP.
pub(crate) struct Plain;

impl Accept for Plain {
    type Stream = tokio::net::TcpStream;
    async fn accept(&self, stream: tokio::net::TcpStream) -> Option<Self::Stream> {
        Some(stream)
    }
    fn tls(&self) -> bool {
        false
    }
}

/// Serves `listener` until `shutdown` is cancelled, then asks open
/// connections to finish their in-flight request and aborts whatever is
/// left after the shutdown budget.
pub(crate) async fn serve<A: Accept>(
    server: Server,
    listener: TcpListener,
    acceptor: Arc<A>,
    limits: HttpLimits,
    shutdown: CancellationToken,
) {
    let permits = Arc::new(Semaphore::new(limits.connection_permits()));
    serve_with(server, listener, acceptor, limits, shutdown, permits).await;
}

/// [`serve`] with the connection permits and the source of connections given.
///
/// One permit is taken before each accept and belongs to the connection that
/// accept returns; a failed accept keeps it for the retry, so errors never
/// cost a permit. Every permit is back once this returns.
async fn serve_with<A: Accept, I: Incoming>(
    server: Server,
    listener: I,
    acceptor: Arc<A>,
    limits: HttpLimits,
    shutdown: CancellationToken,
    permits: Arc<Semaphore>,
) {
    let mut tasks = JoinSet::new();
    let connections = CancellationToken::new();
    'accept: loop {
        // Admission happens before accept: while every permit is held,
        // clients wait in the listen backlog.
        let permit = tokio::select! {
            biased;
            () = shutdown.cancelled() => break 'accept,
            Some(_) = tasks.join_next(), if !tasks.is_empty() => continue 'accept,
            permit = Arc::clone(&permits).acquire_owned() => match permit {
                Ok(permit) => permit,
                Err(_) => break 'accept,
            },
        };
        loop {
            tokio::select! {
                biased;
                () = shutdown.cancelled() => break 'accept,
                Some(_) = tasks.join_next(), if !tasks.is_empty() => {}
                accepted = listener.accept() => match accepted {
                    Ok((stream, peer)) => {
                        let server = server.clone();
                        let acceptor = Arc::clone(&acceptor);
                        let stop = connections.child_token();
                        tasks.spawn(async move {
                            let _ = stream.set_nodelay(true);
                            // The handshake shares the header budget.
                            let handshake = tokio::time::timeout(
                                limits.read_header_timeout,
                                acceptor.accept(stream),
                            );
                            let stream = tokio::select! {
                                stream = handshake => stream.ok().flatten(),
                                () = stop.cancelled() => None,
                            };
                            if let Some(stream) = stream {
                                serve_connection(server, stream, peer, acceptor.tls(), limits, stop).await;
                            }
                            drop(permit);
                        });
                        continue 'accept;
                    }
                    Err(error) => {
                        server
                            .logger()
                            .warn("accept failed", &[("error", text(error.to_string()))]);
                        tokio::select! {
                            () = shutdown.cancelled() => break 'accept,
                            () = tokio::time::sleep(Duration::from_millis(50)) => {}
                        }
                    }
                },
            }
        }
    }
    drop(listener);
    connections.cancel();
    let budget = tokio::time::sleep(limits.shutdown_timeout);
    tokio::pin!(budget);
    while !tasks.is_empty() {
        tokio::select! {
            () = &mut budget => {
                server.logger().warn(
                    "shutdown budget elapsed; closing open connections",
                    &[("remaining", serde_json::json!(tasks.len()))],
                );
                tasks.abort_all();
                break;
            }
            Some(_) = tasks.join_next() => {}
        }
    }
    while tasks.join_next().await.is_some() {}
}

async fn serve_connection<S>(
    server: Server,
    stream: S,
    peer: SocketAddr,
    tls: bool,
    limits: HttpLimits,
    stop: CancellationToken,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let idle = limits.idle_timeout;
    let phase = Arc::new(AtomicU8::new(HEAD));
    let stream = KeepAlive::new(stream, Arc::clone(&phase), limits.read_header_timeout, idle);
    let service = service_fn(move |request| {
        let server = server.clone();
        let phase = Arc::clone(&phase);
        async move {
            phase.store(BUSY, Ordering::SeqCst);
            let response = server.handle(request, peer, tls, idle).await;
            // An upgraded connection stays busy: the tunnel has its own
            // idle limit. Otherwise the connection is idle once the body
            // has been sent.
            let response = if response.status() == StatusCode::SWITCHING_PROTOCOLS {
                response
            } else {
                response.map(|body| IdleWhenDone { body, phase }.boxed_unsync())
            };
            Ok::<_, Infallible>(response)
        }
    });
    // The header and idle limits are KeepAlive's: hyper's own header timer
    // would also run while the connection waits for its next request.
    let connection = hyper::server::conn::http1::Builder::new()
        .timer(TokioTimer::new())
        .header_read_timeout(None)
        .max_buf_size(1 << 20)
        // Larger heads are answered with 431: a limit hyper's buffer size
        // alone cannot express below its 8 KiB minimum.
        .max_header_size(limits.max_header_bytes)
        .serve_connection(TokioIo::new(Progress::new(stream, idle)), service)
        .with_upgrades();
    tokio::pin!(connection);
    tokio::select! {
        _ = connection.as_mut() => {}
        () = stop.cancelled() => {
            connection.as_mut().graceful_shutdown();
            let _ = connection.await;
        }
    }
}

#[cfg(test)]
mod keep_alive_tests;
#[cfg(test)]
mod limit_tests;

/// Reading a request head ([`KeepAlive`]).
const HEAD: u8 = 0;
/// Serving a request: no read limit here (the body timeouts apply).
const BUSY: u8 = 1;
/// The response was sent; waiting for the next request.
const IDLE: u8 = 2;

/// A response body that marks the connection idle once it is dropped:
/// hyper drops it when it has been sent (or the exchange failed).
struct IdleWhenDone {
    body: ProxyBody,
    phase: Arc<AtomicU8>,
}

impl Drop for IdleWhenDone {
    fn drop(&mut self) {
        self.phase.store(IDLE, Ordering::SeqCst);
    }
}

impl Body for IdleWhenDone {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        Pin::new(&mut self.get_mut().body).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.body.size_hint()
    }
}

/// A client stream whose reads are limited like Go's `http.Server`: a
/// request head must arrive within `header` (the first one from the start
/// of the connection, later ones from their first byte), and between
/// requests the connection may wait `idle` for that first byte, after which
/// it reads as closed and hyper ends it quietly. While a request is served
/// (and on an upgraded connection) reads are not limited here.
pub(crate) struct KeepAlive<S> {
    inner: S,
    phase: Arc<AtomicU8>,
    header: Duration,
    idle: Duration,
    timer: Pin<Box<Sleep>>,
    /// The phase the timer is armed for, or `BUSY` when it is not armed.
    armed: u8,
    /// The idle wait ran out: every further read is end of stream.
    expired: bool,
}

impl<S> KeepAlive<S> {
    pub(crate) fn new(inner: S, phase: Arc<AtomicU8>, header: Duration, idle: Duration) -> Self {
        Self {
            inner,
            phase,
            header,
            idle,
            timer: Box::pin(tokio::time::sleep(header)),
            armed: BUSY,
            expired: false,
        }
    }

    fn arm(&mut self, phase: u8, limit: Duration) {
        if self.armed != phase {
            self.armed = phase;
            self.timer.as_mut().reset(Instant::now() + limit);
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for KeepAlive<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if this.expired {
            return Poll::Ready(Ok(()));
        }
        match this.phase.load(Ordering::SeqCst) {
            IDLE => {
                this.arm(IDLE, this.idle);
                let before = buf.filled().len();
                match Pin::new(&mut this.inner).poll_read(cx, buf) {
                    Poll::Ready(Ok(())) if buf.filled().len() > before => {
                        // The next request started: its head gets `header`.
                        this.phase.store(HEAD, Ordering::SeqCst);
                        this.arm(HEAD, this.header);
                        Poll::Ready(Ok(()))
                    }
                    Poll::Ready(result) => Poll::Ready(result),
                    Poll::Pending => {
                        if this.timer.as_mut().poll(cx).is_ready() {
                            // Read as a client that went away: hyper closes
                            // the idle connection without an error.
                            this.expired = true;
                            return Poll::Ready(Ok(()));
                        }
                        Poll::Pending
                    }
                }
            }
            HEAD => {
                this.arm(HEAD, this.header);
                match Pin::new(&mut this.inner).poll_read(cx, buf) {
                    Poll::Ready(result) => Poll::Ready(result),
                    Poll::Pending => {
                        if this.timer.as_mut().poll(cx).is_ready() {
                            return Poll::Ready(Err(std::io::Error::new(
                                std::io::ErrorKind::TimedOut,
                                "request headers not received in time",
                            )));
                        }
                        Poll::Pending
                    }
                }
            }
            _ => {
                this.armed = BUSY;
                Pin::new(&mut this.inner).poll_read(cx, buf)
            }
        }
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for KeepAlive<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

/// A client stream whose writes must make progress: a write, flush or
/// shutdown that stays pending for `limit` fails with `TimedOut`, which ends
/// the connection and releases what it held (the connection permit, the
/// target lease, the application connection). Reads are left to hyper's
/// header timer and the body timeout. Upgraded connections keep this
/// wrapper, so a tunnel to a client that stopped reading ends too.
pub(crate) struct Progress<S> {
    inner: S,
    limit: Duration,
    timer: Pin<Box<Sleep>>,
    armed: bool,
}

impl<S> Progress<S> {
    pub(crate) fn new(inner: S, limit: Duration) -> Self {
        Self {
            inner,
            limit,
            timer: Box::pin(tokio::time::sleep(limit)),
            armed: false,
        }
    }

    fn watch<T>(
        &mut self,
        cx: &mut Context<'_>,
        result: Poll<std::io::Result<T>>,
    ) -> Poll<std::io::Result<T>> {
        if result.is_ready() {
            self.armed = false;
            return result;
        }
        if !self.armed {
            self.armed = true;
            self.timer.as_mut().reset(Instant::now() + self.limit);
        }
        if self.timer.as_mut().poll(cx).is_ready() {
            self.armed = false;
            return Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "client stopped reading",
            )));
        }
        Poll::Pending
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for Progress<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Progress<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        let result = Pin::new(&mut this.inner).poll_write(cx, buf);
        this.watch(cx, result)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        let result = Pin::new(&mut this.inner).poll_write_vectored(cx, bufs);
        this.watch(cx, result)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let result = Pin::new(&mut this.inner).poll_flush(cx);
        this.watch(cx, result)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let result = Pin::new(&mut this.inner).poll_shutdown(cx);
        this.watch(cx, result)
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    use super::*;

    #[tokio::test]
    async fn a_write_without_progress_fails_after_the_limit() {
        let (client, server) = tokio::io::duplex(16);
        let mut stream = Progress::new(server, Duration::from_millis(200));
        // The peer reads nothing: the 16-byte pipe fills up.
        let started = Instant::now();
        let error = stream
            .write_all(&[0_u8; 64])
            .await
            .expect_err("stalled write");
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        let elapsed = started.elapsed();
        assert!(elapsed >= Duration::from_millis(200), "{elapsed:?}");
        assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
        drop(client);
    }

    #[tokio::test]
    async fn a_slow_reader_that_keeps_reading_is_not_cut_off() {
        let (mut client, server) = tokio::io::duplex(8);
        let mut stream = Progress::new(server, Duration::from_millis(300));
        let reader = async {
            let mut received = Vec::new();
            let mut buffer = [0_u8; 8];
            while received.len() < 64 {
                // Slower than the limit in total, never stalled that long.
                // real-clock: a real socket trickles bytes slower than the limit in total but never stalls long enough to trip it
                tokio::time::sleep(Duration::from_millis(100)).await;
                let n = client.read(&mut buffer).await.expect("read");
                received.extend_from_slice(&buffer[..n]);
            }
            received
        };
        let started = Instant::now();
        let (written, received) = tokio::join!(stream.write_all(&[7_u8; 64]), reader);
        written.expect("write made progress");
        assert_eq!(received, vec![7_u8; 64]);
        assert!(started.elapsed() > Duration::from_millis(300));
    }
}
