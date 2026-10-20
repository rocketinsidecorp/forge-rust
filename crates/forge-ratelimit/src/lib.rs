//! Fixed-window rate limiting with fail-closed defaults (ADR 0027).
//!
//! A [`Limiter`] counts hits per key in fixed windows. The window of a key
//! starts with its first hit and lasts [`Limit::per`]; at most [`Limit::n`] hits
//! are allowed in it. Fixed windows are cheap and atomic on every database, at
//! the price of the known boundary effect: a client that times its requests
//! around a window edge can send up to `2N` within one window. Choose `N`
//! accordingly.
//!
//! - [`MemoryLimiter`] is bounded and process-local, with no background task.
//! - `PostgresLimiter`, `MySqlLimiter` and `SqliteLimiter` (features
//!   `postgres`, `mysql`, `sqlite`) share state across replicas through one
//!   atomic statement per hit and prune expired buckets inline in bounded
//!   batches.
//! - [`RateLimit`] is the HTTP [`Middleware`](forge_http::Middleware) and
//!   handler wrapper: `429` with `Retry-After` over the limit, `503` when the
//!   limiter itself fails (unless [`RateLimit::fail_open`] is chosen).
//!
//! What identifies a client is the application's decision (a key function).
//! [`by_client_ip`], the client address resolved under the trusted-proxy
//! policy (`FORGE_HTTP_TRUSTED_PROXIES`, ADR 0032), is the usual choice;
//! [`by_remote_addr`] is always the TCP peer.
//!
//! ```
//! use std::{sync::Arc, time::Duration};
//! use forge_ratelimit::{Limit, MemoryLimiter, MemoryOptions, RateLimit, by_client_ip};
//!
//! # fn main() -> Result<(), forge_ratelimit::LimitError> {
//! let limiter = Arc::new(MemoryLimiter::new(MemoryOptions::default())?);
//! // Rails: rate_limit to: 10, within: 3.minutes, only: :create
//! let create = RateLimit::new(
//!     limiter,
//!     "comments.create",
//!     Limit::new(10, Duration::from_secs(180))?,
//!     by_client_ip,
//! )?;
//! # let _ = create;
//! # Ok(())
//! # }
//! ```

#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use std::{error::Error as StdError, time::Duration};

use async_trait::async_trait;
use sha2::{Digest, Sha256};
use thiserror::Error;

mod memory;
mod middleware;
#[cfg(feature = "mysql")]
pub mod mysql;
#[cfg(feature = "postgres")]
pub mod postgres;
#[cfg(feature = "sqlite")]
pub mod sqlite;
#[cfg(any(feature = "mysql", feature = "postgres", feature = "sqlite"))]
mod store;
#[cfg(feature = "testing")]
pub mod testing;

#[cfg(any(feature = "mysql", feature = "postgres", feature = "sqlite"))]
pub use forge_db::{MAX_PRUNE_ROUNDS, PruneGate};
pub use memory::{MemoryLimiter, MemoryOptions, WhenFull};
pub use middleware::{KeyFn, RateLimit, RateLimited, by_client_ip, by_remote_addr, join};
#[cfg(any(feature = "mysql", feature = "postgres", feature = "sqlite"))]
pub use store::StoreOptions;

/// Largest [`Limit::n`].
pub const MAX_N: u32 = 1 << 30;
/// Shortest window.
pub const MIN_PER: Duration = Duration::from_millis(1);
/// Longest window.
pub const MAX_PER: Duration = Duration::from_secs(30 * 24 * 3600);
/// Longest key a [`Limiter`] accepts. Keys made by [`key`] are 64 characters.
pub const MAX_KEY_LENGTH: usize = 128;

