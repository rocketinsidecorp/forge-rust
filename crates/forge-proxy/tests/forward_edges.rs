//! The proxy's handling of requests and upstreams that go wrong: methods it
//! never tunnels, HTTP/1.0 clients, applications that are gone, silent,
//! hang up or answer with a protocol switch nobody asked for, and clients
//! that abandon an upload. Each ends in a clean status for the client and a
//! log line for the operator, never in a hung connection.

mod support;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use forge_proxy::log::Logger;
use forge_proxy::{GoDuration, Server};
use http::Request;
use http_body_util::Full;
use support::{TEST_HOST, backend, fast_request, front_with, get, send};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

/// A log sink the test can read.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("lock").extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Captured {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().expect("lock")).into_owned()
    }

    /// Waits until the log holds `needle`.
    async fn contains(&self, needle: &str) -> String {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        while !self.text().contains(needle) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "{needle:?} never logged: {}",
                self.text()
            );
            // real-clock: polls the logs of a task or process that runs on its own threads, until a deadline
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        self.text()
    }
}

/// What an application does with a request that is not a health check.
#[derive(Clone)]
enum Behavior {
    /// Writes these bytes, then closes.
    Answer(Vec<u8>),
    /// Closes without a word.
    Close,
    /// Reads the request and never answers.
    Hang,
}

/// An application on a raw socket, healthy for the deploy's checks.
struct Raw {
    addr: SocketAddr,
    stop: CancellationToken,
}

impl Drop for Raw {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

async fn raw_backend(behavior: Behavior) -> Raw {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let stop = CancellationToken::new();
    tokio::spawn({
        let stop = stop.clone();
        async move {
            loop {
                let accepted = tokio::select! {
                    accepted = listener.accept() => accepted,
                    () = stop.cancelled() => return,
                };
                let Ok((mut stream, _)) = accepted else {
                    continue;
                };
                let (behavior, stop) = (behavior.clone(), stop.clone());
                tokio::spawn(async move {
                    let mut head = Vec::new();
                    let mut buffer = [0_u8; 1024];
                    while !head.windows(4).any(|window| window == b"\r\n\r\n") {
                        match stream.read(&mut buffer).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => head.extend_from_slice(&buffer[..n]),
                        }
                    }
                    if head.starts_with(b"GET /health/ready ") {
                        let _ = stream
                            .write_all(
                                b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                            )
                            .await;
                        return;
                    }
                    match behavior {
                        Behavior::Answer(bytes) => {
                            let _ = stream.write_all(&bytes).await;
                        }
                        Behavior::Close => {}
                        Behavior::Hang => stop.cancelled().await,
                    }
                });
            }
        }
    });
    Raw { addr, stop }
}

/// A proxy with a logger the test reads, serving `target` for `TEST_HOST`.
async fn proxy_to(
    target: &str,
    tune: impl FnOnce(&mut forge_proxy::DeployRequest),
) -> (support::Front, Captured) {
    let logs = Captured::default();
    let server = Server::new(Logger::new(Box::new(logs.clone())), None).expect("server");
    let front = front_with(server).await;
    let mut request = fast_request(target);
    tune(&mut request);
    front.server.deploy(request).await.expect("deploy");
    (front, logs)
}

async fn exchange(addr: SocketAddr, request: &str) -> String {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    stream.write_all(request.as_bytes()).await.expect("write");
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(20), stream.read_to_end(&mut response))
        .await
        .expect("the proxy answers in time")
        .expect("read");
    String::from_utf8_lossy(&response).into_owned()
}

fn status_line(response: &str) -> &str {
    response.lines().next().unwrap_or_default()
}

