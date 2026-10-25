//! Streamed and buffered response bodies below the public `Response` API.

use std::task::{Context, Poll, Waker};

use bytes::Bytes;
use hyper::body::Body as _;

use crate::body::{ChunkSource, OutgoingBody, ResponseBody, Unfold};

/// One step function exercises every state of the body, because coverage
/// counts a generic function by its best single instantiation.
#[test]
fn a_streamed_body_waits_skips_empty_chunks_yields_and_ends() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    let mut source = Unfold::new(0_u32, |count: u32| async move {
        if count == 0 {
            // Not ready on the first poll: the state is kept until the
            // next one.
            tokio::task::yield_now().await;
        }
        match count {
            0..=39 => Some((Bytes::new(), count + 1)),
            40 => Some((Bytes::from_static(b"data"), 41)),
            _ => None,
        }
    });
    let mut context = Context::from_waker(Waker::noop());
    runtime.block_on(async {
        assert_eq!(source.poll_chunk(&mut context), Poll::Pending);
        // 32 empty chunks in a row: the body yields instead of holding the
        // thread.
        assert_eq!(source.poll_chunk(&mut context), Poll::Pending);
        // The remaining empty chunks are skipped and the first data comes out.
        assert_eq!(
            source.poll_chunk(&mut context),
            Poll::Ready(Some(Bytes::from_static(b"data")))
        );
        assert_eq!(source.poll_chunk(&mut context), Poll::Ready(None));
        // A finished body stays finished.
        assert_eq!(source.poll_chunk(&mut context), Poll::Ready(None));
    });
}

#[test]
fn outgoing_bodies_know_their_size_only_when_buffered() {
    let empty = OutgoingBody::from(ResponseBody::Full(Bytes::new()));
    assert!(empty.is_end_stream());
    assert_eq!(empty.size_hint().exact(), Some(0));

    let full = OutgoingBody::from(ResponseBody::Full(Bytes::from_static(b"12345")));
    assert!(!full.is_end_stream());
    assert_eq!(full.size_hint().exact(), Some(5));

    let streamed = OutgoingBody::from(ResponseBody::Stream(Box::new(Unfold::new(
        (),
        |()| async { None::<(Bytes, ())> },
    ))));
    assert!(!streamed.is_end_stream());
    assert_eq!(streamed.size_hint().exact(), None);
}