/// Why a limiter, limit or key was refused, or a store failed.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum LimitError {
    /// The key is empty, too long, or not printable ASCII.
    #[error("rate limit key must be 1 to 128 printable ASCII characters")]
    InvalidKey,
    /// `N` is outside `1..=2^30`.
    #[error("rate limit N must be between 1 and 1073741824")]
    InvalidN,
    /// The window is outside `1ms..=30 days`.
    #[error("rate limit window must be between 1ms and 30 days")]
    InvalidWindow,
    /// A limiter option is outside its bounds.
    #[error("invalid rate limiter configuration: {0}")]
    Configuration(&'static str),
    /// The store could not decide. The source never contains SQL, keys or
    /// credentials.
    #[error("rate limit store failed")]
    Store(#[source] Box<dyn StdError + Send + Sync>),
}

/// `n` hits allowed per window of length `per`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Limit {
    n: u32,
    per: Duration,
}

impl Limit {
    /// Validates and builds a limit (`n` in `1..=2^30`, `per` in
    /// `1ms..=30 days`). Every store therefore sees the same inputs.
    pub fn new(n: u32, per: Duration) -> Result<Self, LimitError> {
        if !(1..=MAX_N).contains(&n) {
            return Err(LimitError::InvalidN);
        }
        if !(MIN_PER..=MAX_PER).contains(&per) {
            return Err(LimitError::InvalidWindow);
        }
        Ok(Self { n, per })
    }

    /// Hits allowed per window.
    #[must_use]
    pub const fn n(&self) -> u32 {
        self.n
    }

    /// Window length.
    #[must_use]
    pub const fn per(&self) -> Duration {
        self.per
    }
}

/// The outcome of one hit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct Decision {
    /// `false` when the hit exceeded the limit.
    pub allowed: bool,
    /// Hits left in the current window; zero when denied.
    pub remaining: u32,
    /// Time until the window ends and hits are allowed again; positive only
    /// when denied.
    pub retry_after: Duration,
}

impl Decision {
    /// An allowed hit with `remaining` hits left.
    #[must_use]
    pub const fn allowed(remaining: u32) -> Self {
        Self {
            allowed: true,
            remaining,
            retry_after: Duration::ZERO,
        }
    }

    /// A denied hit; `retry_after` is raised to at least one millisecond.
    #[must_use]
    pub fn denied(retry_after: Duration) -> Self {
        Self {
            allowed: false,
            remaining: 0,
            retry_after: retry_after.max(MIN_PER),
        }
    }
}

/// Counts one hit for a key and reports whether it is within the limit.
///
/// Keys are opaque ASCII tokens of at most [`MAX_KEY_LENGTH`] characters;
/// derive them with [`key`], which also keeps personal data (addresses, e-mail
/// addresses) out of shared stores. Every call counts as a hit, denied or not,
/// so a client cannot probe for free. Implementations are safe for concurrent
/// use. An error means the limiter could not decide: callers choose whether to
/// fail closed (the middleware's default) or open.
#[async_trait]
pub trait Limiter: Send + Sync {
    /// Counts a hit for `key` against `limit`.
    async fn allow(&self, key: &str, limit: Limit) -> Result<Decision, LimitError>;
}

/// Implemented by limiters with persistent storage, which keep expired
/// buckets until pruned.
///
/// Each call removes at most [`MAX_PRUNE_ROUNDS`] bounded batches and returns
/// how many buckets it removed; call it periodically if inline pruning is
/// disabled (the stores also prune opportunistically after hits).
#[async_trait]
pub trait Pruner: Send + Sync {
    /// Removes expired buckets and returns how many.
    async fn prune(&self) -> Result<u64, LimitError>;
}

/// Validates a store key. Limiter implementations call it first so that every
/// store rejects the same inputs.
pub fn check_key(key: &str) -> Result<(), LimitError> {
    if key.is_empty()
        || key.len() > MAX_KEY_LENGTH
        || !key.bytes().all(|byte| (0x21..0x7f).contains(&byte))
    {
        return Err(LimitError::InvalidKey);
    }
    Ok(())
}

/// Whole seconds a `Retry-After` header carries: rounded up, at least 1.
#[must_use]
pub fn retry_after_seconds(retry_after: Duration) -> u64 {
    let seconds = retry_after.as_secs() + u64::from(retry_after.subsec_nanos() > 0);
    seconds.max(1)
}

