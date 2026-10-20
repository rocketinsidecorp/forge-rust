//! Shared helpers of the telemetry tests: an instrumented server and a raw
//! HTTP/1.1 client.

#![allow(dead_code)]

use std::{net::SocketAddr, time::Duration};

use forge_config::ServerConfig;
use forge_http::{
    BoxResponseFuture, Method, Middleware, Next, Request, Response, Router, Server, StatusCode,
};
use forge_telemetry::{Options, SpanSink, Telemetry, testing::Receiver};
use http::HeaderMap;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
};

pub const INCOMING: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

/// Answers `418` for `/teapot` before routing.
struct Teapot;

impl Middleware for Teapot {
    fn handle<'a>(&'a self, request: Request, next: Next<'a>) -> BoxResponseFuture<'a> {
        if request.uri().path() == "/teapot" {
            return Box::pin(async { Response::empty(StatusCode::IM_A_TEAPOT) });
        }
        next.run(request)
    }
}

fn router() -> Router {
    let mut router = Router::new();
    router.layer(Teapot);
    router
        .route(Method::GET, "/users/{id}", |request: Request| async move {
            tracing::info!(route = request.route(), "handled");
            let traceparent = request
                .trace_context()
                .map(forge_telemetry::SpanContext::traceparent)
                .unwrap_or_default();
            Response::new(StatusCode::OK, traceparent)
        })
        .expect("route");
    router
        .route(Method::GET, "/fail", |_| async {
            Response::empty(StatusCode::SERVICE_UNAVAILABLE)
        })
        .expect("route");
    router
        .route(Method::GET, "/panic", |_| async {
            panic!("handler bug");
        })
        .expect("route");
    router
}

pub struct Running {
    pub address: SocketAddr,
    stop: oneshot::Sender<()>,
    task: tokio::task::JoinHandle<()>,
}

pub async fn serve(telemetry: &Telemetry) -> Running {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    let config = ServerConfig {
        bind: address,
        ..ServerConfig::default()
    };
    let server =
        Server::new(config, router()).with_telemetry(telemetry.http_server().expect("instruments"));
    let (stop, stopped) = oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        server
            .serve(listener, async {
                let _ = stopped.await;
            })
            .await
            .expect("serves");
    });
    Running {
        address,
        stop,
        task,
    }
}

impl Running {
    pub async fn stop(self) {
        let _ = self.stop.send(());
        self.task.await.expect("server task");
    }
}

/// Sends one request and returns the status code and body.
pub async fn get(address: SocketAddr, path: &str, headers: &[(&str, &str)]) -> (u16, String) {
    let mut stream = TcpStream::connect(address).await.expect("connect");
    let mut request = format!("GET {path} HTTP/1.1\r\nhost: test\r\nconnection: close\r\n");
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).await.expect("write");
    let mut response = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(10), stream.read_to_end(&mut response)).await;
    let text = String::from_utf8_lossy(&response).into_owned();
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    let body = text
        .split_once("\r\n\r\n")
        .map(|(_, body)| body.to_owned())
        .unwrap_or_default();
    (status, body)
}

pub fn telemetry(trust: bool, receiver: Option<&Receiver>) -> Telemetry {
    let mut options = Options::new("svc");
    options.trust_incoming = trust;
    if let Some(receiver) = receiver {
        options.spans = Some(SpanSink::Otlp(receiver.client(
            HeaderMap::new(),
            false,
            Duration::from_secs(10),
        )));
        options.logs = Some(receiver.client(HeaderMap::new(), false, Duration::from_secs(10)));
    }
    let telemetry = Telemetry::new(options).expect("valid");
    telemetry.start();
    telemetry
}
