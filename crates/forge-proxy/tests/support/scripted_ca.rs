//! A scripted ACME certificate authority: a loopback HTTP server whose every
//! answer a test decides, to drive the client through failures a real CA
//! produces.

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use forge_proxy::acme::Client;
use forge_proxy::der::generate_p256;
use http::{Request, Response, StatusCode};
use http_body_util::Full;
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

/// What the scripted CA is asked.
pub struct Ask<'a> {
    pub base: &'a str,
    pub method: &'a str,
    pub path: &'a str,
    /// How many requests with this method and path came before.
    pub nth: usize,
}

pub struct Reply {
    status: u16,
    headers: Vec<(&'static str, String)>,
    body: Vec<u8>,
}

pub fn reply(status: u16, body: &Value) -> Reply {
    Reply {
        status,
        headers: Vec::new(),
        body: body.to_string().into_bytes(),
    }
}

pub fn raw(status: u16, body: impl Into<Vec<u8>>) -> Reply {
    Reply {
        status,
        headers: Vec::new(),
        body: body.into(),
    }
}

impl Reply {
    pub fn with(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.headers.push((name, value.into()));
        self
    }
}

pub fn problem(kind: &str, detail: &str) -> Value {
    json!({"type": kind, "detail": detail})
}

/// A CA that does everything right; tests override single steps.
pub fn happy(ask: &Ask<'_>) -> Reply {
    let base = ask.base;
    match (ask.method, ask.path) {
        ("GET", "/directory") => reply(
            200,
            &json!({"newNonce": format!("{base}/nonce"), "newAccount": format!("{base}/account"), "newOrder": format!("{base}/order")}),
        ),
        ("HEAD", "/nonce") => raw(200, ""),
        ("POST", "/account") => reply(201, &json!({"status": "valid"})).with("location", format!("{base}/acct/1")),
        ("POST", "/order") => reply(
            201,
            &json!({"status": "pending", "authorizations": [format!("{base}/authz")], "finalize": format!("{base}/finalize")}),
        )
        .with("location", format!("{base}/order/1")),
        ("POST", "/authz") => reply(200, &json!({"status": "valid"})),
        ("POST", "/finalize") => reply(200, &json!({"status": "processing"})),
        ("POST", "/order/1") => {
            reply(200, &json!({"status": "valid", "certificate": format!("{base}/cert")}))
        }
        ("POST", "/cert") => raw(200, "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n"),
        _ => reply(404, &problem("urn:ietf:params:acme:error:malformed", "unknown")),
    }
}

/// A pending authorization offering one challenge.
pub fn pending(base: &str, token: &str) -> Reply {
    reply(
        200,
        &json!({"status": "pending", "challenges": [{"type": "http-01", "token": token, "url": format!("{base}/chall")}]}),
    )
}

pub type Script = dyn Fn(&Ask<'_>) -> Reply + Send + Sync;

/// A scripted CA on a loopback port; stops when dropped. Every response
/// carries a fresh `Replay-Nonce` unless the script removed it.
pub struct Ca {
    pub base: String,
    stop: CancellationToken,
}

impl Drop for Ca {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

pub async fn ca(script: impl Fn(&Ask<'_>) -> Reply + Send + Sync + 'static) -> Ca {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    let stop = CancellationToken::new();
    let script: Arc<Script> = Arc::new(script);
    let counts: Arc<Mutex<HashMap<String, usize>>> = Arc::default();
    let nonces = Arc::new(std::sync::atomic::AtomicU64::new(0));
    tokio::spawn({
        let (base, stop) = (base.clone(), stop.clone());
        async move {
            loop {
                let accepted = tokio::select! {
                    accepted = listener.accept() => accepted,
                    () = stop.cancelled() => return,
                };
                let Ok((stream, _)) = accepted else { continue };
                let (script, counts, nonces, base) = (
                    Arc::clone(&script),
                    Arc::clone(&counts),
                    Arc::clone(&nonces),
                    base.clone(),
                );
                tokio::spawn(async move {
                    let service = hyper::service::service_fn(
                        move |request: Request<hyper::body::Incoming>| {
                            let (script, counts, nonces, base) = (
                                Arc::clone(&script),
                                Arc::clone(&counts),
                                Arc::clone(&nonces),
                                base.clone(),
                            );
                            async move {
                                let method = request.method().as_str().to_owned();
                                let path = request.uri().path().to_owned();
                                let nth = {
                                    let mut counts = counts.lock().expect("counts");
                                    let seen =
                                        counts.entry(format!("{method} {path}")).or_default();
                                    let nth = *seen;
                                    *seen += 1;
                                    nth
                                };
                                let reply = script(&Ask {
                                    base: &base,
                                    method: &method,
                                    path: &path,
                                    nth,
                                });
                                let mut response =
                                    Response::new(Full::new(Bytes::from(reply.body)));
                                *response.status_mut() =
                                    StatusCode::from_u16(reply.status).expect("status");
                                let serial =
                                    nonces.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                                response.headers_mut().insert(
                                    "replay-nonce",
                                    format!("nonce-{serial}").parse().expect("nonce"),
                                );
                                for (name, value) in reply.headers {
                                    if value.is_empty() {
                                        response.headers_mut().remove(name);
                                    } else {
                                        response
                                            .headers_mut()
                                            .insert(name, value.parse().expect("header"));
                                    }
                                }
                                Ok::<_, std::convert::Infallible>(response)
                            }
                        },
                    );
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
        }
    });
    Ca { base, stop }
}

pub fn client(base: &str) -> Client {
    Client::new(
        &format!("{base}/directory"),
        &generate_p256().expect("key"),
        "ops@example.com",
    )
    .expect("client")
}

/// A script that is `happy` except for the steps `change` answers.
pub fn except(
    change: impl Fn(&Ask<'_>) -> Option<Reply> + Send + Sync + 'static,
) -> impl Fn(&Ask<'_>) -> Reply + Send + Sync + 'static {
    move |ask| change(ask).unwrap_or_else(|| happy(ask))
}
