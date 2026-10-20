//! The counting allocator, as the global allocator of a real program.
//!
//! The counters are process-wide and the test harness's own threads allocate,
//! so these tests assert lower bounds that the code under test guarantees,
//! never exact totals.

use std::hint::black_box;

use forge_bench::alloc::{Counting, snapshot};

#[global_allocator]
static ALLOCATOR: Counting = Counting;

#[test]
fn counts_every_allocation_and_its_bytes() {
    let before = snapshot();
    let boxes: Vec<Box<[u8; 100]>> = (0..1_000).map(|_| black_box(Box::new([7; 100]))).collect();
    let used = snapshot().since(before);
    assert!(used.allocations >= 1_000, "{used:?}");
    assert!(used.bytes >= 100_000, "{used:?}");
    // The memory is intact: the allocator hands out what the program wrote.
    assert!(
        boxes
            .iter()
            .all(|boxed| boxed.iter().all(|byte| *byte == 7))
    );
}

#[test]
fn a_zeroed_allocation_is_counted_and_zeroed() {
    let before = snapshot();
    let zeroes = black_box(vec![0_u8; 4_096]);
    let used = snapshot().since(before);
    assert!(used.allocations >= 1 && used.bytes >= 4_096, "{used:?}");
    assert!(zeroes.iter().all(|byte| *byte == 0));
}

#[test]
fn a_growing_vector_counts_reallocations_and_keeps_its_contents() {
    let before = snapshot();
    let mut numbers = Vec::new();
    for number in 0..10_000_u32 {
        numbers.push(number);
    }
    let used = snapshot().since(before);
    // Doubling growth from empty to 10,000 u32 takes at least a dozen
    // allocation calls (the first allocation and the reallocations).
    assert!(used.allocations >= 10, "{used:?}");
    assert!(
        numbers
            .iter()
            .enumerate()
            .all(|(index, number)| *number == index as u32)
    );
}

#[test]
fn freeing_is_not_counted_as_allocating() {
    let kept = black_box(Box::new(1_u64));
    let before = snapshot();
    drop(kept);
    let used = snapshot().since(before);
    // Dropping allocates nothing; other test threads may, but never 1,000.
    assert!(used.allocations < 1_000, "{used:?}");
}

#[test]
fn since_saturates_instead_of_wrapping() {
    let now = snapshot();
    let later = forge_bench::alloc::Snapshot {
        allocations: now.allocations + 5,
        bytes: now.bytes + 50,
    };
    assert_eq!(now.since(later), forge_bench::alloc::Snapshot::default());
    assert_eq!(later.since(now).allocations, 5);
}
