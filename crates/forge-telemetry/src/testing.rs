//! An OTLP/HTTP receiver for tests (feature `testing`): a hyper server
//! standing in for an OpenTelemetry Collector that records every request it
//! accepts, decompressing gzip bodies. Test support: it panics on setup
//! failures and malformed bodies.

use std::{
    convert::Infallible,
    io::Read as _,
    net::SocketAddr,
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};

use bytes::Bytes;
use http::{HeaderMap, HeaderValue, Request, Response, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper::{body::Incoming, service::service_fn};
use hyper_util::rt::TokioIo;
use serde_json::Value;
use tokio::{net::TcpListener, task::JoinHandle};

use crate::OtlpClient;

/// One accepted export request.
#[derive(Clone, Debug)]
pub struct Received {
    /// Request path, such as `/v1/traces`.
    pub path: String,
    /// Request headers.
    pub headers: HeaderMap,
    /// The JSON body (decompressed).
    pub body: Value,
}

/// What the receiver answers.
#[derive(Clone, Copy, Debug)]
pub enum Behavior {
    /// 200 for everything.
    Accept,
    /// The status for the first `n` requests, 200 afterwards.
    FailFirst(usize, u16),
    /// Always this status.
    Always(u16),
    /// Never answers.
    Hang,
}

/// The running receiver; stopped when dropped.
pub struct Receiver {
    address: SocketAddr,
    received: Arc<Mutex<Vec<Received>>>,
    attempts: Arc<Mutex<usize>>,
    task: JoinHandle<()>,
}

impl Receiver {
    /// Listens on a loopback port.
    pub async fn start(behavior: Behavior) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        let received = Arc::new(Mutex::new(Vec::new()));
        let attempts = Arc::new(Mutex::new(0_usize));
        let (store, count) = (Arc::clone(&received), Arc::clone(&attempts));
        let task = tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let (store, count) = (Arc::clone(&store), Arc::clone(&count));
                connections.spawn(async move {
                    let service = service_fn(move |request: Request<Incoming>| {
                        let (store, count) = (Arc::clone(&store), Arc::clone(&count));
                        async move {
                            let attempt = {
                                let mut count =
                                    count.lock().unwrap_or_else(PoisonError::into_inner);
                                *count += 1;
                                *count
                            };
                            let (parts, body) = request.into_parts();
                            let bytes = body
                                .collect()
                                .await
                                .map(http_body_util::Collected::to_bytes)
                                .unwrap_or_default();
                            let status = match behavior {
                                Behavior::Accept => 200,
                                Behavior::FailFirst(n, status) if attempt <= n => status,
                                Behavior::FailFirst(..) => 200,
                                Behavior::Always(status) => status,
                                Behavior::Hang => {
                                    std::future::pending::<()>().await;
                                    200
                                }
                            };
                            if status == 200 {
                                let json = if parts.headers.get("content-encoding")
                                    == Some(&HeaderValue::from_static("gzip"))
                                {
                                    let mut text = String::new();
                                    flate2::read::GzDecoder::new(&bytes[..])
                                        .read_to_string(&mut text)
                                        .expect("gzip");
                                    text
                                } else {
                                    String::from_utf8(bytes.to_vec()).expect("utf-8")
                                };
                                store.lock().unwrap_or_else(PoisonError::into_inner).push(
                                    Received {
                                        path: parts.uri.path().to_owned(),
                                        headers: parts.headers,
                                        body: serde_json::from_str(&json).expect("JSON body"),
                                    },
                                );
                            }
                            let mut response = Response::new(Full::new(Bytes::from_static(b"{}")));
                            *response.status_mut() = StatusCode::from_u16(status).expect("status");
                            Ok::<_, Infallible>(response)
                        }
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
        });
        Self {
            address,
            received,
            attempts,
            task,
        }
    }

    /// The base URL to configure as the OTLP endpoint.
    #[must_use]
    pub fn endpoint(&self) -> String {
        format!("http://{}", self.address)
    }

    /// A client for this receiver with a short retry backoff.
    #[must_use]
    pub fn client(&self, headers: HeaderMap, gzip: bool, timeout: Duration) -> Arc<OtlpClient> {
        Arc::new(
            OtlpClient::new(&self.endpoint(), headers, gzip, timeout)
                .expect("valid endpoint")
                .with_initial_backoff(Duration::from_millis(10)),
        )
    }

    /// The accepted requests to `path`.
    #[must_use]
    pub fn received(&self, path: &str) -> Vec<Received> {
        self.received
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|received| received.path == path)
            .cloned()
            .collect()
    }

    /// Every request received, accepted or not.
    #[must_use]
    pub fn attempts(&self) -> usize {
        *self.attempts.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        self.task.abort();
    }
}
