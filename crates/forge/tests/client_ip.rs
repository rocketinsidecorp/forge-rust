//! The client address through a real HTTP server (ADR 0032):
//! `FORGE_HTTP_TRUSTED_PROXIES` is read by configuration, the server resolves
//! `Request::client_ip` from it, and a sign-in throttle keyed by it gives two
//! clients behind one trusted proxy independent counters, while forged
//! headers from untrusted peers change nothing.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use forge::auth::{
    LoginThrottle, MemoryLoginThrottle, ThrottleConfig, ThrottleDecision, ThrottleKey,
};
use forge::config::AppConfig;
use forge::core::Cancellation;
use forge::http::{Method, Request, Response, Router, Server, StatusCode};

/// A sign-in stand-in: counts every attempt for one login by client address.
fn router() -> Router {
    let throttle = Arc::new(
        MemoryLoginThrottle::new(ThrottleConfig {
            window: Duration::from_secs(900),
            pair_limit: 2,
            account_limit: 50,
            source_limit: 300,
            max_entries: 1_000,
        })
        .expect("throttle"),
    );
    let mut router = Router::new();
    router
        .route(Method::POST, "/session", move |request: Request| {
            let throttle = Arc::clone(&throttle);
            async move {
                let key = ThrottleKey::new("sign_in", "ada@example.com", Some(request.client_ip()));
                match throttle.attempt(&key).await {
                    Ok(ThrottleDecision::Allowed) => {
                        Response::new(StatusCode::OK, request.client_ip().to_string())
                    }
                    Ok(ThrottleDecision::Throttled { .. }) => {
                        Response::new(StatusCode::TOO_MANY_REQUESTS, "throttled")
                    }
                    Err(_) => Response::new(StatusCode::SERVICE_UNAVAILABLE, "unavailable"),
                }
            }
        })
        .expect("route");
    router
}

/// Serves `router` with the configuration read from `trusted_proxies` and
/// returns the address and the shutdown.
async fn serve(trusted_proxies: Option<&str>) -> (SocketAddr, Cancellation) {
    let value = trusted_proxies.map(str::to_owned);
    let config = AppConfig::from_lookup(|key| {
        (key == "FORGE_HTTP_TRUSTED_PROXIES")
            .then(|| value.clone())
            .flatten()
    })
    .expect("valid configuration");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("binds");
    let address = listener.local_addr().expect("address");
    let shutdown = Cancellation::new();
    let stop = shutdown.clone();
    tokio::spawn(
        Server::new(config.server, router()).serve(listener, async move { stop.cancelled().await }),
    );
    (address, shutdown)
}

/// Posts a sign-in attempt from 127.0.0.1 with `X-Forwarded-For` lines.
async fn attempt(address: SocketAddr, forwarded_for: &[&str]) -> (u16, String) {
    let mut raw = String::from(
        "POST /session HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nConnection: close\r\n",
    );
    for line in forwarded_for {
        raw.push_str(&format!("X-Forwarded-For: {line}\r\n"));
    }
    raw.push_str("X-Real-IP: 192.0.2.99\r\nForwarded: for=192.0.2.98\r\n\r\n");
    tokio::task::spawn_blocking(move || {
        let mut stream = TcpStream::connect(address).expect("connects");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("timeout");
        stream.write_all(raw.as_bytes()).expect("writes");
        let mut response = String::new();
        stream.read_to_string(&mut response).expect("reads");
        let status = response
            .get(9..12)
            .and_then(|code| code.parse().ok())
            .expect("status line");
        let body = response
            .split_once("\r\n\r\n")
            .map(|(_, body)| body.to_owned())
            .unwrap_or_default();
        (status, body)
    })
    .await
    .expect("client task")
}

#[tokio::test]
async fn clients_behind_a_trusted_proxy_get_independent_sign_in_counters() {
    // The test client connects from 127.0.0.1, which plays the proxy.
    let (address, shutdown) = serve(Some("127.0.0.0/8")).await;

    assert_eq!(
        attempt(address, &["203.0.113.9"]).await,
        (200, "203.0.113.9".to_owned())
    );
    assert_eq!(attempt(address, &["203.0.113.9"]).await.0, 200);
    assert_eq!(attempt(address, &["203.0.113.9"]).await.0, 429);
    // Forged entries left of the proxy's entry, on the same or another
    // header line, do not escape the counter.
    assert_eq!(
        attempt(address, &["198.51.100.7, 203.0.113.9"]).await.0,
        429
    );
    assert_eq!(
        attempt(address, &["198.51.100.7", "203.0.113.9"]).await.0,
        429
    );
    // Another client behind the same proxy has its own counter.
    assert_eq!(
        attempt(address, &["198.51.100.7"]).await,
        (200, "198.51.100.7".to_owned())
    );
    // Rotating inside one IPv6 /64 is one client.
    assert_eq!(attempt(address, &["2001:db8:1:2::1"]).await.0, 200);
    assert_eq!(attempt(address, &["2001:db8:1:2::2"]).await.0, 200);
    assert_eq!(attempt(address, &["2001:db8:1:2:ffff::3"]).await.0, 429);
    // A malformed or overlong chain is the proxy itself, never a value the
    // client chose.
    assert_eq!(
        attempt(address, &["203.0.113.50:80"]).await,
        (200, "127.0.0.1".to_owned())
    );
    let overlong = vec!["127.0.0.5"; 33].join(", ");
    let spoofed = format!("192.0.2.1, {overlong}");
    assert_eq!(
        attempt(address, &[&spoofed]).await,
        (200, "127.0.0.1".to_owned())
    );
    shutdown.cancel();
}

#[tokio::test]
async fn without_trusted_proxies_forwarding_headers_are_ignored() {
    let (address, shutdown) = serve(None).await;
    // Every request is the peer, whatever it claims: one counter.
    assert_eq!(
        attempt(address, &["203.0.113.1"]).await,
        (200, "127.0.0.1".to_owned())
    );
    assert_eq!(attempt(address, &["203.0.113.2"]).await.0, 200);
    assert_eq!(attempt(address, &["203.0.113.3"]).await.0, 429);
    shutdown.cancel();
}

#[test]
fn unsafe_trusted_proxies_stop_the_configuration_without_echoing_them() {
    for value in [
        "0.0.0.0/0",
        "::/0",
        "10.0.0.0/7",
        "fc00::/15",
        "::ffff:10.0.0.0/104",
        "proxy.internal",
        "10.0.0.0/8,",
    ] {
        let error = AppConfig::from_lookup(|key| {
            (key == "FORGE_HTTP_TRUSTED_PROXIES").then(|| value.to_owned())
        })
        .expect_err(value);
        let message = error.to_string();
        assert!(message.contains("FORGE_HTTP_TRUSTED_PROXIES"), "{message}");
        assert!(!message.contains(value), "{value}: {message}");
    }
}
