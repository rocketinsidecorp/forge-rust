//! Accepting client connections: plain HTTP and, when configured, TLS.
//! Connections are bounded, owned by the accept loop and shut down
//! gracefully within a budget.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::log::text;
use crate::server::Server;

/// The most client connections open at once per listener; further clients
/// wait in the kernel backlog.
const MAX_CONNECTIONS: usize = 10_000;

/// Time limits of client connections.
#[derive(Clone, Copy, Debug)]
pub struct HttpLimits {
    /// Time allowed to read request headers (also the keep-alive idle
    /// limit: hyper starts the timer when it waits for the next request).
    pub read_header_timeout: Duration,
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
    let service = service_fn(move |request| {
        let server = server.clone();
        async move { Ok::<_, Infallible>(server.handle(request, peer, tls).await) }
    });
    let connection = hyper::server::conn::http1::Builder::new()
        .timer(TokioTimer::new())
        .header_read_timeout(limits.read_header_timeout)
        .max_buf_size(1 << 20)
        .serve_connection(TokioIo::new(stream), service)
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
