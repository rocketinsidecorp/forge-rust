//! Throttling of credential guessing (sign-in and password-reset attempts).
//!
//! Every syntactically valid attempt is counted before credentials are
//! checked, against three counters: the account (the canonical login the
//! attempt names, whatever else it names), the network source (the client
//! address the HTTP server resolved, `Request::client_ip`: the socket peer
//! unless it is a trusted proxy listed in `FORGE_HTTP_TRUSTED_PROXIES`, ADR
//! 0032; IPv6 by its /64 prefix) and the pair of both. The pair has the tight limit, so a guesser is
//! stopped quickly; the account alone has a higher cap, so guessing from
//! many sources is still bounded while one source cannot lock a known login
//! out for everyone; the source alone bounds password spraying. Keys are
//! SHA-256 digests, so the limiter never stores a login or an address.
//!
//! [`MemoryLoginThrottle`] is the bounded in-process default: one window per
//! key and at most [`ThrottleConfig::max_entries`] keys. When the table is
//! full it evicts the oldest counters that are not blocking anything, and
//! refuses only when every counter is blocking, so filling the table from
//! many sources neither flushes a blocked attacker nor locks everyone out.
//! Counters are indexed in expiry order, so an attempt removes only the
//! counters that expired since the last one and never scans the table under
//! its lock (Forge-Go's `MemoryLoginThrottler` keeps a creation-order list).
//! A horizontally scaled deployment needs a shared [`LoginThrottle`]
//! implementation; counters of one process do not limit another.

use std::{
    collections::{BTreeMap, HashMap, hash_map::Entry},
    net::IpAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use sha2::{Digest, Sha256};
use thiserror::Error;

/// Digest identity of one attempt: the account it names, its network source
/// when known, and the pair of both.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct ThrottleKey {
    account: [u8; 32],
    source: Option<[u8; 32]>,
    pair: Option<[u8; 32]>,
}

impl ThrottleKey {
    /// Derives the key of an attempt.
    ///
    /// `scope` separates independent limits (for example `"sign_in"` and
    /// `"password_reset"`); `account` is the canonical identity the attempt
    /// names (such as a [`CanonicalLogin`](crate::CanonicalLogin)), whether
    /// or not it exists; `source` is the client address
    /// (`forge_http::Request::client_ip`), never a header the caller reads
    /// itself. IPv6 sources count per /64 prefix (one subscriber's
    /// allocation), IPv4-mapped IPv6 addresses as their IPv4 address.
    #[must_use]
    pub fn new(scope: &str, account: &str, source: Option<IpAddr>) -> Self {
        let account_digest = digest(&[b"account", scope.as_bytes(), account.as_bytes()]);
        let source = source.map(source_text);
        Self {
            account: account_digest,
            source: source
                .as_ref()
                .map(|text| digest(&[b"source", scope.as_bytes(), text.as_bytes()])),
            pair: source.as_ref().map(|text| {
                digest(&[
                    b"pair",
                    scope.as_bytes(),
                    account.as_bytes(),
                    text.as_bytes(),
                ])
            }),
        }
    }
}

/// The text a source is counted by: an IPv4 address, or an IPv6 /64.
fn source_text(address: IpAddr) -> String {
    match address {
        IpAddr::V4(address) => address.to_string(),
        IpAddr::V6(address) => match address.to_ipv4_mapped() {
            Some(address) => address.to_string(),
            None => {
                let segments = address.segments();
                format!(
                    "{:x}:{:x}:{:x}:{:x}::/64",
                    segments[0], segments[1], segments[2], segments[3]
                )
            }
        },
    }
}

impl std::fmt::Debug for ThrottleKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ThrottleKey([REDACTED])")
    }
}

fn digest(parts: &[&[u8]]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    for part in parts {
        // Length-prefix every part so ("ab", "c") and ("a", "bc") differ.
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part);
    }
    hasher.finalize().into()
}

