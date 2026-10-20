use std::{
    collections::{BTreeMap, HashMap},
    sync::{Mutex, MutexGuard, PoisonError},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use forge_config::RateLimitConfig;

use crate::{Decision, Limit, LimitError, Limiter, check_key};

/// What [`MemoryLimiter`] does when it holds `max_keys` live buckets and a new
/// key arrives.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum WhenFull {
    /// Drops the bucket closest to expiring to make room. Under a flood of
    /// distinct keys this resets the counters of the evicted keys (an attacker
    /// needs more than `max_keys` keys in one window to do so), but legitimate
    /// new clients are never refused.
    #[default]
    EvictSoonest,
    /// Refuses the new key (as rate limited) until a bucket expires. No
    /// counter is ever lost, but a flood of distinct keys locks out new
    /// clients. Prefer it where a bypass is worse than an outage.
    Deny,
}

/// Bounds of a [`MemoryLimiter`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MemoryOptions {
    max_keys: usize,
    when_full: WhenFull,
}

/// Largest `max_keys`.
const MAX_MEMORY_KEYS: usize = 10_000_000;

impl Default for MemoryOptions {
    fn default() -> Self {
        Self {
            max_keys: 100_000,
            when_full: WhenFull::EvictSoonest,
        }
    }
}

impl MemoryOptions {
    /// The options `FORGE_RATE_LIMIT_MAX_KEYS` selects.
    #[must_use]
    pub fn from_config(config: &RateLimitConfig) -> Self {
        Self {
            max_keys: config.max_keys,
            ..Self::default()
        }
    }

    /// Bounds the live buckets (`1..=10_000_000`, checked by
    /// [`MemoryLimiter::new`]).
    #[must_use]
    pub const fn with_max_keys(mut self, max_keys: usize) -> Self {
        self.max_keys = max_keys;
        self
    }

    /// Chooses the behavior when full.
    #[must_use]
    pub const fn with_when_full(mut self, when_full: WhenFull) -> Self {
        self.when_full = when_full;
        self
    }
}

type Clock = Box<dyn Fn() -> Instant + Send + Sync>;

/// A [`Limiter`] that keeps its buckets in process memory.
///
/// Its size is bounded by `max_keys`. It starts no tasks: expired buckets
/// leave through a deadline-ordered map during later calls, so there is
/// nothing to stop and nothing that can leak. Counters saturate at `N + 1`, so
/// a flood cannot overflow them.
///
/// State is lost on restart and not shared between replicas; with `R`
/// replicas the effective limit is up to `R` times higher. Use a shared store
/// when that matters.
pub struct MemoryLimiter {
    options: MemoryOptions,
    clock: Clock,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    buckets: HashMap<String, Bucket>,
    /// Buckets ordered by the end of their window; the id breaks ties.
    deadlines: BTreeMap<(Instant, u64), String>,
    next_id: u64,
}

struct Bucket {
    hits: u32,
    reset_at: Instant,
    id: u64,
}

impl std::fmt::Debug for MemoryLimiter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MemoryLimiter")
            .field("options", &self.options)
            .finish_non_exhaustive()
    }
}

impl MemoryLimiter {
    /// An empty limiter.
    pub fn new(options: MemoryOptions) -> Result<Self, LimitError> {
        Self::with_clock(options, Box::new(Instant::now))
    }

    pub(crate) fn with_clock(options: MemoryOptions, clock: Clock) -> Result<Self, LimitError> {
        if !(1..=MAX_MEMORY_KEYS).contains(&options.max_keys) {
            return Err(LimitError::Configuration(
                "rate limit max keys must be between 1 and 10000000",
            ));
        }
        Ok(Self {
            options,
            clock,
            state: Mutex::new(State::default()),
        })
    }

