//! Throttling of credential guessing (sign-in and password-reset attempts).
//!
//! Every syntactically valid attempt is counted before credentials are
//! checked, per account (the tenant and login the attempt names) and per
//! network source (the socket peer address; forwarded headers are not
//! trusted). Keys are SHA-256 digests, so the limiter never stores a login
//! or an address in plain text.
//!
//! [`MemoryLoginThrottle`] is the bounded in-process default: one window per
//! key, at most [`ThrottleConfig::max_entries`] keys, and it fails closed
//! (refuses attempts) instead of evicting live keys when it is full, so an
//! attacker cannot flush a victim's counter by filling the table. A
//! horizontally scaled deployment needs a shared [`LoginThrottle`]
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

/// Digest identity of one attempt: the account it names and, when known, its
/// network source.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct ThrottleKey {
    account: [u8; 32],
    source: Option<[u8; 32]>,
}

impl ThrottleKey {
    /// Derives the key of an attempt.
    ///
    /// `scope` separates independent limits (for example `"sign_in"` and
    /// `"password_reset"`); `account` is the normalized identity the attempt
    /// names (such as the tenant slug and lowercase login), whether or not
    /// it exists; `source` is the socket peer address.
    #[must_use]
    pub fn new(scope: &str, account: &str, source: Option<IpAddr>) -> Self {
        let account = digest(&[b"account", scope.as_bytes(), account.as_bytes()]);
        let source = source.map(|address| {
            let text = address.to_string();
            digest(&[b"source", scope.as_bytes(), text.as_bytes()])
        });
        Self { account, source }
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
    /// Counts one attempt for `key`, or refuses it when the account or the
    /// source exhausted its limit in the current window.
    async fn attempt(&self, key: &ThrottleKey) -> Result<ThrottleDecision, ThrottleError>;

    /// Records a successful attempt: the account's counter is cleared, the
    /// source's is not, so one good password does not reset the pressure of
    /// a source that sprays many accounts.
    async fn success(&self, key: &ThrottleKey) -> Result<(), ThrottleError>;
}

/// Limits of [`MemoryLoginThrottle`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ThrottleConfig {
    /// Length of one counting window.
    pub window: Duration,
    /// Attempts per account and window.
    pub account_limit: u32,
    /// Attempts per network source and window.
    pub source_limit: u32,
    /// Most keys held at once; beyond it attempts are refused.
    pub max_entries: usize,
}

impl Default for ThrottleConfig {
    /// 5 attempts per account and 300 per source in 15 minutes, at most
    /// 20,000 keys.
    fn default() -> Self {
        Self {
            window: Duration::from_secs(15 * 60),
            account_limit: 5,
            source_limit: 300,
            max_entries: 20_000,
        }
    }
}

/// Invalid [`ThrottleConfig`].
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[error("throttle window, limits and capacity must be positive (capacity at least 2)")]
pub struct ThrottleConfigError;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum Scope {
    Account,
    Source,
}

#[derive(Clone, Copy, Debug)]
struct Bucket {
    count: u32,
    resets_at: Instant,
}

type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;