/// Whether an attempt may proceed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ThrottleDecision {
    /// The attempt was counted and may check credentials.
    Allowed,
    /// Too many attempts: refuse without checking credentials, and tell the
    /// client to wait `retry_after` (HTTP `429` with `Retry-After`).
    Throttled {
        /// Time until the blocking window ends.
        retry_after: Duration,
    },
}

/// The throttle could not decide; callers must refuse the attempt.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[error("attempt throttle unavailable")]
pub struct ThrottleError;

/// Counts attempts and decides whether one may proceed.
#[async_trait]
pub trait LoginThrottle: Send + Sync {
    /// Counts one attempt for `key`, or refuses it when the pair, the
    /// account or the source exhausted its limit in the current window.
    async fn attempt(&self, key: &ThrottleKey) -> Result<ThrottleDecision, ThrottleError>;

    /// Records a successful attempt: the pair's and the account's counters
    /// are cleared, the source's is not, so one good password does not reset
    /// the pressure of a source that sprays many accounts.
    async fn success(&self, key: &ThrottleKey) -> Result<(), ThrottleError>;
}

/// Limits of [`MemoryLoginThrottle`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ThrottleConfig {
    /// Length of one counting window.
    pub window: Duration,
    /// Attempts per account and source pair and window.
    pub pair_limit: u32,
    /// Attempts per account and window, from all sources together.
    pub account_limit: u32,
    /// Attempts per network source and window.
    pub source_limit: u32,
    /// Most keys held at once.
    pub max_entries: usize,
}

impl Default for ThrottleConfig {
    /// In 15 minutes: 5 attempts per account and source, 50 per account,
    /// 300 per source; at most 20,000 keys.
    fn default() -> Self {
        Self {
            window: Duration::from_secs(15 * 60),
            pair_limit: 5,
            account_limit: 50,
            source_limit: 300,
            max_entries: 20_000,
        }
    }
}

/// Invalid [`ThrottleConfig`].
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[error("throttle window, limits and capacity must be positive (capacity at least 3)")]
pub struct ThrottleConfigError;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum Scope {
    Pair,
    Account,
    Source,
}

#[derive(Clone, Copy, Debug)]
struct Bucket {
    count: u32,
    limit: u32,
    resets_at: Instant,
    /// Creation sequence: with `resets_at`, the counter's place in
    /// [`Table::order`].
    sequence: u64,
}

impl Bucket {
    fn blocking(&self) -> bool {
        self.count >= self.limit
    }

    fn order_key(&self) -> (Instant, u64) {
        (self.resets_at, self.sequence)
    }
}

type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;
type BucketKey = (Scope, [u8; 32]);

/// The counters and an index of them in expiry order, so expired counters
/// are removed from the front and the oldest idle ones found without
/// scanning the whole table under the lock.
#[derive(Default)]
struct Table {
    buckets: HashMap<BucketKey, Bucket>,
    /// Every counter by `(resets_at, sequence)`: oldest (soonest to expire)
    /// first.
    order: BTreeMap<(Instant, u64), BucketKey>,
    /// Counters at or over their limit.
    blocking: usize,
    next_sequence: u64,
}

impl Table {
    fn len(&self) -> usize {
        self.buckets.len()
    }

    fn remove(&mut self, key: &BucketKey) {
        if let Some(bucket) = self.buckets.remove(key) {
            self.order.remove(&bucket.order_key());
            if bucket.blocking() {
                self.blocking = self.blocking.saturating_sub(1);
            }
        }
    }

    /// Removes the counters whose window ended, from the front of the
    /// expiry order: the cost is the number removed (each in `O(log n)`),
    /// never the size of the table.
    fn expire(&mut self, now: Instant) {
        while let Some((&(resets_at, _), &key)) = self.order.first_key_value() {
            if now < resets_at {
                return;
            }
            self.remove(&key);
        }
    }

    /// The live counter of `key`; one whose window ended is removed first.
    fn live(&mut self, key: &BucketKey, now: Instant) -> Option<&Bucket> {
        if self
            .buckets
            .get(key)
            .is_some_and(|bucket| now >= bucket.resets_at)
        {
            self.remove(key);
        }
        self.buckets.get(key)
    }