/// Derives a store key from a scope (what is limited, such as a route) and the
/// parts that identify the client.
///
/// The result is a SHA-256 digest in hex: fixed length, free of personal data,
/// and unambiguous because every part is length-prefixed (`["a", "bc"]` and
/// `["ab", "c"]` differ).
///
/// ```
/// assert_eq!(forge_ratelimit::key("login", &["203.0.113.7"]).len(), 64);
/// assert_ne!(
///     forge_ratelimit::key("s", &["a", "bc"]),
///     forge_ratelimit::key("s", &["ab", "c"]),
/// );
/// ```
#[must_use]
pub fn key(scope: &str, parts: &[&str]) -> String {
    fn write(hasher: &mut Sha256, value: &str) {
        hasher.update((value.len() as u64).to_be_bytes());
        hasher.update(value.as_bytes());
    }
    let mut hasher = Sha256::new();
    hasher.update(b"forge.ratelimit.v1");
    write(&mut hasher, scope);
    for part in parts {
        write(&mut hasher, part);
    }
    hasher
        .finalize()
        .iter()
        .flat_map(|byte| {
            [
                char::from(HEX[usize::from(byte >> 4)]),
                char::from(HEX[usize::from(byte & 0x0f)]),
            ]
        })
        .collect()
}

const HEX: &[u8; 16] = b"0123456789abcdef";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_are_validated_by_construction() {
        assert!(Limit::new(1, MIN_PER).is_ok());
        assert!(Limit::new(MAX_N, MAX_PER).is_ok());
        assert!(matches!(
            Limit::new(0, Duration::from_secs(1)),
            Err(LimitError::InvalidN)
        ));
        assert!(matches!(
            Limit::new(MAX_N + 1, Duration::from_secs(1)),
            Err(LimitError::InvalidN)
        ));
        for per in [
            Duration::ZERO,
            Duration::from_micros(999),
            MAX_PER + Duration::from_secs(1),
        ] {
            assert!(matches!(Limit::new(1, per), Err(LimitError::InvalidWindow)));
        }
    }

    #[test]
    fn keys_are_checked() {
        for bad in ["", "a b", "café", "a\u{0}b", "a\tb", &"k".repeat(129)] {
            assert!(
                matches!(check_key(bad), Err(LimitError::InvalidKey)),
                "{bad:?}"
            );
        }
        assert!(check_key(&"k".repeat(128)).is_ok());
        assert!(check_key(&key("s", &["x"])).is_ok());
    }

    #[test]
    fn derived_keys_are_deterministic_unambiguous_and_opaque() {
        assert_eq!(key("s", &["a", "bc"]), key("s", &["a", "bc"]));
        assert_ne!(key("s", &["a", "bc"]), key("s", &["ab", "c"]));
        assert_ne!(key("ab", &["c"]), key("a", &["bc"]));
        assert_ne!(key("s", &[]), key("s", &[""]));
        assert_ne!(key("s", &["x"]), key("t", &["x"]));
        let derived = key("login", &["203.0.113.7", "alice@example.com"]);
        assert_eq!(derived.len(), 64);
        assert!(derived.bytes().all(|b| b.is_ascii_hexdigit()));
        assert!(!derived.contains("203") && !derived.contains("alice"));
    }

    #[test]
    fn retry_after_rounds_up_and_is_at_least_one_second() {
        for (input, expected) in [
            (Duration::from_millis(1), 1),
            (Duration::from_secs(1), 1),
            (Duration::from_millis(1001), 2),
            (Duration::from_secs(90), 90),
            (Duration::from_millis(89_001), 90),
        ] {
            assert_eq!(retry_after_seconds(input), expected, "{input:?}");
        }
    }

    #[test]
    fn denied_decisions_always_carry_a_positive_retry_after() {
        assert_eq!(Decision::denied(Duration::ZERO).retry_after, MIN_PER);
        assert!(Decision::allowed(3).retry_after.is_zero());
    }
}
