//! `StallTimeout` over in-memory streams: vectored writes, flushes and the
//! stalled-write timeout of every write kind.

use std::io::{ErrorKind, IoSlice};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};

use crate::io::StallTimeout;

const LIMIT: Duration = Duration::from_millis(100);

#[tokio::test]
async fn writes_flushes_and_shutdowns_pass_through_while_the_peer_reads() {
    let (inner, mut peer) = duplex(64);
    let mut stream = StallTimeout::new(inner, LIMIT);
    stream.write_all(b"one ").await.expect("write");
    let written = stream
        .write_vectored(&[IoSlice::new(b"two "), IoSlice::new(b"three")])
        .await
        .expect("vectored write");
    assert_eq!(written, 9);
    // Whether the inner stream prefers vectored writes is its own business.
    let _ = tokio::io::AsyncWrite::is_write_vectored(&stream);
    stream.flush().await.expect("flush");
    stream.shutdown().await.expect("shutdown");
    let mut received = String::new();
    peer.read_to_string(&mut received).await.expect("read");
    assert_eq!(received, "one two three");
}

#[tokio::test]
async fn reads_pass_through_untimed() {
    let (inner, mut peer) = duplex(64);
    let mut stream = StallTimeout::new(inner, LIMIT);
    peer.write_all(b"hello").await.expect("peer write");
    let mut buffer = [0_u8; 5];
    stream.read_exact(&mut buffer).await.expect("read");
    assert_eq!(&buffer, b"hello");
}

#[tokio::test]
async fn a_vectored_write_that_makes_no_progress_times_out() {
    // One byte of buffer, filled and never read: the next write stalls.
    let (inner, _peer) = duplex(1);
    let mut stream = StallTimeout::new(inner, LIMIT);
    stream.write_all(b"x").await.expect("fills the buffer");
    let error = stream
        .write_vectored(&[IoSlice::new(b"yz")])
        .await
        .expect_err("the peer stopped reading");
    assert_eq!(error.kind(), ErrorKind::TimedOut);
    assert_eq!(error.to_string(), "the peer stopped reading the response");
}

#[tokio::test]
async fn a_plain_write_that_makes_no_progress_times_out_too() {
    let (inner, _peer) = duplex(1);
    let mut stream = StallTimeout::new(inner, LIMIT);
    stream.write_all(b"x").await.expect("fills the buffer");
    let error = stream.write_all(b"yz").await.expect_err("stalled");
    assert_eq!(error.kind(), ErrorKind::TimedOut);
}