    /// Counts one attempt on `key`, creating its counter (window from `now`)
    /// when it has none.
    fn count(&mut self, key: BucketKey, limit: u32, resets_at: Instant) {
        let bucket = match self.buckets.entry(key) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => {
                let sequence = self.next_sequence;
                self.next_sequence = self.next_sequence.wrapping_add(1);
                self.order.insert((resets_at, sequence), key);
                entry.insert(Bucket {
                    count: 0,
                    limit,
                    resets_at,
                    sequence,
                })
            }
        };
        let was_blocking = bucket.blocking();
        bucket.count = bucket.count.saturating_add(1);
        if !was_blocking && bucket.blocking() {
            self.blocking += 1;
        }
    }

    /// Removes the `count` oldest counters that block nothing and are not
    /// part of the current attempt. `false` (nothing removed) when there are
    /// not enough of them. The walk from the oldest counter passes only
    /// blocking and current counters before it finds the victims.
    fn evict(&mut self, current: &[(BucketKey, u32)], count: usize) -> bool {
        let victims: Vec<BucketKey> = self
            .order
            .values()
            .filter(|bucket_key| {
                !current.iter().any(|(key, _)| key == *bucket_key)
                    && self
                        .buckets
                        .get(*bucket_key)
                        .is_some_and(|bucket| !bucket.blocking())
            })
            .take(count)
            .copied()
            .collect();
        if victims.len() < count {
            return false;
        }
        for victim in &victims {
            self.remove(victim);
        }
        true
    }
}

/// Bounded in-memory [`LoginThrottle`] for one process.
pub struct MemoryLoginThrottle {
    config: ThrottleConfig,
    clock: Clock,
    table: Mutex<Table>,
}

impl std::fmt::Debug for MemoryLoginThrottle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MemoryLoginThrottle")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl MemoryLoginThrottle {
    /// Creates a throttle with `config` and the monotonic system clock.
    pub fn new(config: ThrottleConfig) -> Result<Self, ThrottleConfigError> {
        Self::with_clock(config, Instant::now)
    }

    /// Creates a throttle reading time from `clock` (for tests).
    pub fn with_clock(
        config: ThrottleConfig,
        clock: impl Fn() -> Instant + Send + Sync + 'static,
    ) -> Result<Self, ThrottleConfigError> {
        if config.window.is_zero()
            || config.pair_limit == 0
            || config.account_limit == 0
            || config.source_limit == 0
            || config.max_entries < 3
        {
            return Err(ThrottleConfigError);
        }
        Ok(Self {
            config,
            clock: Arc::new(clock),
            table: Mutex::new(Table::default()),
        })
    }

    fn keys(&self, key: &ThrottleKey) -> Vec<(BucketKey, u32)> {
        let mut keys = vec![((Scope::Account, key.account), self.config.account_limit)];
        if let Some(pair) = key.pair {
            keys.push(((Scope::Pair, pair), self.config.pair_limit));
        }
        if let Some(source) = key.source {
            keys.push(((Scope::Source, source), self.config.source_limit));
        }
        keys
    }

    /// Decides one attempt. The work under the lock is proportional to the
    /// counters that expired since the last attempt and, when the table is
    /// full, to the blocking counters older than the evicted ones; it never
    /// scans every counter.
    fn decide(&self, key: &ThrottleKey) -> Result<ThrottleDecision, ThrottleError> {
        let now = (self.clock)();
        let mut table = self.table.lock().map_err(|_| ThrottleError)?;
        table.expire(now);
        let keys = self.keys(key);

        let mut retry_after = None;
        let mut missing = 0;
        let mut present = 0;
        for (bucket_key, _) in &keys {
            match table.live(bucket_key, now) {
                None => missing += 1,
                Some(bucket) => {
                    present += 1;
                    if bucket.blocking() {
                        let wait = bucket.resets_at.saturating_duration_since(now);
                        retry_after = retry_after.max(Some(wait));
                    }
                }
            }
        }
        if let Some(retry_after) = retry_after {
            return Ok(ThrottleDecision::Throttled { retry_after });
        }

        let overflow = (table.len() + missing).saturating_sub(self.config.max_entries);
        // Every present counter of this attempt is idle (a blocking one was
        // refused above) and must stay, so only the others can go.
        if overflow > 0
            && (table.len() - table.blocking - present < overflow || !table.evict(&keys, overflow))
        {
            return Ok(ThrottleDecision::Throttled {
                retry_after: self.config.window,
            });
        }

        let resets_at = now + self.config.window;
        for (bucket_key, limit) in keys {
            table.count(bucket_key, limit, resets_at);
        }
        Ok(ThrottleDecision::Allowed)
    }
}