/// Bounded in-memory [`LoginThrottle`] for one process.
pub struct MemoryLoginThrottle {
    config: ThrottleConfig,
    clock: Clock,
    buckets: Mutex<HashMap<(Scope, [u8; 32]), Bucket>>,
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
            || config.account_limit == 0
            || config.source_limit == 0
            || config.max_entries < 2
        {
            return Err(ThrottleConfigError);
        }
        Ok(Self {
            config,
            clock: Arc::new(clock),
            buckets: Mutex::new(HashMap::new()),
        })
    }

    fn decide(&self, key: &ThrottleKey) -> Result<ThrottleDecision, ThrottleError> {
        let now = (self.clock)();
        let mut buckets = self.buckets.lock().map_err(|_| ThrottleError)?;
        buckets.retain(|_, bucket| now < bucket.resets_at);

        let mut keys = vec![((Scope::Account, key.account), self.config.account_limit)];
        if let Some(source) = key.source {
            keys.push(((Scope::Source, source), self.config.source_limit));
        }

        let retry_after = keys
            .iter()
            .filter_map(|(bucket_key, limit)| {
                buckets
                    .get(bucket_key)
                    .filter(|bucket| bucket.count >= *limit)
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
        if buckets.len() + missing > self.config.max_entries {
            return Ok(ThrottleDecision::Throttled {
                retry_after: self.config.window,
            });
        }

        let resets_at = now + self.config.window;
        for (bucket_key, _) in keys {
            buckets
                .entry(bucket_key)
                .or_insert(Bucket {
                    count: 0,
                    resets_at,
                })
                .count += 1;
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
        self.buckets
            .lock()
            .map_err(|_| ThrottleError)?
            .remove(&(Scope::Account, key.account));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    struct FakeClock {
        start: Instant,
        offset_secs: Arc<AtomicU64>,
    }

    fn throttle(config: ThrottleConfig) -> (MemoryLoginThrottle, Arc<AtomicU64>) {
        let clock = FakeClock {
            start: Instant::now(),
            offset_secs: Arc::new(AtomicU64::new(0)),
        };
        let offset = Arc::clone(&clock.offset_secs);
        let throttle = MemoryLoginThrottle::with_clock(config, move || {
            clock.start + Duration::from_secs(clock.offset_secs.load(Ordering::SeqCst))
        })
        .expect("valid config");
        (throttle, offset)
    }

    fn config() -> ThrottleConfig {
        ThrottleConfig {
            window: Duration::from_secs(60),
            account_limit: 2,
            source_limit: 3,
            max_entries: 10,
        }
    }

    fn source(last: u8) -> Option<IpAddr> {
        Some(IpAddr::from([192, 0, 2, last]))
    }

    #[tokio::test]
    async fn an_account_is_throttled_after_its_limit_until_the_window_ends() {
        let (throttle, offset) = throttle(config());
        let key = ThrottleKey::new("sign_in", "acme\0ada@example.com", source(1));

        for _ in 0..2 {
            assert_eq!(throttle.attempt(&key).await, Ok(ThrottleDecision::Allowed));
        }
        offset.store(10, Ordering::SeqCst);
        assert_eq!(
            throttle.attempt(&key).await,
            Ok(ThrottleDecision::Throttled {
                retry_after: Duration::from_secs(50)
            })
        );
        offset.store(60, Ordering::SeqCst);
        assert_eq!(throttle.attempt(&key).await, Ok(ThrottleDecision::Allowed));
    }

    #[tokio::test]
    async fn a_source_is_throttled_across_accounts_and_success_keeps_its_count() {
        let (throttle, _) = throttle(config());
        for account in ["a", "b", "c"] {
            let key = ThrottleKey::new("sign_in", account, source(1));
            assert_eq!(throttle.attempt(&key).await, Ok(ThrottleDecision::Allowed));
            throttle.success(&key).await.expect("success");
        }
        let fourth = ThrottleKey::new("sign_in", "d", source(1));
        assert!(matches!(
            throttle.attempt(&fourth).await,
            Ok(ThrottleDecision::Throttled { .. })
        ));
        let elsewhere = ThrottleKey::new("sign_in", "d", source(2));
        assert_eq!(
            throttle.attempt(&elsewhere).await,
            Ok(ThrottleDecision::Allowed)
        );
    }

    #[tokio::test]
    async fn success_clears_the_account_counter() {
        let (throttle, _) = throttle(config());
        let key = ThrottleKey::new("sign_in", "acme\0ada", None);
        throttle.attempt(&key).await.expect("attempt");
        throttle.attempt(&key).await.expect("attempt");
        throttle.success(&key).await.expect("success");
        assert_eq!(throttle.attempt(&key).await, Ok(ThrottleDecision::Allowed));
    }

    #[tokio::test]
    async fn scopes_are_independent() {
        let (throttle, _) = throttle(config());
        let sign_in = ThrottleKey::new("sign_in", "acme\0ada", None);
        let reset = ThrottleKey::new("password_reset", "acme\0ada", None);
        for _ in 0..2 {
            throttle.attempt(&sign_in).await.expect("attempt");
        }
        assert_eq!(
            throttle.attempt(&reset).await,
            Ok(ThrottleDecision::Allowed)
        );
    }

    #[tokio::test]
    async fn a_full_table_refuses_new_keys_instead_of_evicting_live_ones() {
        let (throttle, offset) = throttle(ThrottleConfig {
            max_entries: 2,
            ..config()
        });
        let victim = ThrottleKey::new("sign_in", "victim", None);
        throttle.attempt(&victim).await.expect("attempt");
        throttle.attempt(&victim).await.expect("attempt");
        let filler = ThrottleKey::new("sign_in", "filler", None);
        assert_eq!(
            throttle.attempt(&filler).await,
            Ok(ThrottleDecision::Allowed)
        );
        let refused = ThrottleKey::new("sign_in", "another", None);
        assert_eq!(
            throttle.attempt(&refused).await,
            Ok(ThrottleDecision::Throttled {
                retry_after: Duration::from_secs(60)
            })
        );
        // The victim is still throttled; expired keys free the table.
        assert!(matches!(
            throttle.attempt(&victim).await,
            Ok(ThrottleDecision::Throttled { .. })
        ));
        offset.store(61, Ordering::SeqCst);
        assert_eq!(
            throttle.attempt(&refused).await,
            Ok(ThrottleDecision::Allowed)
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
                account_limit: 0,
                ..config()
            },
            ThrottleConfig {
                source_limit: 0,
                ..config()
            },
            ThrottleConfig {
                max_entries: 1,
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
    }
}
