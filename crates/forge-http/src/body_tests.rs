//! Streamed and buffered response bodies below the public `Response` API.

use std::task::{Context, Poll, Waker};

use bytes::Bytes;
use hyper::body::Body as _;

use crate::body::{ChunkSource, OutgoingBody, ResponseBody, Unfold};

#[test]
fn a_step_that_only_yields_empty_chunks_gives_way_to_the_scheduler_and_resumes() {
    let mut source = Unfold::new(0_u32, |count: u32| async move {
        match count {
            0..=39 => Some((Bytes::new(), count + 1)),
            40 => Some((Bytes::from_static(b"data"), 41)),
            _ => None,
        }
    });
    let mut context = Context::from_waker(Waker::noop());
    // 32 empty chunks in a row: the body yields instead of holding the thread.
    assert_eq!(source.poll_chunk(&mut context), Poll::Pending);
    // The remaining empty chunks are skipped and the first data comes out.
    assert_eq!(
        source.poll_chunk(&mut context),
        Poll::Ready(Some(Bytes::from_static(b"data")))
    );
    assert_eq!(source.poll_chunk(&mut context), Poll::Ready(None));
    // A finished body stays finished.
    assert_eq!(source.poll_chunk(&mut context), Poll::Ready(None));
}

#[tokio::test]
async fn a_step_that_is_not_ready_keeps_its_state_until_polled_again() {
    let mut source = Unfold::new(false, |ready: bool| async move {
        if !ready {
            tokio::task::yield_now().await;
        }
        Some((Bytes::from_static(b"late"), true))
    });
    let mut context = Context::from_waker(Waker::noop());
    assert_eq!(source.poll_chunk(&mut context), Poll::Pending);
    assert_eq!(
        source.poll_chunk(&mut context),
        Poll::Ready(Some(Bytes::from_static(b"late")))
    );
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
