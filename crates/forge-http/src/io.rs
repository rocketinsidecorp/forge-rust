//! Connection I/O with a bound on how long a write may make no progress,
//! and on how long a request head may take to arrive.

use std::{
    future::Future,
    io,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
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

/// Whether a connection is waiting for a request head: true from its
/// start and again once a handler returned, false from the moment the
/// server has a request's head until its handler returns.
#[derive(Clone, Debug)]
pub(crate) struct AwaitingHead(Arc<AtomicBool>);

impl AwaitingHead {
    pub(crate) fn new() -> Self {
        Self(Arc::new(AtomicBool::new(true)))
    }

    pub(crate) fn set(&self, awaiting: bool) {
        self.0.store(awaiting, Ordering::Release);
    }

    fn get(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// Wraps a connection so that a request head must arrive within `limit`
/// (`FORGE_HTTP_READ_HEADER_TIMEOUT`, Forge-Go's `ReadHeaderTimeout`): for
/// the first request the clock starts with the connection, for each later
/// one with its first byte. A read that is still waiting when the clock
/// runs out fails with [`io::ErrorKind::TimedOut`], which ends the
/// connection.
///
/// The wait for that first byte of a later request is the keep-alive idle
/// time, which hyper bounds with its own header timer, set to
/// `FORGE_HTTP_IDLE_TIMEOUT`: hyper starts that timer as soon as the
/// connection waits for a request, so on its own it could not tell the
/// idle time from the time a head takes.
pub(crate) struct HeadDeadline<T> {
    inner: T,
    limit: Duration,
    awaiting: AwaitingHead,
    /// The awaiting state the last read saw, to notice a new wait.
    was_awaiting: bool,
    deadline: Option<Pin<Box<Sleep>>>,
}

impl<T> HeadDeadline<T> {
    pub(crate) fn new(inner: T, limit: Duration, awaiting: AwaitingHead) -> Self {
        Self {
            inner,
            limit,
            awaiting,
            was_awaiting: true,
            // The first request's head is due from the connection's start.
            deadline: Some(Box::pin(tokio::time::sleep(limit))),
        }
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for HeadDeadline<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let awaiting = this.awaiting.get();
        if !awaiting {
            // A request is in progress: its body has its own bound.
            this.was_awaiting = false;
            this.deadline = None;
            return Pin::new(&mut this.inner).poll_read(context, buffer);
        }
        if !this.was_awaiting {
            // A handler just returned: the next head is due from its first
            // byte, and until then the connection is idle.
            this.was_awaiting = true;
            this.deadline = None;
        }
        let before = buffer.filled().len();
        match Pin::new(&mut this.inner).poll_read(context, buffer) {
            Poll::Ready(Ok(())) => {
                if buffer.filled().len() > before && this.deadline.is_none() {
                    this.deadline = Some(Box::pin(tokio::time::sleep(this.limit)));
                }
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
            Poll::Pending => {
                let expired = this
                    .deadline
                    .as_mut()
                    .is_some_and(|deadline| deadline.as_mut().poll(context).is_ready());
                if expired {
                    Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "the request head did not arrive in time",
                    )))
                } else {
                    Poll::Pending
                }
            }
        }
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for HeadDeadline<T> {
    fn poll_write(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(context, buffer)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffers: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write_vectored(context, buffers)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(context)
    }

    fn poll_shutdown(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(context)
    }
}

/// Polls a future inside `catch_unwind`, so the server can answer a
/// panicking handler with a `500` and record it. Without it the panic
/// reaches the connection task, which tokio catches only after the request's
/// state is gone, and the client is cut off without a response.
pub(crate) struct CatchUnwind<F> {
    inner: Pin<Box<F>>,
}

impl<F: Future> CatchUnwind<F> {
    pub(crate) fn new(inner: F) -> Self {
        Self {
            inner: Box::pin(inner),
        }
    }
}

impl<F: Future> Future for CatchUnwind<F> {
    type Output = Result<F::Output, Box<dyn std::any::Any + Send>>;

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let inner = self.inner.as_mut();
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| inner.poll(context))) {
            Ok(poll) => poll.map(Ok),
            Err(panic) => Poll::Ready(Err(panic)),
        }
    }
}
