//! The connection limit around accept errors and shutdown, with a listener
//! that fails on demand (a real one fails with `EMFILE` when the process is
//! out of file descriptors, which a test cannot cause safely).

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use super::{
    DEFAULT_MAX_CONNECTIONS, DEFAULT_MAX_HEADER_BYTES, HttpLimits, Incoming, Plain, serve_with,
};
use crate::log::Logger;
use crate::server::Server;

/// Fails the first `failures` accepts, then accepts for real.
struct Flaky {
    failures: AtomicUsize,
    inner: TcpListener,
}

impl Incoming for Flaky {
    async fn accept(&self) -> io::Result<(TcpStream, SocketAddr)> {
        let failing = self
            .failures
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                left.checked_sub(1)
            })
            .is_ok();
        if failing {
            return Err(io::Error::other("too many open files"));
        }
        self.inner.accept().await
    }
}

fn limits(max_connections: usize) -> HttpLimits {
    HttpLimits {
        read_header_timeout: Duration::from_secs(10),
        idle_timeout: Duration::from_secs(120),
        shutdown_timeout: Duration::from_secs(5),
        max_header_bytes: DEFAULT_MAX_HEADER_BYTES,
        max_connections,
    }
}

async fn flaky(failures: usize) -> (Flaky, SocketAddr) {
    let inner = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = inner.local_addr().expect("addr");
    (
        Flaky {
            failures: AtomicUsize::new(failures),
            inner,
        },
        addr,
    )
}

/// One request to an unrouted host; the status line of the answer.
async fn status_line(addr: SocketAddr) -> String {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: nobody.example.com\r\nConnection: close\r\n\r\n")
        .await
        .expect("request");
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), stream.read_to_end(&mut response))
        .await
        .expect("answered in time")
        .expect("response");
    String::from_utf8_lossy(&response)
        .lines()
        .next()
        .unwrap_or_default()
        .to_owned()
}

#[tokio::test]
async fn failed_accepts_cost_no_permit() {
    let permits = Arc::new(Semaphore::new(1));
    let (listener, addr) = flaky(4).await;
    let shutdown = CancellationToken::new();
    let serving = tokio::spawn(serve_with(
        Server::new(Logger::discard(), None).expect("server"),
        listener,
        Arc::new(Plain),
        limits(1),
        shutdown.clone(),
        Arc::clone(&permits),
    ));

    // With a single permit, an accept error that kept (or leaked) one would
    // starve every connection after it: two are served one after the other.
    for _ in 0..2 {
        let status = status_line(addr).await;
        assert!(status.starts_with("HTTP/1.1 "), "{status:?}");
    }

    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(10), serving)
        .await
        .expect("shutdown does not wait for a permit")
        .expect("join");
    assert_eq!(permits.available_permits(), 1, "a permit was leaked");
}

#[tokio::test]
async fn shutdown_during_accept_errors_returns_every_permit() {
    let permits = Arc::new(Semaphore::new(2));
    let (listener, _addr) = flaky(usize::MAX).await;
    let shutdown = CancellationToken::new();
    let serving = tokio::spawn(serve_with(
        Server::new(Logger::discard(), None).expect("server"),
        listener,
        Arc::new(Plain),
        limits(2),
        shutdown.clone(),
        Arc::clone(&permits),
    ));
    // The loop keeps retrying, holding one permit for the retry: wait until
    // it holds it, however long a loaded runner takes to start the task.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while permits.available_permits() != 1 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the accept loop never took its retry permit"
        );
        // real-clock: polls a real TCP listener or connection, which a fake clock cannot advance, until a deadline
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // And it keeps only that one while it retries.
    // real-clock: proves the accept loop does NOT take more permits while it retries: real time must pass
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(permits.available_permits(), 1);

    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(10), serving)
        .await
        .expect("shutdown interrupts the retry")
        .expect("join");
    assert_eq!(permits.available_permits(), 2, "a permit was leaked");
}

#[test]
fn the_permits_are_the_limit_clamped_to_what_a_semaphore_holds() {
    let permits = |max_connections| limits(max_connections).connection_permits();
    assert_eq!(permits(DEFAULT_MAX_CONNECTIONS), 10_000);
    assert_eq!(permits(1), 1);
    // A limit of zero (only reachable through the library) still serves.
    assert_eq!(permits(0), 1);
    // Larger than a semaphore holds would panic when it was built.
    assert_eq!(permits(usize::MAX), Semaphore::MAX_PERMITS);
    let _ = Semaphore::new(permits(usize::MAX));
}
