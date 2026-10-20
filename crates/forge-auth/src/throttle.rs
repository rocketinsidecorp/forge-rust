//! Throttling of credential guessing (sign-in and password-reset attempts).
//!
//! Every syntactically valid attempt is counted before credentials are
//! checked, against three counters: the account (the canonical login the
//! attempt names, whatever else it names), the network source (the socket
//! peer address, IPv6 by its /64 prefix; forwarded headers are not trusted)
//! and the pair of both. The pair has the tight limit, so a guesser is
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
//! A horizontally scaled deployment needs a shared [`LoginThrottle`]
//! implementation; counters of one process do not limit another.

use std::{
    collections::HashMap,
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
    /// or not it exists; `source` is the socket peer address. IPv6 sources
    /// count per /64 prefix (one subscriber's allocation), IPv4-mapped IPv6
    /// addresses as their IPv4 address.
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
}

impl Bucket {
    fn blocking(&self) -> bool {
        self.count >= self.limit
    }
}

type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;
type BucketKey = (Scope, [u8; 32]);

/// Bounded in-memory [`LoginThrottle`] for one process.
pub struct MemoryLoginThrottle {
    config: ThrottleConfig,
    clock: Clock,
    buckets: Mutex<HashMap<BucketKey, Bucket>>,
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
            buckets: Mutex::new(HashMap::new()),
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

    fn decide(&self, key: &ThrottleKey) -> Result<ThrottleDecision, ThrottleError> {
        let now = (self.clock)();
        let mut buckets = self.buckets.lock().map_err(|_| ThrottleError)?;
        buckets.retain(|_, bucket| now < bucket.resets_at);
        let keys = self.keys(key);

        let retry_after = keys
            .iter()
            .filter_map(|(bucket_key, _)| {
                buckets
                    .get(bucket_key)
                    .filter(|bucket| bucket.blocking())
                    .map(|bucket| bucket.resets_at.saturating_duration_since(now))
            })
            .max();
        if let Some(retry_after) = retry_after {
            return Ok(ThrottleDecision::Throttled { retry_after });
        }

        let missing = keys
            .iter()
            .filter(|(bucket_key, _)| !buckets.contains_key(bucket_key))
            .count();
        let overflow = (buckets.len() + missing).saturating_sub(self.config.max_entries);
        if overflow > 0 && !evict(&mut buckets, &keys, overflow) {
            return Ok(ThrottleDecision::Throttled {
                retry_after: self.config.window,
            });
        }

        let resets_at = now + self.config.window;
        for (bucket_key, limit) in keys {
            buckets
                .entry(bucket_key)
                .or_insert(Bucket {
                    count: 0,
                    limit,
                    resets_at,
                })
                .count += 1;
        }
        Ok(ThrottleDecision::Allowed)
    }
}

/// Removes the `count` oldest counters that block nothing and are not part
/// of the current attempt. `false` (nothing removed) when there are not
/// enough of them.
fn evict(
    buckets: &mut HashMap<BucketKey, Bucket>,
    current: &[(BucketKey, u32)],
    count: usize,
) -> bool {
    let mut candidates: Vec<(Instant, BucketKey)> = buckets
        .iter()
        .filter(|(bucket_key, bucket)| {
            !bucket.blocking() && !current.iter().any(|(key, _)| key == *bucket_key)
        })
        .map(|(bucket_key, bucket)| (bucket.resets_at, *bucket_key))
        .collect();
    if candidates.len() < count {
        return false;
    }
    candidates.sort_unstable_by_key(|(resets_at, _)| *resets_at);
    for (_, bucket_key) in candidates.into_iter().take(count) {
        buckets.remove(&bucket_key);
    }
    true
}

#[async_trait]
impl LoginThrottle for MemoryLoginThrottle {
    async fn attempt(&self, key: &ThrottleKey) -> Result<ThrottleDecision, ThrottleError> {
        self.decide(key)
    }

    async fn success(&self, key: &ThrottleKey) -> Result<(), ThrottleError> {
        let mut buckets = self.buckets.lock().map_err(|_| ThrottleError)?;
        buckets.remove(&(Scope::Account, key.account));
        if let Some(pair) = key.pair {
            buckets.remove(&(Scope::Pair, pair));
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
