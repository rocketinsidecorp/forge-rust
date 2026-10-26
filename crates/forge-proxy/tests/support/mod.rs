//! Real-socket helpers: backends, a front proxy and raw clients.

#![allow(dead_code)]

pub mod scripted_ca;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use bytes::Bytes;
use forge_proxy::log::Logger;
use forge_proxy::{DeployRequest, GoDuration, HttpLimits, Server};
use http::{HeaderMap, Request, Response, StatusCode};
use http_body_util::{BodyExt as _, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

pub const TEST_HOST: &str = "app.example.com";

pub type Handler = Arc<
    dyn Fn(
            Request<Incoming>,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Response<Full<Bytes>>> + Send>>
        + Send
        + Sync,
>;

/// An application container stand-in.
pub struct Backend {
    pub addr: String,
    pub healthy: Arc<AtomicBool>,
    pub hits: Arc<AtomicU64>,
    stop: CancellationToken,
}

impl Drop for Backend {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

pub fn text(body: impl Into<Bytes>) -> Response<Full<Bytes>> {
    Response::new(Full::new(body.into()))
}

pub fn status(code: u16) -> Response<Full<Bytes>> {
    let mut response = text(Bytes::new());
    *response.status_mut() = StatusCode::from_u16(code).expect("status");
    response
}

/// A backend answering `name`, or what `handler` returns.
pub async fn backend(name: &'static str, handler: Option<Handler>) -> Backend {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr").to_string();
    let healthy = Arc::new(AtomicBool::new(true));
    let hits = Arc::new(AtomicU64::new(0));
    let stop = CancellationToken::new();
    let (h, c, s) = (Arc::clone(&healthy), Arc::clone(&hits), stop.clone());
    tokio::spawn(async move {
        loop {
            let accepted = tokio::select! {
                accepted = listener.accept() => accepted,
                () = s.cancelled() => return,
            };
            let Ok((stream, _)) = accepted else { continue };
            let (healthy, hits, handler) = (Arc::clone(&h), Arc::clone(&c), handler.clone());
            let stop = s.clone();
            tokio::spawn(async move {
                let service = service_fn(move |request: Request<Incoming>| {
                    let (healthy, hits, handler) =
                        (Arc::clone(&healthy), Arc::clone(&hits), handler.clone());
                    async move {
                        if request.uri().path() == "/health/ready" {
                            let code = if healthy.load(Ordering::SeqCst) {
                                200
                            } else {
                                503
                            };
                            return Ok::<_, Infallible>(status(code));
                        }
                        hits.fetch_add(1, Ordering::SeqCst);
                        match handler {
                            Some(handler) => Ok(handler(request).await),
                            None => Ok(text(name)),
                        }
                    }
                });
                let connection = hyper::server::conn::http1::Builder::new()
                    .timer(TokioTimer::new())
                    .serve_connection(TokioIo::new(stream), service)
                    .with_upgrades();
                tokio::select! {
                    _ = connection => {}
                    () = stop.cancelled() => {}
                }
            });
        }
    });
    Backend {
        addr,
        healthy,
        hits,
        stop,
    }
}

/// The proxy behind a real listener.
pub struct Front {
    pub server: Server,
    pub addr: SocketAddr,
    pub stop: CancellationToken,
}

impl Drop for Front {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

pub fn limits() -> HttpLimits {
    HttpLimits {
        read_header_timeout: Duration::from_secs(10),
        idle_timeout: Duration::from_secs(120),
        shutdown_timeout: Duration::from_secs(5),
    }
}

pub async fn front_with(server: Server) -> Front {
    front_with_limits(server, limits()).await
}

pub async fn front_with_limits(server: Server, limits: HttpLimits) -> Front {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let stop = CancellationToken::new();
    tokio::spawn(forge_proxy::serve_plain(
        server.clone(),
        listener,
        limits,
        stop.clone(),
    ));
    Front { server, addr, stop }
}

pub async fn front(state: Option<std::path::PathBuf>) -> Front {
    front_with(Server::new(Logger::discard(), state).expect("server")).await
}

pub fn fast_request(target: &str) -> DeployRequest {
    DeployRequest {
        service: "app".to_owned(),
        target: target.to_owned(),
        hosts: vec![TEST_HOST.to_owned()],
        health_interval: GoDuration::from_millis(1),
        healthy_count: 2,
        deploy_timeout: GoDuration::from_secs(2),
        drain_timeout: GoDuration::from_secs(5),
        ..DeployRequest::default()
    }
}

/// One request over its own connection.
pub async fn send(
    addr: SocketAddr,
    request: Request<Full<Bytes>>,
) -> (StatusCode, HeaderMap, Bytes) {
    let stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .expect("handshake");
    tokio::spawn(connection);
    let response = sender.send_request(request).await.expect("response");
    let (parts, body) = response.into_parts();
    let body = body
        .collect()
        .await
        .map(|b| b.to_bytes())
        .unwrap_or_default();
    (parts.status, parts.headers, body)
}

pub async fn get(addr: SocketAddr, host: &str, path: &str) -> (StatusCode, HeaderMap, String) {
    let request = Request::get(path)
        .header("host", host)
        .body(Full::new(Bytes::new()))
        .expect("request");
    let (status, headers, body) = send(addr, request).await;
    (status, headers, String::from_utf8_lossy(&body).into_owned())
}

/// Sends a chunked body with a pause between chunks; returns the status and
/// how long the exchange took.
pub async fn post_chunked(
    addr: SocketAddr,
    chunks: Vec<Vec<u8>>,
    pause: Duration,
) -> (u16, Duration) {
    let mut stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
    let started = std::time::Instant::now();
    let head = format!(
        "POST /upload HTTP/1.1\r\nHost: {TEST_HOST}\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(head.as_bytes()).await.expect("head");
    let (mut reader, mut writer) = stream.into_split();
    let sending = tokio::spawn(async move {
        for chunk in chunks {
            let frame = format!("{:x}\r\n", chunk.len());
            if writer.write_all(frame.as_bytes()).await.is_err()
                || writer.write_all(&chunk).await.is_err()
                || writer.write_all(b"\r\n").await.is_err()
            {
                return;
            }
            tokio::time::sleep(pause).await;
        }
        let _ = writer.write_all(b"0\r\n\r\n").await;
        // Keep the write side open until the response arrives.
        tokio::time::sleep(Duration::from_secs(5)).await;
    });
    let mut response = Vec::new();
    let mut buffer = [0_u8; 1024];
    loop {
        match tokio::time::timeout(Duration::from_secs(10), reader.read(&mut buffer)).await {
            Ok(Ok(0)) | Err(_) | Ok(Err(_)) => break,
            Ok(Ok(n)) => {
                response.extend_from_slice(&buffer[..n]);
                if response.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
        }
    }
    let elapsed = started.elapsed();
    sending.abort();
    let text = String::from_utf8_lossy(&response);
    let code = text
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    (code, elapsed)
}
