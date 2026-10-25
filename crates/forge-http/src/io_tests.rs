//! `StallTimeout`: every kind of write passes through while the inner stream
//! makes progress and fails with `TimedOut` once it stops.
//!
//! One inner type serves every case: coverage counts a generic function by
//! its best single instantiation, so the progress and stall arms of each
//! write kind must all run on the same one.

use std::io::{self, ErrorKind, IoSlice};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};

use crate::io::StallTimeout;

const LIMIT: Duration = Duration::from_millis(50);

/// Accepts everything, or blocks every write kind while `blocked` is set.
struct Gate {
    blocked: Arc<AtomicBool>,
}

impl Gate {
    fn ready<T>(&self, value: T) -> Poll<io::Result<T>> {
        if self.blocked.load(Ordering::SeqCst) {
            Poll::Pending
        } else {
            Poll::Ready(Ok(value))
        }
    }
}

impl AsyncWrite for Gate {
    fn poll_write(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.ready(buffer.len())
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        buffers: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        self.ready(buffers.iter().map(|buffer| buffer.len()).sum())
    }

    fn is_write_vectored(&self) -> bool {
        true
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.ready(())
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.ready(())
    }
}

impl AsyncRead for Gate {
    fn poll_read(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        buffer.put_slice(b"hi");
        Poll::Ready(Ok(()))
    }
}

fn gate(blocked: bool) -> StallTimeout<Gate> {
    StallTimeout::new(
        Gate {
            blocked: Arc::new(AtomicBool::new(blocked)),
        },
        LIMIT,
    )
}

#[tokio::test]
async fn every_write_kind_passes_through_while_the_peer_makes_progress() {
    let mut stream = gate(false);
    stream.write_all(b"plain").await.expect("write");
    let written = stream
        .write_vectored(&[IoSlice::new(b"two "), IoSlice::new(b"three")])
        .await
        .expect("vectored write");
    assert_eq!(written, 9);
    assert!(AsyncWrite::is_write_vectored(&stream));
    stream.flush().await.expect("flush");
    stream.shutdown().await.expect("shutdown");
    let mut buffer = [0_u8; 2];
    stream.read_exact(&mut buffer).await.expect("read");
    assert_eq!(&buffer, b"hi");
}

#[tokio::test]
async fn every_write_kind_times_out_when_the_peer_stops_reading() {
    let mut stalled = gate(true);
    let error = stalled.write_all(b"plain").await.expect_err("write stalls");
    assert_eq!(error.kind(), ErrorKind::TimedOut);
    assert_eq!(error.to_string(), "the peer stopped reading the response");
    let error = gate(true)
        .write_vectored(&[IoSlice::new(b"a")])
        .await
        .expect_err("vectored write stalls");
    assert_eq!(error.kind(), ErrorKind::TimedOut);
    let error = gate(true).flush().await.expect_err("flush stalls");
    assert_eq!(error.kind(), ErrorKind::TimedOut);
    let error = gate(true).shutdown().await.expect_err("shutdown stalls");
    assert_eq!(error.kind(), ErrorKind::TimedOut);
}