#[async_trait]
impl LoginThrottle for MemoryLoginThrottle {
    async fn attempt(&self, key: &ThrottleKey) -> Result<ThrottleDecision, ThrottleError> {
        self.decide(key)
    }

    async fn success(&self, key: &ThrottleKey) -> Result<(), ThrottleError> {
        let mut table = self.table.lock().map_err(|_| ThrottleError)?;
        table.remove(&(Scope::Account, key.account));
        if let Some(pair) = key.pair {
            table.remove(&(Scope::Pair, pair));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv6Addr;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    fn throttle(config: ThrottleConfig) -> (MemoryLoginThrottle, Arc<AtomicU64>) {
        let start = Instant::now();
        let offset = Arc::new(AtomicU64::new(0));
        let clock = Arc::clone(&offset);
        let throttle = MemoryLoginThrottle::with_clock(config, move || {
            start + Duration::from_secs(clock.load(Ordering::SeqCst))
        })
        .expect("valid config");
        (throttle, offset)
    }

    fn config() -> ThrottleConfig {
        ThrottleConfig {
            window: Duration::from_secs(60),
            pair_limit: 2,
            account_limit: 4,
            source_limit: 6,
            max_entries: 30,
        }
    }

    fn source(last: u8) -> Option<IpAddr> {
        Some(IpAddr::from([192, 0, 2, last]))
    }

    async fn allowed(throttle: &MemoryLoginThrottle, key: &ThrottleKey) -> bool {
        throttle.attempt(key).await == Ok(ThrottleDecision::Allowed)
    }

    #[tokio::test]
    async fn a_pair_is_throttled_after_its_limit_until_the_window_ends() {
        let (throttle, offset) = throttle(config());
        let key = ThrottleKey::new("sign_in", "ada@example.com", source(1));

        for _ in 0..2 {
            assert!(allowed(&throttle, &key).await);
        }
        offset.store(10, Ordering::SeqCst);
        assert_eq!(
            throttle.attempt(&key).await,
            Ok(ThrottleDecision::Throttled {
                retry_after: Duration::from_secs(50)
            })
        );
        offset.store(60, Ordering::SeqCst);
        assert!(allowed(&throttle, &key).await);
    }

    #[tokio::test]
    async fn one_source_cannot_lock_a_login_out_for_other_sources() {
        let (throttle, _) = throttle(config());
        let attacker = ThrottleKey::new("sign_in", "owner@example.com", source(66));
        for _ in 0..2 {
            assert!(allowed(&throttle, &attacker).await);
        }
        assert!(!allowed(&throttle, &attacker).await);
        let owner = ThrottleKey::new("sign_in", "owner@example.com", source(1));
        assert!(allowed(&throttle, &owner).await, "the owner still signs in");
    }

    #[tokio::test]
    async fn guessing_one_login_from_many_sources_is_capped() {
        let (throttle, _) = throttle(config());
        for last in 1..=4 {
            let key = ThrottleKey::new("sign_in", "ada@example.com", source(last));
            assert!(allowed(&throttle, &key).await, "{last}");
        }
        let fifth = ThrottleKey::new("sign_in", "ada@example.com", source(5));
        assert!(!allowed(&throttle, &fifth).await);
    }

    #[tokio::test]
    async fn a_source_is_throttled_across_accounts_and_success_keeps_its_count() {
        let (throttle, _) = throttle(config());
        for account in ["a", "b", "c", "d", "e", "f"] {
            let key = ThrottleKey::new("sign_in", account, source(1));
            assert!(allowed(&throttle, &key).await, "{account}");
            throttle.success(&key).await.expect("success");
        }
        let seventh = ThrottleKey::new("sign_in", "g", source(1));
        assert!(!allowed(&throttle, &seventh).await);
        let elsewhere = ThrottleKey::new("sign_in", "g", source(2));
        assert!(allowed(&throttle, &elsewhere).await);
    }

    #[tokio::test]
    async fn success_clears_the_pair_and_account_counters() {
        let (throttle, _) = throttle(config());
        let key = ThrottleKey::new("sign_in", "ada", source(1));
        throttle.attempt(&key).await.expect("attempt");
        throttle.attempt(&key).await.expect("attempt");
        throttle.success(&key).await.expect("success");
        assert!(allowed(&throttle, &key).await);
    }

    #[tokio::test]
    async fn ipv6_sources_count_per_slash_64() {
        let (throttle, _) = throttle(config());
        let address = |last: u16| {
            Some(IpAddr::V6(Ipv6Addr::new(
                0x2001, 0xdb8, 1, 2, 0, 0, 0, last,
            )))
        };
        for (index, account) in ["a", "b", "c", "d", "e", "f"].iter().enumerate() {
            let rotated = address(u16::try_from(index).unwrap_or_default());
            assert!(allowed(&throttle, &ThrottleKey::new("sign_in", account, rotated)).await);
        }
        let same_prefix = ThrottleKey::new("sign_in", "g", address(999));
        assert!(
            !allowed(&throttle, &same_prefix).await,
            "rotating within a /64 is one source"
        );
        let other_prefix = Some(IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 1, 3, 0, 0, 0, 1)));
        assert!(allowed(&throttle, &ThrottleKey::new("sign_in", "g", other_prefix)).await);
        let mapped = Some(IpAddr::V6(Ipv6Addr::new(
            0, 0, 0, 0, 0, 0xffff, 0xc000, 0x0201,
        )));
        assert_eq!(
            ThrottleKey::new("sign_in", "x", mapped),
            ThrottleKey::new("sign_in", "x", source(1))
        );
    }

