//! `-max-header-bytes` and `-max-connections` on client connections, over
//! real sockets: a head over the limit is answered with `431`, and a
//! listener holds at most `max_connections` open connections, the rest
//! waiting in the kernel's listen backlog until one closes.

mod support;

use std::net::SocketAddr;
use std::time::Duration;

use forge_proxy::log::Logger;
use forge_proxy::{
    DEFAULT_MAX_CONNECTIONS, DEFAULT_MAX_HEADER_BYTES, HttpLimits, MIN_MAX_HEADER_BYTES, Server,
    serve_plain,
};
use support::{TEST_HOST, backend, fast_request};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// A proxy in front of one application, and the handle of its accept loop.
struct Proxy {
    addr: SocketAddr,
    stop: CancellationToken,
    serving: JoinHandle<()>,
    app: support::Backend,
}

async fn proxy(limits: HttpLimits) -> Proxy {
    let app = backend("app", None).await;
    let server = Server::new(Logger::discard(), None).expect("server");
    server
        .deploy(fast_request(&app.addr))
        .await
        .expect("deploy");
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let stop = CancellationToken::new();
    let serving = tokio::spawn(serve_plain(server, listener, limits, stop.clone()));
    Proxy {
        addr,
        stop,
        serving,
        app,
    }
}

fn with(max_header_bytes: usize, max_connections: usize) -> HttpLimits {
    HttpLimits {
        max_header_bytes,
        max_connections,
        ..support::limits()
    }
}

/// A request whose request line and headers are exactly `head_bytes` long.
fn request_of(head_bytes: usize) -> Vec<u8> {
    let prefix = format!("GET / HTTP/1.1\r\nHost: {TEST_HOST}\r\nX-Pad: ");
    let suffix = "\r\n\r\n";
    let padding = head_bytes
        .checked_sub(prefix.len() + suffix.len())
        .expect("a head this large has room for the padding");
    let request = format!("{prefix}{}{suffix}", "a".repeat(padding));
    assert_eq!(request.len(), head_bytes);
    request.into_bytes()
}

/// Sends `request` on a new connection; the status line and whether the
/// proxy then closed the connection.
async fn exchange(addr: SocketAddr, request: &[u8]) -> (String, bool) {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    // The proxy may answer and close before it has read all of an oversized
    // head, so a failed write is not a failed test: the answer is read below.
    let _ = stream.write_all(request).await;
    let mut response = Vec::new();
    let mut closed = false;
    let mut buffer = [0_u8; 4096];
    loop {
        match tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buffer)).await {
            Ok(Ok(0) | Err(_)) => {
                closed = true;
                break;
            }
            Ok(Ok(n)) => response.extend_from_slice(&buffer[..n]),
            Err(_) => break,
        }
        // An answered request stays open; a refused one is read to its end.
        let text = String::from_utf8_lossy(&response);
        if text.contains("\r\n\r\n") && text.ends_with("app") {
            break;
        }
    }
    let text = String::from_utf8_lossy(&response);
    (text.lines().next().unwrap_or_default().to_owned(), closed)
}

#[test]
fn the_defaults_are_64_kib_and_10000_connections() {
    assert_eq!(DEFAULT_MAX_HEADER_BYTES, 65_536);
    assert_eq!(MIN_MAX_HEADER_BYTES, 4_096);
    assert_eq!(DEFAULT_MAX_CONNECTIONS, 10_000);
    let limits = support::limits();
    assert_eq!(limits.max_header_bytes, 65_536);
    assert_eq!(limits.max_connections, 10_000);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_head_over_the_limit_is_answered_with_431_and_never_reaches_the_application() {
    let limit = MIN_MAX_HEADER_BYTES;
    let front = proxy(with(limit, 100)).await;

    // Exactly the limit is allowed, one byte more is not.
    let (status, _) = exchange(front.addr, &request_of(limit)).await;
    assert!(status.starts_with("HTTP/1.1 200"), "{status}");
    assert_eq!(front.app.hits.load(std::sync::atomic::Ordering::SeqCst), 1);

    for head in [limit + 1, limit * 2, 1 << 20] {
        let (status, closed) = exchange(front.addr, &request_of(head)).await;
        assert!(
            status.starts_with("HTTP/1.1 431"),
            "a head of {head} bytes: {status}"
        );
        assert!(closed, "the connection is closed after the 431");
    }
    assert_eq!(
        front.app.hits.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "an oversized request was forwarded"
    );
    front.stop.cancel();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_default_head_limit_is_64_kib() {
    let front = proxy(support::limits()).await;
    let limit = DEFAULT_MAX_HEADER_BYTES;
    // Well above the 8 KiB of hyper's buffer and the old 1 MiB bound.
    let (status, _) = exchange(front.addr, &request_of(limit - 1024)).await;
    assert!(status.starts_with("HTTP/1.1 200"), "{status}");
    let (status, _) = exchange(front.addr, &request_of(limit + 1024)).await;
    assert!(status.starts_with("HTTP/1.1 431"), "{status}");
    front.stop.cancel();
}

/// Opens a connection that has completed one request, so the proxy holds it
/// (keep-alive) and it counts against the limit.
async fn held(addr: SocketAddr) -> TcpStream {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    stream
        .write_all(format!("GET / HTTP/1.1\r\nHost: {TEST_HOST}\r\n\r\n").as_bytes())
        .await
        .expect("request");
    let mut response = Vec::new();
    let mut buffer = [0_u8; 1024];
    while !String::from_utf8_lossy(&response).ends_with("\r\n\r\napp") {
        let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buffer))
            .await
            .expect("answered in time")
            .expect("response");
        assert!(n > 0, "closed before the response");
        response.extend_from_slice(&buffer[..n]);
    }
    stream
}

