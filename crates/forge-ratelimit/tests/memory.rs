//! The in-memory limiter against the shared conformance suite.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use forge_ratelimit::{
    MemoryLimiter, MemoryOptions,
    testing::{Subject, run},
};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn memory_limiter_conforms_to_the_limiter_contract() {
    run(|| async {
        Subject {
            limiter: Arc::new(MemoryLimiter::new(MemoryOptions::default()).expect("limiter")),
            pruner: None,
            guard: Box::new(()),
        }
    })
    .await;
}
