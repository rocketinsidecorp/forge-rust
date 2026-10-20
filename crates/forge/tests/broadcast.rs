//! Live updates through a real HTTP server: an open event stream receives
//! a broadcast, and beginning shutdown ends the stream at once, so the
//! server drains long before its grace period.

#![cfg(all(feature = "broadcast", feature = "testing"))]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{Read as _, Write as _};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use forge::broadcast::{
    Channel, EventStreams, Signer, StreamAccess, StreamDenied, StreamViewer, publish,
};
use forge::config::{AppConfig, ServerConfig};
use forge::core::Cancellation;
use forge::http::{Request, Router, Server};
use forge::views::{DomId, TurboStreamAction};
use forge::{Composition, Fanout};

/// Public global channels only: enough to exercise the transport.
struct Public;

#[async_trait]
impl StreamAccess for Public {
    async fn authorize(
        &self,
        _: &Request,
        channel: &Channel,
    ) -> Result<StreamViewer, StreamDenied> {
        if channel.is_global() {
            Ok(StreamViewer::anonymous())
        } else {
            Err(StreamDenied::Forbidden)
        }
    }
}

/// Reads until `marker` arrived, or panics at EOF.
fn read_until(stream: &mut TcpStream, received: &mut String, marker: &str) {
    let mut buffer = [0_u8; 4096];
    while !received.contains(marker) {
        let read = stream.read(&mut buffer).expect("the stream must answer");
        assert!(read > 0, "connection closed before {marker:?}:\n{received}");
        received.push_str(&String::from_utf8_lossy(&buffer[..read]));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn open_streams_receive_broadcasts_and_end_when_shutdown_begins() {
    let config = AppConfig::default();
    let mut router = Router::new();
    let shutdown = Cancellation::new();
    let mut composition =
        Composition::for_testing(&config, &mut router).with_shutdown(shutdown.clone());
    let broker = composition.broadcast(Fanout::InProcess).expect("broker");
    let signer = Arc::new(Signer::random().expect("signer"));
    EventStreams::new(Arc::clone(&broker), Arc::clone(&signer), Arc::new(Public))
        .register(composition.router(), "/streams")
        .expect("registers");
    let channel = Channel::global("posts").expect("valid");
    let source = signer.source_url("/streams", &channel).expect("signs");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("binds");
    let address = listener.local_addr().expect("address");
    let server_config = ServerConfig {
        // A long grace period: the test fails if the drain waits for it.
        shutdown_grace: Duration::from_secs(60),
        ..ServerConfig::default()
    };
    let stop = shutdown.clone();
    let server = tokio::spawn(
        Server::new(server_config, router).serve(listener, async move { stop.cancelled().await }),
    );

    let client = tokio::task::spawn_blocking(move || {
        let mut stream = TcpStream::connect(address).expect("connects");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("timeout");
        stream
            .write_all(format!("GET {source} HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes())
            .expect("request");
        let mut received = String::new();
        read_until(&mut stream, &mut received, ": connected");
        (stream, received)
    })
    .await
    .expect("client");
    let (mut stream, mut received) = client;
    assert!(received.starts_with("HTTP/1.1 200 "), "{received}");
    assert!(
        received
            .to_ascii_lowercase()
            .contains("content-type: text/event-stream"),
        "{received}"
    );

    publish(
        broker.as_ref(),
        &channel,
        &[TurboStreamAction::remove(
            DomId::new("post_1").expect("valid"),
        )],
    )
    .await
    .expect("publishes");
    let (mut stream, received) = tokio::task::spawn_blocking(move || {
        read_until(&mut stream, &mut received, "target=\"post_1\"");
        (stream, received)
    })
    .await
    .expect("client");
    assert!(
        received.contains("data: <turbo-stream action=\"remove\" target=\"post_1\">"),
        "{received}"
    );

    let started = std::time::Instant::now();
    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .expect("the server drains without waiting for its grace period")
        .expect("no panic")
        .expect("clean shutdown");
    assert!(started.elapsed() < Duration::from_secs(10));
    // The stream ended: the client reaches the end of the response.
    tokio::task::spawn_blocking(move || {
        let mut rest = Vec::new();
        stream
            .read_to_end(&mut rest)
            .expect("the connection closes");
    })
    .await
    .expect("client");
}
