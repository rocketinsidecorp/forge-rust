//! Accepting client connections: plain HTTP and, when configured, TLS.
//! Connections are bounded, owned by the accept loop and shut down
//! gracefully within a budget. A client that stops reading is disconnected
//! once a write to it makes no progress for the idle timeout.

use std::convert::Infallible;
use std::future::Future as _;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio::time::{Instant, Sleep};
use tokio_util::sync::CancellationToken;

use crate::log::text;
use crate::server::Server;

/// The most client connections open at once per listener; further clients
/// wait in the kernel backlog.
const MAX_CONNECTIONS: usize = 10_000;

/// Time limits of client connections.
#[derive(Clone, Copy, Debug)]
pub struct HttpLimits {
    /// Time allowed to read request headers.
    pub read_header_timeout: Duration,
    /// How long a keep-alive connection may wait for its next request (hyper
    /// starts the header timer then, so the shorter of this and
    /// `read_header_timeout` applies), how long a write to the client may
    /// make no progress, and how long an upgraded (WebSocket) connection
    /// may carry no data in either direction.
    pub idle_timeout: Duration,
    /// Graceful shutdown budget.
    pub shutdown_timeout: Duration,
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
    let mut tasks = JoinSet::new();
    let connections = CancellationToken::new();
    let permits = Arc::new(Semaphore::new(MAX_CONNECTIONS));
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
    let service = service_fn(move |request| {
        let server = server.clone();
        async move { Ok::<_, Infallible>(server.handle(request, peer, tls, idle).await) }
    });
    let connection = hyper::server::conn::http1::Builder::new()
        .timer(TokioTimer::new())
        .header_read_timeout(limits.read_header_timeout.min(idle))
        .max_buf_size(1 << 20)
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