    #[tokio::test]
    async fn scopes_are_independent() {
        let (throttle, _) = throttle(config());
        let sign_in = ThrottleKey::new("sign_in", "ada", source(1));
        let reset = ThrottleKey::new("password_reset", "ada", source(1));
        for _ in 0..2 {
            throttle.attempt(&sign_in).await.expect("attempt");
        }
        assert!(allowed(&throttle, &reset).await);
    }

    #[tokio::test]
    async fn a_full_table_evicts_idle_counters_and_keeps_blocking_ones() {
        let (throttle, offset) = throttle(ThrottleConfig {
            max_entries: 6,
            ..config()
        });
        // The attacker's pair is blocking (2 of 2).
        let attacker = ThrottleKey::new("sign_in", "victim", source(66));
        throttle.attempt(&attacker).await.expect("attempt");
        throttle.attempt(&attacker).await.expect("attempt");
        // Flooding from many sources fills the table but locks nobody out.
        for last in 1..=40 {
            offset.store(u64::from(last), Ordering::SeqCst);
            let filler = ThrottleKey::new("sign_in", &format!("filler{last}"), source(last));
            assert!(allowed(&throttle, &filler).await, "{last}");
        }
        assert!(
            !allowed(&throttle, &attacker).await,
            "the blocking pair was kept"
        );
        let owner = ThrottleKey::new("sign_in", "owner", source(200));
        assert!(allowed(&throttle, &owner).await);
    }

    #[tokio::test]
    async fn a_table_full_of_blocking_counters_refuses() {
        let (throttle, _) = throttle(ThrottleConfig {
            account_limit: 1,
            max_entries: 3,
            ..config()
        });
        for name in ["a", "b", "c"] {
            assert!(allowed(&throttle, &ThrottleKey::new("sign_in", name, None)).await);
        }
        assert_eq!(
            throttle
                .attempt(&ThrottleKey::new("sign_in", "d", None))
                .await,
            Ok(ThrottleDecision::Throttled {
                retry_after: Duration::from_secs(60)
            })
        );
    }

