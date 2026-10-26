//! [`KeepAlive`] on an in-memory stream with tokio's paused clock: the
//! limits are checked in virtual time, so a loaded machine cannot make the
//! tests flaky.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _, DuplexStream};
use tokio::time::Instant;

use super::{BUSY, HEAD, IDLE, KeepAlive};

const HEADER: Duration = Duration::from_secs(10);
const IDLE_LIMIT: Duration = Duration::from_secs(120);

fn stream() -> (DuplexStream, KeepAlive<DuplexStream>, Arc<AtomicU8>) {
    let (client, server) = tokio::io::duplex(1024);
    let phase = Arc::new(AtomicU8::new(HEAD));
    let stream = KeepAlive::new(server, Arc::clone(&phase), HEADER, IDLE_LIMIT);
    (client, stream, phase)
}

#[tokio::test(start_paused = true)]
async fn the_first_head_has_the_header_timeout_from_the_start() {
    let (_client, mut stream, _phase) = stream();
    let started = Instant::now();
    let error = stream
        .read(&mut [0_u8; 16])
        .await
        .expect_err("a silent first request times out");
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    assert_eq!(started.elapsed(), HEADER);
}

#[tokio::test(start_paused = true)]
async fn an_idle_connection_waits_the_idle_timeout_then_reads_as_closed() {
    let (_client, mut stream, phase) = stream();
    phase.store(IDLE, Ordering::SeqCst);
    let started = Instant::now();
    let read = stream.read(&mut [0_u8; 16]).await.expect("end of stream");
    assert_eq!(read, 0);
    // The idle limit, not the shorter header limit.
    assert_eq!(started.elapsed(), IDLE_LIMIT);
    // And it stays closed.
    assert_eq!(stream.read(&mut [0_u8; 16]).await.expect("closed"), 0);
}

#[tokio::test(start_paused = true)]
async fn the_next_head_has_the_header_timeout_from_its_first_byte() {
    let (mut client, mut stream, phase) = stream();
    phase.store(IDLE, Ordering::SeqCst);
    let started = Instant::now();
    let writer = tokio::spawn(async move {
        // Idle for longer than the header limit, then one byte.
        tokio::time::sleep(HEADER * 3).await;
        client.write_all(b"G").await.expect("first byte");
        client
    });
    let mut buffer = [0_u8; 16];
    assert_eq!(stream.read(&mut buffer).await.expect("first byte"), 1);
    assert_eq!(started.elapsed(), HEADER * 3);
    assert_eq!(phase.load(Ordering::SeqCst), HEAD);
    let error = stream
        .read(&mut buffer)
        .await
        .expect_err("the rest of the head never comes");
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    assert_eq!(started.elapsed(), HEADER * 4);
    drop(writer.await.expect("writer"));
}

#[tokio::test(start_paused = true)]
async fn a_busy_connection_has_no_read_limit() {
    let (mut client, mut stream, phase) = stream();
    phase.store(BUSY, Ordering::SeqCst);
    let started = Instant::now();
    let writer = tokio::spawn(async move {
        tokio::time::sleep(IDLE_LIMIT * 5).await;
        client.write_all(b"body").await.expect("body");
        client
    });
    let mut buffer = [0_u8; 16];
    assert_eq!(stream.read(&mut buffer).await.expect("body"), 4);
    assert_eq!(started.elapsed(), IDLE_LIMIT * 5);
    drop(writer.await.expect("writer"));
}

#[tokio::test(start_paused = true)]
async fn data_within_the_header_limit_is_read() {
    let (mut client, mut stream, _phase) = stream();
    let writer = tokio::spawn(async move {
        tokio::time::sleep(HEADER / 2).await;
        client.write_all(b"GET").await.expect("head");
        client
    });
    assert_eq!(stream.read(&mut [0_u8; 16]).await.expect("head"), 3);
    drop(writer.await.expect("writer"));
}