    /// Buckets currently held, expired ones included until a later call drops
    /// them.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().buckets.len()
    }

    /// Whether no bucket is held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // A panic while the lock was held cannot leave the maps inconsistent
        // in a way that weakens limits: every mutation keeps them in step.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl State {
    fn expire(&mut self, now: Instant) {
        while let Some(entry) = self.deadlines.first_entry() {
            if now < entry.key().0 {
                break;
            }
            let key = entry.remove();
            self.buckets.remove(&key);
        }
    }

    fn remove(&mut self, key: &str) {
        if let Some(bucket) = self.buckets.remove(key) {
            self.deadlines.remove(&(bucket.reset_at, bucket.id));
        }
    }

    fn until_first_expiry(&self, now: Instant) -> Duration {
        self.deadlines
            .keys()
            .next()
            .map_or(Duration::from_secs(1), |(reset_at, _)| {
                reset_at.saturating_duration_since(now)
            })
    }
}

#[async_trait]
impl Limiter for MemoryLimiter {
    async fn allow(&self, key: &str, limit: Limit) -> Result<Decision, LimitError> {
        check_key(key)?;
        let now = (self.clock)();
        let mut state = self.lock();
        state.expire(now);

        if !state.buckets.contains_key(key) {
            if state.buckets.len() >= self.options.max_keys {
                match self.options.when_full {
                    WhenFull::Deny => {
                        return Ok(Decision::denied(state.until_first_expiry(now)));
                    }
                    WhenFull::EvictSoonest => {
                        let soonest = state.deadlines.values().next().cloned();
                        if let Some(soonest) = soonest {
                            state.remove(&soonest);
                        }
                    }
                }
            }
            let reset_at = now
                .checked_add(limit.per())
                .ok_or(LimitError::Configuration(
                    "rate limit window overflows the clock",
                ))?;
            state.next_id += 1;
            let id = state.next_id;
            state.deadlines.insert((reset_at, id), key.to_owned());
            state.buckets.insert(
                key.to_owned(),
                Bucket {
                    hits: 0,
                    reset_at,
                    id,
                },
            );
        }
        let Some(bucket) = state.buckets.get_mut(key) else {
            return Err(LimitError::Configuration("rate limit bucket vanished"));
        };
        // Saturate: a flood cannot overflow the counter.
        if bucket.hits <= limit.n() {
            bucket.hits += 1;
        }
        if bucket.hits > limit.n() {
            return Ok(Decision::denied(
                bucket.reset_at.saturating_duration_since(now),
            ));
        }
        Ok(Decision::allowed(limit.n() - bucket.hits))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };

    use super::*;

    /// A clock tests advance by hand.
    #[derive(Clone)]
    pub(crate) struct ManualClock {
        base: Instant,
        offset_nanos: Arc<AtomicU64>,
    }

    impl ManualClock {
        pub(crate) fn new() -> Self {
            Self {
                base: Instant::now(),
                offset_nanos: Arc::new(AtomicU64::new(0)),
            }
        }

        pub(crate) fn advance(&self, by: Duration) {
            let nanos = u64::try_from(by.as_nanos()).unwrap_or(u64::MAX);
            self.offset_nanos.fetch_add(nanos, Ordering::SeqCst);
        }

        pub(crate) fn boxed(&self) -> Clock {
            let clock = self.clone();
            Box::new(move || {
                clock.base + Duration::from_nanos(clock.offset_nanos.load(Ordering::SeqCst))
            })
        }
    }

    fn limiter(max_keys: usize, when_full: WhenFull) -> (MemoryLimiter, ManualClock) {
        let clock = ManualClock::new();
        let options = MemoryOptions::default()
            .with_max_keys(max_keys)
            .with_when_full(when_full);
        let limiter = MemoryLimiter::with_clock(options, clock.boxed()).expect("valid options");
        (limiter, clock)
    }

    fn one_per_minute() -> Limit {
        Limit::new(1, Duration::from_secs(60)).expect("valid limit")
    }