    /// The table's index and blocking count agree with its counters.
    fn consistent(throttle: &MemoryLoginThrottle) -> usize {
        let table = throttle.table.lock().expect("table");
        assert_eq!(table.order.len(), table.buckets.len());
        for (order_key, bucket_key) in &table.order {
            assert_eq!(
                table.buckets.get(bucket_key).map(Bucket::order_key),
                Some(*order_key)
            );
        }
        assert_eq!(
            table.blocking,
            table
                .buckets
                .values()
                .filter(|bucket| bucket.blocking())
                .count()
        );
        table.len()
    }

    #[tokio::test]
    async fn expired_counters_leave_from_the_front_and_the_index_stays_consistent() {
        let (throttle, offset) = throttle(config());
        for last in 1..=5 {
            offset.store(u64::from(last), Ordering::SeqCst);
            let key = ThrottleKey::new("sign_in", &format!("user{last}"), source(last));
            assert!(allowed(&throttle, &key).await);
        }
        // One pair blocking (2 of 2), then cleared by a success.
        let blocked = ThrottleKey::new("sign_in", "user1", source(1));
        assert!(allowed(&throttle, &blocked).await);
        assert!(!allowed(&throttle, &blocked).await);
        assert_eq!(consistent(&throttle), 15);
        throttle.success(&blocked).await.expect("success");
        assert_eq!(consistent(&throttle), 13);

        // The first three attempts' windows end; only theirs are removed.
        offset.store(63, Ordering::SeqCst);
        let late = ThrottleKey::new("sign_in", "late", source(9));
        assert!(allowed(&throttle, &late).await);
        assert_eq!(consistent(&throttle), 2 * 3 + 3);
        // The rest expire; a key whose window ended starts over.
        offset.store(200, Ordering::SeqCst);
        assert!(allowed(&throttle, &late).await);
        assert_eq!(consistent(&throttle), 3);
    }

    #[tokio::test]
    async fn a_full_table_refuses_without_evicting_when_only_blocking_counters_remain() {
        let (throttle, _) = throttle(ThrottleConfig {
            account_limit: 1,
            max_entries: 4,
            ..config()
        });
        for name in ["a", "b", "c"] {
            assert!(allowed(&throttle, &ThrottleKey::new("sign_in", name, None)).await);
        }
        // One idle slot left: a new account fits, the next does not, and the
        // blocking counters are all kept.
        assert!(allowed(&throttle, &ThrottleKey::new("sign_in", "d", None)).await);
        assert!(!allowed(&throttle, &ThrottleKey::new("sign_in", "e", None)).await);
        assert_eq!(consistent(&throttle), 4);
        assert!(!allowed(&throttle, &ThrottleKey::new("sign_in", "a", None)).await);
    }

    #[test]
    fn invalid_configuration_is_rejected_and_keys_are_redacted() {
        for invalid in [
            ThrottleConfig {
                window: Duration::ZERO,
                ..config()
            },
            ThrottleConfig {
                pair_limit: 0,
                ..config()
            },
            ThrottleConfig {
                account_limit: 0,
                ..config()
            },
            ThrottleConfig {
                source_limit: 0,
                ..config()
            },
            ThrottleConfig {
                max_entries: 2,
                ..config()
            },
        ] {
            assert!(MemoryLoginThrottle::new(invalid).is_err());
        }
        let key = ThrottleKey::new("sign_in", "ada@example.com", source(1));
        assert_eq!(format!("{key:?}"), "ThrottleKey([REDACTED])");
        assert_ne!(
            ThrottleKey::new("a", "bc", None),
            ThrottleKey::new("ab", "c", None)
        );
        assert_eq!(ThrottleConfig::default().pair_limit, 5);
        assert_eq!(ThrottleConfig::default().account_limit, 50);
    }
}