#[tokio::test]
async fn the_proxy_never_opens_tunnels_to_arbitrary_destinations() {
    let app = backend("app", None).await;
    let (front, logs) = proxy_to(&app.addr, |_| {}).await;
    let response = exchange(
        front.addr,
        &format!(
            "CONNECT example.org:443 HTTP/1.1\r\nHost: {TEST_HOST}\r\nConnection: close\r\n\r\n"
        ),
    )
    .await;
    assert!(
        status_line(&response).contains("405"),
        "{}",
        status_line(&response)
    );
    assert!(response.contains("method not allowed"), "{response}");
    logs.contains("\"method\":\"CONNECT\"").await;
    assert_eq!(app.hits.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[tokio::test]
async fn http_1_0_clients_are_served_and_logged_as_such() {
    let app = backend("app", None).await;
    let (front, logs) = proxy_to(&app.addr, |_| {}).await;
    let response = exchange(
        front.addr,
        &format!("GET /old HTTP/1.0\r\nHost: {TEST_HOST}\r\n\r\n"),
    )
    .await;
    assert!(response.ends_with("app"), "{response}");
    logs.contains("\"proto\":\"HTTP/1.0\"").await;
}

#[tokio::test]
async fn an_application_that_is_gone_is_a_bad_gateway_with_the_dial_error_logged() {
    let (front, logs) = {
        let app = backend("app", None).await;
        let (front, logs) = proxy_to(&app.addr, |_| {}).await;
        assert_eq!(get(front.addr, TEST_HOST, "/").await.2, "app");
        (front, logs)
        // The application's listener stops here.
    };
    // The listener task stops asynchronously: the first refused dial is the
    // deterministic signal, so wait for the 502 rather than for a duration.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let (status, _, body) = get(front.addr, TEST_HOST, "/").await;
        if status.as_u16() == 502 {
            assert_eq!(body.trim(), "bad gateway");
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "still {status}");
        // real-clock: polls a real TCP listener or connection, which a fake clock cannot advance, until a deadline
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let text = logs.contains("\"msg\":\"upstream error\"").await;
    assert!(text.contains("dial tcp 127.0.0.1:"), "{text}");
}

#[tokio::test]
async fn an_application_that_never_answers_is_cut_off_at_the_response_timeout() {
    let app = raw_backend(Behavior::Hang).await;
    let (front, logs) = proxy_to(&app.addr.to_string(), |request| {
        request.response_timeout = GoDuration::from_millis(150);
    })
    .await;
    let (status, _, body) = get(front.addr, TEST_HOST, "/slow").await;
    assert_eq!((status.as_u16(), body.trim()), (502, "bad gateway"));
    logs.contains("timeout awaiting response headers").await;
}

#[tokio::test]
async fn an_application_that_hangs_up_or_speaks_garbage_is_a_bad_gateway() {
    for behavior in [
        Behavior::Close,
        Behavior::Answer(b"this is not http\r\n\r\n".to_vec()),
    ] {
        let app = raw_backend(behavior).await;
        let (front, logs) = proxy_to(&app.addr.to_string(), |_| {}).await;
        let (status, _, body) = get(front.addr, TEST_HOST, "/broken").await;
        assert_eq!((status.as_u16(), body.trim()), (502, "bad gateway"));
        logs.contains("\"msg\":\"upstream error\"").await;
    }
}

#[tokio::test]
async fn a_protocol_switch_nobody_asked_for_is_refused() {
    let switching = |protocol: &str| {
        Behavior::Answer(
            format!(
                "HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: {protocol}\r\n\r\n"
            )
            .into_bytes(),
        )
    };
    // Unsolicited: the client asked for no upgrade.
    let app = raw_backend(switching("websocket")).await;
    let (front, _) = proxy_to(&app.addr.to_string(), |_| {}).await;
    let (status, _, _) = get(front.addr, TEST_HOST, "/ws").await;
    assert_eq!(status.as_u16(), 502);

    // A different protocol than the client requested.
    let app = raw_backend(switching("h2c")).await;
    let (front, logs) = proxy_to(&app.addr.to_string(), |_| {}).await;
    let request = Request::get("/ws")
        .header("host", TEST_HOST)
        .header("connection", "upgrade")
        .header("upgrade", "websocket")
        .body(Full::new(Bytes::new()))
        .expect("request");
    let (status, _, _) = send(front.addr, request).await;
    assert_eq!(status.as_u16(), 502);
    let text = logs.contains("tried to switch protocol").await;
    assert!(text.contains("h2c") && text.contains("websocket"), "{text}");
}

#[tokio::test]
async fn an_abandoned_upload_is_answered_and_logged_without_reaching_the_application() {
    let app = backend("app", None).await;
    let (front, logs) = proxy_to(&app.addr, |_| {}).await;
    let mut stream = TcpStream::connect(front.addr).await.expect("connect");
    stream
        .write_all(
            format!("POST /upload HTTP/1.1\r\nHost: {TEST_HOST}\r\nContent-Length: 100\r\n\r\nabc")
                .as_bytes(),
        )
        .await
        .expect("write");
    // The client goes away with 97 bytes unsent.
    drop(stream);
    logs.contains("\"status\":408").await;
    assert_eq!(app.hits.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert_eq!(get(front.addr, TEST_HOST, "/").await.2, "app");
}
