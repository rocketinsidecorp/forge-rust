//! The conformance suite every [`Limiter`] must pass (feature `testing`).
//!
//! Run it against a real database for the shared limiters:
//!
//! ```ignore
//! forge_ratelimit::testing::run(|| async {
//!     Subject::new(limiter_on_a_fresh_migrated_database)
//! })
//! .await;
//! ```
//!
//! The suite panics with a descriptive message on the first violated
//! guarantee. Windows are short so expiry can be observed with real clocks,
//! including the database clocks of the shared limiters.
//!
//! [`run_unavailable`] is the contract of a shared limiter whose storage fails.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{
    any::Any,
    future::Future,
    sync::Arc,
    time::{Duration, Instant},
};

use crate::{Limit, LimitError, Limiter, MAX_PER, Pruner};

/// A limiter on fresh storage plus whatever must stay alive with it (such as a
/// test database).
pub struct Subject {
    /// The limiter under test.
    pub limiter: Arc<dyn Limiter>,
    /// Its pruner, when it keeps persistent buckets.
    pub pruner: Option<Arc<dyn Pruner>>,
    /// Kept alive until the case ends.
    pub guard: Box<dyn Any + Send + Sync>,
}

const WINDOW: Duration = Duration::from_millis(400);

fn limit(n: u32, per: Duration) -> Limit {
    Limit::new(n, per).expect("valid limit")
}

async fn allow(limiter: &Arc<dyn Limiter>, key: &str, limit: Limit) -> crate::Decision {
    limiter
        .allow(key, limit)
        .await
        .unwrap_or_else(|error| panic!("allow({key}): {error}"))
}

