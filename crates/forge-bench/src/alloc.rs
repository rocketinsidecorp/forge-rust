//! A counting global allocator: the heap figures (`B/op`, `allocs/op`) of the
//! micro-benchmarks.
//!
//! This is the only `unsafe` code in the repository. It is an exception to the
//! "no framework-authored unsafe" rule that is justified, isolated and bounded:
//!
//! - **Where it lives.** This module of `forge-bench`, a crate that no other
//!   crate depends on and that is never part of a release build. The bench
//!   binary opts in with `#[global_allocator]`; nothing else does.
//! - **What it does.** [`Counting`] forwards every call, unchanged, to
//!   [`System`] and adds to two atomic counters. It never inspects, moves,
//!   resizes or frees memory itself.
//! - **Invariants (the soundness argument).** `GlobalAlloc`'s contract is
//!   the caller's: layouts and pointers come from the language's allocation
//!   machinery. Forwarding them verbatim to `System`, which implements the
//!   same contract, preserves it. The counters are `AtomicU64`s updated with
//!   `Relaxed` ordering: they synchronize nothing, cannot panic and cannot
//!   allocate, so the implementation never re-enters the allocator and never
//!   unwinds out of an allocator method.
//! - **Tests.** `tests/alloc.rs` runs a real program under the allocator and
//!   checks the counts and that memory survives `realloc`.
//!
//! Counts are process-wide (every thread, as Go's `testing` package counts
//! every goroutine) and include the allocations of the harness between runs,
//! which are negligible next to the iterations they are divided by.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};

static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);
static BYTES: AtomicU64 = AtomicU64::new(0);

/// Heap activity since the process started: allocation calls and bytes
/// requested. A `realloc` counts as one allocation of its new size.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Snapshot {
    /// Calls to `alloc`, `alloc_zeroed` and `realloc`.
    pub allocations: u64,
    /// Bytes requested by those calls.
    pub bytes: u64,
}

impl Snapshot {
    /// The activity between `earlier` and `self`.
    #[must_use]
    pub fn since(self, earlier: Snapshot) -> Snapshot {
        Snapshot {
            allocations: self.allocations.saturating_sub(earlier.allocations),
            bytes: self.bytes.saturating_sub(earlier.bytes),
        }
    }
}

/// The counters now. All zero unless [`Counting`] is the global allocator.
#[must_use]
pub fn snapshot() -> Snapshot {
    Snapshot {
        allocations: ALLOCATIONS.load(Ordering::Relaxed),
        bytes: BYTES.load(Ordering::Relaxed),
    }
}

/// [`System`] with counters; install it with `#[global_allocator]`.
#[derive(Clone, Copy, Debug, Default)]
pub struct Counting;

fn record(size: usize) {
    ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
    BYTES.fetch_add(size as u64, Ordering::Relaxed);
}

// SAFETY: every method forwards its arguments unchanged to `System`, which
// upholds the `GlobalAlloc` contract, so the contract holds for `Counting`.
// `record` only touches atomics: it cannot allocate, panic or unwind.
#[allow(unsafe_code)]
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        // SAFETY: the caller upholds `GlobalAlloc::alloc`'s contract for
        // `layout`; it is passed on untouched.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        // SAFETY: as in `alloc`, for `GlobalAlloc::alloc_zeroed`.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` and `layout` are the caller's, from a previous call of
        // this allocator, which delegated to `System`.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        record(new_size);
        // SAFETY: `ptr`, `layout` and `new_size` satisfy `GlobalAlloc::realloc`'s
        // contract because the caller's do, and they are passed on untouched.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}