/// Whether `stream` receives the first bytes of a response within `wait`.
async fn answered_within(stream: &mut TcpStream, wait: Duration) -> bool {
    let mut buffer = [0_u8; 16];
    matches!(
        tokio::time::timeout(wait, stream.read(&mut buffer)).await,
        Ok(Ok(n)) if n > 0
    )
}

async fn waiting(addr: SocketAddr) -> TcpStream {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    stream
        .write_all(format!("GET / HTTP/1.1\r\nHost: {TEST_HOST}\r\n\r\n").as_bytes())
        .await
        .expect("request");
    stream
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_listener_serves_at_most_max_connections_and_the_rest_wait_for_a_slot() {
    let front = proxy(with(DEFAULT_MAX_HEADER_BYTES, 2)).await;
    let first = held(front.addr).await;
    let second = held(front.addr).await;

    // The kernel completes the handshake, but nothing is accepted: no
    // answer while both slots are taken.
    let mut third = waiting(front.addr).await;
    assert!(
        !answered_within(&mut third, Duration::from_millis(500)).await,
        "a third connection was served over the limit of 2"
    );

    // One close frees exactly one slot: the third is served, a fourth waits.
    drop(first);
    assert!(
        answered_within(&mut third, Duration::from_secs(5)).await,
        "a freed slot did not admit the waiting connection"
    );
    let mut fourth = waiting(front.addr).await;
    assert!(
        !answered_within(&mut fourth, Duration::from_millis(500)).await,
        "a closed connection freed more than one slot"
    );
    drop(second);
    assert!(answered_within(&mut fourth, Duration::from_secs(5)).await);
    front.stop.cancel();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_does_not_wait_for_a_slot_and_ends_every_connection() {
    let front = proxy(with(DEFAULT_MAX_HEADER_BYTES, 1)).await;
    let mut open = held(front.addr).await;
    // Another client waits for the only slot, so the accept loop is blocked
    // on admission, not on accept, when shutdown begins.
    let mut waiting = waiting(front.addr).await;
    assert!(!answered_within(&mut waiting, Duration::from_millis(300)).await);

    front.stop.cancel();
    tokio::time::timeout(Duration::from_secs(10), front.serving)
        .await
        .expect("the accept loop stops at the cap")
        .expect("join");

    // The held connection was asked to finish and closed; nothing is left
    // listening for the waiting one.
    let mut buffer = [0_u8; 16];
    let read = tokio::time::timeout(Duration::from_secs(5), open.read(&mut buffer))
        .await
        .expect("the held connection ends");
    assert!(matches!(read, Ok(0) | Err(_)), "{read:?}");
    let read = tokio::time::timeout(Duration::from_secs(5), waiting.read(&mut buffer))
        .await
        .expect("the waiting connection ends");
    assert!(!matches!(read, Ok(n) if n > 0), "{read:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_limit_of_one_still_serves_connections_one_after_the_other() {
    let front = proxy(with(DEFAULT_MAX_HEADER_BYTES, 1)).await;
    for _ in 0..3 {
        let (status, closed) = exchange(front.addr, &request_of(512)).await;
        assert!(status.starts_with("HTTP/1.1 200"), "{status}");
        assert!(!closed, "the response completes before the connection ends");
    }
    front.stop.cancel();
}