    #[tokio::test]
    async fn is_bounded_and_evicts_the_soonest_expiring() {
        let (limiter, clock) = limiter(3, WhenFull::EvictSoonest);
        let limit = one_per_minute();
        for i in 0..3 {
            assert!(
                limiter
                    .allow(&format!("k{i}"), limit)
                    .await
                    .expect("allow")
                    .allowed
            );
            clock.advance(Duration::from_secs(1));
        }
        // k0 is closest to expiring: a fourth key evicts it and never grows the map.
        assert!(limiter.allow("k3", limit).await.expect("allow").allowed);
        assert_eq!(limiter.len(), 3);
        assert!(
            limiter.allow("k0", limit).await.expect("allow").allowed,
            "k0 was evicted, so its counter restarts"
        );
        for i in 0..10_000 {
            limiter
                .allow(&format!("flood-{i}"), limit)
                .await
                .expect("allow");
        }
        assert_eq!(
            limiter.len(),
            3,
            "a flood of distinct keys cannot grow the map"
        );
    }

    #[tokio::test]
    async fn deny_when_full_keeps_every_counter() {
        let (limiter, clock) = limiter(2, WhenFull::Deny);
        let limit = one_per_minute();
        limiter.allow("a", limit).await.expect("allow");
        limiter.allow("b", limit).await.expect("allow");
        let decision = limiter.allow("c", limit).await.expect("allow");
        assert!(!decision.allowed && decision.retry_after > Duration::ZERO);
        assert!(
            !limiter.allow("a", limit).await.expect("allow").allowed,
            "a's counter survives"
        );
        clock.advance(Duration::from_secs(60));
        assert!(limiter.allow("c", limit).await.expect("allow").allowed);
    }

    #[tokio::test]
    async fn expired_buckets_leave_the_map_without_any_sweeper() {
        let (limiter, clock) = limiter(100, WhenFull::EvictSoonest);
        let limit = Limit::new(1, Duration::from_secs(1)).expect("valid limit");
        for i in 0..50 {
            limiter.allow(&format!("k{i}"), limit).await.expect("allow");
        }
        clock.advance(Duration::from_secs(2));
        limiter.allow("fresh", limit).await.expect("allow");
        assert_eq!(limiter.len(), 1, "only the fresh bucket remains");
    }

    #[tokio::test]
    async fn fixed_window_starts_at_the_first_hit() {
        let (limiter, clock) = limiter(100, WhenFull::EvictSoonest);
        let limit = Limit::new(2, Duration::from_secs(10)).expect("valid limit");
        limiter.allow("k", limit).await.expect("allow");
        clock.advance(Duration::from_secs(4));
        limiter.allow("k", limit).await.expect("allow");
        let denied = limiter.allow("k", limit).await.expect("allow");
        assert!(!denied.allowed);
        assert_eq!(
            denied.retry_after,
            Duration::from_secs(6),
            "window started at the first hit"
        );
        clock.advance(Duration::from_secs(6));
        assert!(
            limiter.allow("k", limit).await.expect("allow").allowed,
            "window resets when it ends"
        );
    }

    #[tokio::test]
    async fn counters_saturate_instead_of_overflowing() {
        let (limiter, _clock) = limiter(10, WhenFull::EvictSoonest);
        let limit = Limit::new(1, Duration::from_secs(60)).expect("valid limit");
        for _ in 0..1000 {
            limiter.allow("k", limit).await.expect("allow");
        }
        let state = limiter.lock();
        assert_eq!(state.buckets.get("k").map(|b| b.hits), Some(2), "N + 1");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_hits_never_exceed_the_limit_and_spawn_nothing() {
        let limiter = Arc::new(MemoryLimiter::new(MemoryOptions::default()).expect("valid"));
        let limit = Limit::new(25, Duration::from_secs(60)).expect("valid limit");
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..16 {
            let limiter = Arc::clone(&limiter);
            tasks.spawn(async move {
                let mut allowed = 0_u32;
                for _ in 0..10 {
                    if limiter.allow("shared", limit).await.expect("allow").allowed {
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
        assert_eq!(total, 25, "160 concurrent hits allow exactly the limit");
    }

    #[test]
    fn options_are_validated() {
        for max_keys in [0, MAX_MEMORY_KEYS + 1] {
            let options = MemoryOptions::default().with_max_keys(max_keys);
            assert!(matches!(
                MemoryLimiter::new(options),
                Err(LimitError::Configuration(_))
            ));
        }
        let config = RateLimitConfig { max_keys: 7 };
        assert_eq!(MemoryOptions::from_config(&config).max_keys, 7);
    }
}
