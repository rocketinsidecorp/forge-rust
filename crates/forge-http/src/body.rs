//! Response bodies: buffered bytes, or chunks produced over time by an
//! asynchronous step function (Server-Sent Events, for example).

use std::{
    convert::Infallible,
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

use bytes::Bytes;
use hyper::body::{Frame, SizeHint};

/// Produces the chunks of a streamed body. Implemented by [`Unfold`]; the
/// trait object keeps [`crate::Response`] free of type parameters.
pub(crate) trait ChunkSource: Send {
    /// The next non-empty chunk, or `None` once the body is complete.
    fn poll_chunk(&mut self, context: &mut Context<'_>) -> Poll<Option<Bytes>>;
}

type StepFuture<S> = Pin<Box<dyn Future<Output = Option<(Bytes, S)>> + Send>>;

/// A body built from a state and a step function, like
/// `futures::stream::unfold`: each step consumes the state and yields one
/// chunk and the next state, or `None` to end the body.
pub(crate) struct Unfold<S, F> {
    step: F,
    state: UnfoldState<S>,
}

enum UnfoldState<S> {
    Ready(S),
    Running(StepFuture<S>),
    Done,
}

impl<S, F> Unfold<S, F> {
    pub(crate) fn new(state: S, step: F) -> Self {
        Self {
            step,
            state: UnfoldState::Ready(state),
        }
    }
}

impl<S, F, Fut> ChunkSource for Unfold<S, F>
where
    S: Send + 'static,
    F: FnMut(S) -> Fut + Send,
    Fut: Future<Output = Option<(Bytes, S)>> + Send + 'static,
{
    fn poll_chunk(&mut self, context: &mut Context<'_>) -> Poll<Option<Bytes>> {
        loop {
            match std::mem::replace(&mut self.state, UnfoldState::Done) {
                UnfoldState::Ready(state) => {
                    self.state = UnfoldState::Running(Box::pin((self.step)(state)));
                }
                UnfoldState::Running(mut future) => match future.as_mut().poll(context) {
                    Poll::Pending => {
                        self.state = UnfoldState::Running(future);
                        return Poll::Pending;
                    }
                    Poll::Ready(None) => return Poll::Ready(None),
                    Poll::Ready(Some((chunk, state))) => {
                        self.state = UnfoldState::Ready(state);
                        // An empty chunk carries nothing; ask for the next.
                        if !chunk.is_empty() {
                            return Poll::Ready(Some(chunk));
                        }
                    }
                },
                UnfoldState::Done => return Poll::Ready(None),
            }
        }
    }
}

/// What a [`crate::Response`] carries.
pub(crate) enum ResponseBody {
    /// The whole body, already in memory.
    Full(Bytes),
    /// Chunks produced while the response is being written.
    Stream(Box<dyn ChunkSource>),
}

/// The body handed to Hyper.
pub(crate) enum OutgoingBody {
    /// Buffered bytes, `None` once sent (or when empty).
    Full(Option<Bytes>),
    /// A streamed body, sent with chunked transfer encoding.
    Stream(Box<dyn ChunkSource>),
}

impl From<ResponseBody> for OutgoingBody {
    fn from(body: ResponseBody) -> Self {
        match body {
            ResponseBody::Full(bytes) if bytes.is_empty() => Self::Full(None),
            ResponseBody::Full(bytes) => Self::Full(Some(bytes)),
            ResponseBody::Stream(source) => Self::Stream(source),
        }
    }
}

impl hyper::body::Body for OutgoingBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        match self.get_mut() {
            Self::Full(bytes) => Poll::Ready(bytes.take().map(|bytes| Ok(Frame::data(bytes)))),
            Self::Stream(source) => source
                .poll_chunk(context)
                .map(|chunk| chunk.map(|bytes| Ok(Frame::data(bytes)))),
        }
    }

    fn is_end_stream(&self) -> bool {
        matches!(self, Self::Full(None))
    }

    fn size_hint(&self) -> SizeHint {
        match self {
            Self::Full(None) => SizeHint::with_exact(0),
            Self::Full(Some(bytes)) => {
                SizeHint::with_exact(u64::try_from(bytes.len()).unwrap_or(u64::MAX))
            }
            Self::Stream(_) => SizeHint::default(),
        }
    }
}
