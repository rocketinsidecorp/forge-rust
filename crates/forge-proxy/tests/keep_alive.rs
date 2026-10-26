//! `-idle-timeout` and `-read-header-timeout` on client connections, as Go's
//! `http.Server` applies `IdleTimeout` and `ReadHeaderTimeout`: a keep-alive
//! connection may wait the idle timeout for its next request, whose headers
//! then have the header timeout; the first request has the header timeout
//! from the start of the connection.
//!
//! These run over real sockets, so their bounds are wide: a timer never
//! fires early, so lower bounds are exact, and upper bounds only need to stay
//! below the other limit. The exact limits are checked in virtual time by
//! the unit tests in `src/listen/keep_alive_tests.rs`.

mod support;

use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use forge_proxy::log::Logger;
use forge_proxy::{HttpLimits, Server};
use support::{Front, TEST_HOST, backend, fast_request, front_with_limits, text};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;

fn limits(read_header: Duration, idle: Duration) -> HttpLimits {
    HttpLimits {
        read_header_timeout: read_header,
        idle_timeout: idle,
        shutdown_timeout: Duration::from_secs(5),
        ..support::limits()
    }
}

async fn proxy(limits: HttpLimits, handler: Option<support::Handler>) -> (Front, support::Backend) {
    let app = backend("app", handler).await;
    let front = front_with_limits(
        Server::new(Logger::discard(), None).expect("server"),
        limits,
    )
    .await;
    front
        .server
        .deploy(fast_request(&app.addr))
        .await
        .expect("deploy");
    (front, app)
}

/// Sends one keep-alive request and reads its response (body `app`).
async fn request(stream: &mut TcpStream) {
    stream
        .write_all(format!("GET / HTTP/1.1\r\nHost: {TEST_HOST}\r\n\r\n").as_bytes())
        .await
        .expect("request");
    let mut response = Vec::new();
    let mut buffer = [0_u8; 1024];
    while !String::from_utf8_lossy(&response).ends_with("\r\n\r\napp") {
        let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buffer))
            .await
            .expect("response in time")
            .expect("response");
        assert!(n > 0, "closed before the response");
        response.extend_from_slice(&buffer[..n]);
    }
    assert!(
        String::from_utf8_lossy(&response).starts_with("HTTP/1.1 200"),
        "{}",
        String::from_utf8_lossy(&response)
    );
}

/// How long until the proxy closes `stream` (reads 0 or fails), at most
/// `wait`; `None` if it stays open.
async fn closed_after(stream: &mut TcpStream, wait: Duration) -> Option<Duration> {
    let started = Instant::now();
    let mut buffer = [0_u8; 1024];
    loop {
        match tokio::time::timeout_at(
            tokio::time::Instant::from_std(started + wait),
            stream.read(&mut buffer),
        )
        .await
        {
            Err(_) => return None,
            Ok(Ok(0) | Err(_)) => return Some(started.elapsed()),
            Ok(Ok(_)) => {}
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_keep_alive_connection_waits_the_idle_timeout_not_the_header_timeout() {
    let (front, _app) = proxy(
        limits(Duration::from_millis(200), Duration::from_secs(3)),
        None,
    )
    .await;
    let mut stream = TcpStream::connect(front.addr).await.expect("connect");
    request(&mut stream).await;

    // Idle for five header timeouts: still open (before the fix the
    // shorter header timeout closed it). A slow runner only stretches the
    // pause, and only past the 3 s idle limit would it fail.
    // real-clock: the connection must stay idle for real time to show the idle limit does not cut it
    tokio::time::sleep(Duration::from_secs(1)).await;
    request(&mut stream).await;

    // Then closed once idle for the idle timeout (measured from after the
    // response was read, so a little less than the limit at the earliest).
    let closed = closed_after(&mut stream, Duration::from_secs(60))
        .await
        .expect("an idle connection is closed");
    assert!(closed >= Duration::from_millis(2500), "{closed:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_next_request_head_has_the_header_timeout_from_its_first_byte() {
    let (front, _app) = proxy(
        limits(Duration::from_millis(300), Duration::from_secs(30)),
        None,
    )
    .await;
    let mut stream = TcpStream::connect(front.addr).await.expect("connect");
    request(&mut stream).await;
    // real-clock: the connection must stay idle for real time before the next request
    tokio::time::sleep(Duration::from_millis(600)).await;

    // A head that starts and never ends is cut at the header timeout, not
    // after the (longer) idle timeout.
    stream
        .write_all(format!("GET / HTTP/1.1\r\nHost: {TEST_HOST}\r\n").as_bytes())
        .await
        .expect("partial head");
    // Well before the 30 s idle limit.
    let closed = closed_after(&mut stream, Duration::from_secs(20))
        .await
        .expect("a stalled head is cut off before the idle limit");
    assert!(closed >= Duration::from_millis(250), "{closed:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_first_request_has_the_header_timeout_from_the_connection_start() {
    let (front, _app) = proxy(
        limits(Duration::from_millis(300), Duration::from_secs(30)),
        None,
    )
    .await;
    let mut stream = TcpStream::connect(front.addr).await.expect("connect");
    // Well before the 30 s idle limit.
    closed_after(&mut stream, Duration::from_secs(20))
        .await
        .expect("a silent client is cut off before the idle limit");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_slow_response_is_not_cut_by_the_idle_or_header_timeout() {
    let slow: support::Handler = Arc::new(|_| {
        Box::pin(async {
            // real-clock: real sockets: the handler must outlive the timeout under test, so a paused clock would skip it instantly
            tokio::time::sleep(Duration::from_millis(900)).await;
            text(Bytes::from_static(b"app"))
        })
    });
    let (front, _app) = proxy(
        limits(Duration::from_millis(200), Duration::from_millis(300)),
        Some(slow),
    )
    .await;
    let mut stream = TcpStream::connect(front.addr).await.expect("connect");
    // While the request is served, the client sends nothing for three idle
    // timeouts; twice on the same connection.
    request(&mut stream).await;
    request(&mut stream).await;
}