/// Runs every case, each on a fresh [`Subject`].
pub async fn run<F, Fut>(new_subject: F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = Subject>,
{
    allows_up_to_the_limit_then_denies(new_subject().await).await;
    keys_are_independent(new_subject().await).await;
    window_resets(new_subject().await).await;
    denied_hits_report_retry_after(new_subject().await).await;
    concurrent_hits_never_exceed_the_limit(new_subject().await).await;
    rejects_invalid_keys(new_subject().await).await;
    prunes_expired_buckets_only(new_subject().await).await;
}

async fn allows_up_to_the_limit_then_denies(subject: Subject) {
    let l = limit(3, Duration::from_secs(60));
    for i in 0..3 {
        let d = allow(&subject.limiter, "k", l).await;
        assert!(d.allowed, "hit {} must be allowed", i + 1);
        assert_eq!(d.remaining, 3 - i - 1, "remaining after hit {}", i + 1);
        assert!(d.retry_after.is_zero(), "allowed hits carry no retry-after");
    }
    for _ in 0..3 {
        let d = allow(&subject.limiter, "k", l).await;
        assert!(!d.allowed && d.remaining == 0, "hit over the limit: {d:?}");
    }
}

async fn keys_are_independent(subject: Subject) {
    let l = limit(1, Duration::from_secs(60));
    assert!(allow(&subject.limiter, "a", l).await.allowed);
    assert!(!allow(&subject.limiter, "a", l).await.allowed);
    assert!(
        allow(&subject.limiter, "b", l).await.allowed,
        "key b must not be affected by key a"
    );
}

async fn window_resets(subject: Subject) {
    let l = limit(2, WINDOW);
    // The third hit is refused only inside the window. A store that took the
    // whole window to answer three hits (a loaded CI runner) may rightly have
    // opened a new one, so the refusal is asserted on the first attempt, each
    // on a fresh key, whose hits finished inside the window (it opens at the
    // first hit, after `started`). If no attempt does, the test fails: the
    // assertion cannot be skipped silently.
    let mut asserted = false;
    for attempt in 0..5 {
        let key = format!("k{attempt}");
        let started = Instant::now();
        allow(&subject.limiter, &key, l).await;
        allow(&subject.limiter, &key, l).await;
        let third = allow(&subject.limiter, &key, l).await;
        if started.elapsed() >= WINDOW {
            continue;
        }
        assert!(!third.allowed, "third hit in the window");
        tokio::time::sleep(WINDOW + Duration::from_millis(150)).await;
        let d = allow(&subject.limiter, &key, l).await;
        assert!(
            d.allowed && d.remaining == 1,
            "first hit of a new window: {d:?}"
        );
        asserted = true;
        break;
    }
    assert!(
        asserted,
        "no attempt finished its three hits inside a {WINDOW:?} window in 5 tries"
    );
}

async fn denied_hits_report_retry_after(subject: Subject) {
    let l = limit(1, Duration::from_secs(30));
    allow(&subject.limiter, "k", l).await;
    let d = allow(&subject.limiter, "k", l).await;
    assert!(!d.allowed);
    assert!(
        d.retry_after > Duration::ZERO && d.retry_after <= l.per(),
        "retry-after {:?} must be within (0, {:?}]",
        d.retry_after,
        l.per()
    );
}

async fn concurrent_hits_never_exceed_the_limit(subject: Subject) {
    const TASKS: usize = 16;
    const HITS_EACH: usize = 10;
    const LIMIT_N: u32 = 25;
    let l = limit(LIMIT_N, Duration::from_secs(60));
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..TASKS {
        let limiter = Arc::clone(&subject.limiter);
        tasks.spawn(async move {
            let mut allowed = 0_u32;
            for _ in 0..HITS_EACH {
                if limiter.allow("shared", l).await.expect("allow").allowed {
                    allowed += 1;
                }
            }
            allowed
        });
    }
    let mut total = 0;
    while let Some(result) = tasks.join_next().await {
        total += result.expect("task");
    }
    assert_eq!(
        total,
        LIMIT_N,
        "{} concurrent hits must allow exactly {LIMIT_N}",
        TASKS * HITS_EACH
    );
}

async fn rejects_invalid_keys(subject: Subject) {
    let l = limit(1, Duration::from_secs(60));
    let long = "9".repeat(129);
    for (name, key) in [
        ("empty key", ""),
        ("space in key", "a b"),
        ("non-ASCII key", "café"),
        ("long key", long.as_str()),
        ("control in key", "a\u{0}b"),
    ] {
        let result = subject.limiter.allow(key, l).await;
        assert!(
            matches!(result, Err(LimitError::InvalidKey)),
            "{name}: expected InvalidKey, got {result:?}"
        );
    }
    // 128 characters is the longest valid key.
    allow(&subject.limiter, &"1".repeat(128), l).await;
    // The window bound is enforced by `Limit` itself.
    assert!(Limit::new(1, MAX_PER + Duration::from_secs(3600)).is_err());
}

async fn prunes_expired_buckets_only(subject: Subject) {
    let Some(pruner) = subject.pruner.clone() else {
        return;
    };
    let short = limit(5, WINDOW);
    let long = limit(5, Duration::from_secs(3600));
    for i in 0..3 {
        allow(&subject.limiter, &format!("short-{i}"), short).await;
    }
    allow(&subject.limiter, "long", long).await;
    tokio::time::sleep(WINDOW + Duration::from_millis(150)).await;
    assert_eq!(
        pruner.prune().await.expect("prune"),
        3,
        "the 3 expired buckets"
    );
    let d = allow(&subject.limiter, "long", long).await;
    assert_eq!(d.remaining, 3, "the live bucket keeps its count");
    assert_eq!(
        pruner.prune().await.expect("prune"),
        0,
        "nothing left to remove"
    );
}

/// What every database-backed limiter must do when its storage fails (the
/// Forge-Go `limitertest.RunUnavailable`): `allow` reports the failure and
/// never a decision (the caller chooses whether to fail open or closed),
/// pruning reports it, and the hits counted before the outage still count
/// afterwards while the failed hit counts for nothing.
///
/// `subject` is a limiter on fresh storage. `sabotage` makes that storage
/// unavailable (a renamed table, a revoked grant) and `restore` makes it
/// available again; both run with a connection that can change the schema.
///
/// The subject's database must not be shared with other tests: a panic between
/// `sabotage` and `restore` leaves the table renamed, and nothing puts it back.
///
/// # Panics
///
/// With a message naming the first guarantee the limiter broke.
pub async fn run_unavailable<S, R>(subject: Subject, sabotage: S, restore: R)
where
    S: AsyncFnOnce(),
    R: AsyncFnOnce(),
{
    let l = limit(2, Duration::from_secs(3600));
    allow(&subject.limiter, "key", l).await; // one hit of two

    sabotage().await;
    match subject.limiter.allow("key", l).await {
        Err(LimitError::Store(_)) => {}
        other => panic!("allow during the outage = {other:?}; want the failure and no decision"),
    }
    if let Some(pruner) = &subject.pruner {
        assert!(
            pruner.prune().await.is_err(),
            "prune succeeded during the outage"
        );
    }
    restore().await;

    // The failed hit was not counted: one hit remains, then the limit bites.
    let second = allow(&subject.limiter, "key", l).await;
    assert!(
        second.allowed && second.remaining == 0,
        "second hit = {second:?}, want allowed with none remaining"
    );
    let third = allow(&subject.limiter, "key", l).await;
    assert!(
        !third.allowed,
        "third hit = {third:?}, want denied: the hit before the outage must still count"
    );
}
