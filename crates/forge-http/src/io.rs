//! Connection I/O with a bound on how long a write may make no progress.

use std::{
    future::Future,
    io,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    time::Sleep,
};

/// Wraps a connection so that a write which stays blocked (the peer stopped
/// reading and the socket buffer is full) for longer than `limit` fails with
/// [`io::ErrorKind::TimedOut`], which ends the connection.
///
/// Every write that makes progress resets the clock, so slow but live
/// clients are unaffected; only a stalled peer is disconnected. Without it,
/// a client that never reads a long or streamed response would hold its
/// connection slot forever. Reads are not limited here (the server bounds
/// header and request time separately).
pub(crate) struct StallTimeout<T> {
    inner: T,
    limit: Duration,
    stalled: Option<Pin<Box<Sleep>>>,
}

impl<T> StallTimeout<T> {
    pub(crate) fn new(inner: T, limit: Duration) -> Self {
        Self {
            inner,
            limit,
            stalled: None,
        }
    }

    /// Called when the inner write is pending: starts the stall clock (the
    /// timer also registers this task's waker) or fails once it ran out.
    fn pending<R>(&mut self, context: &mut Context<'_>) -> Poll<io::Result<R>> {
        let limit = self.limit;
        let timer = self
            .stalled
            .get_or_insert_with(|| Box::pin(tokio::time::sleep(limit)));
        match timer.as_mut().poll(context) {
            Poll::Ready(()) => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "the peer stopped reading the response",
            ))),
            Poll::Pending => Poll::Pending,
        }
    }

    fn progressed<R>(&mut self, result: io::Result<R>) -> Poll<io::Result<R>> {
        self.stalled = None;
        Poll::Ready(result)
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for StallTimeout<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_read(context, buffer)
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for StallTimeout<T> {
    fn poll_write(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_write(context, buffer) {
            Poll::Ready(result) => this.progressed(result),
            Poll::Pending => this.pending(context),
        }
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffers: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_write_vectored(context, buffers) {
            Poll::Ready(result) => this.progressed(result),
            Poll::Pending => this.pending(context),
        }
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_flush(context) {
            Poll::Ready(result) => this.progressed(result),
            Poll::Pending => this.pending(context),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_shutdown(context) {
            Poll::Ready(result) => this.progressed(result),
            Poll::Pending => this.pending(context),
        }
    }
}
