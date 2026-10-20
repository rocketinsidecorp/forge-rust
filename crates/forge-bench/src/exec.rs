//! Driving futures without a runtime.

use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

/// A benchmark future that has not finished after this many polls is awaiting
/// something (I/O, a timer) that a benchmark must not wait for.
const MAX_POLLS: u32 = 1_000_000;

/// Polls `future` to completion on the calling thread, with a waker that does
/// nothing.
///
/// The futures the micro-benchmarks drive (`Router::dispatch`, the
/// in-memory stores) never suspend, so one poll completes them; this is
/// cheaper and steadier than entering a Tokio runtime for every iteration and
/// keeps the scheduler out of the figures. A future that does suspend would
/// never be woken, so it panics instead of spinning forever.
///
/// # Panics
///
/// When the future is still pending after a million polls.
pub fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    for _ in 0..MAX_POLLS {
        if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
            return output;
        }
        std::thread::yield_now();
    }
    panic!("a benchmarked future is still pending: benchmarks must not await I/O or timers");
}

#[cfg(test)]
mod tests {
    use super::block_on;

    #[test]
    fn completes_an_immediate_future() {
        assert_eq!(block_on(async { 41 + 1 }), 42);
    }

    #[test]
    #[should_panic(expected = "still pending")]
    fn refuses_a_future_that_never_completes() {
        block_on(std::future::pending::<()>());
    }
}
