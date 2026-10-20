//! Application cache, the equivalent of Rails' Solid Cache (ADR 0028): byte
//! values under string keys with a time to live, a bounded in-memory cache
//! ([`MemoryCache`]) and database-backed stores (`PostgresCache`,
//! `MySqlCache`, `SqliteCache`; features `postgres`, `mysql`, `sqlite`) that
//! every replica shares.
//!
//! A cache holds copies: it is an optimization the application must be able to
//! live without. Values are opaque bytes (use [`get_json`], [`set_json`] and
//! [`fetch_json`] for typed values), keys and values are never logged, and every
//! store is bounded: a maximum value size, a maximum number of entries, and
//! expiry. A miss is not an error.
//!
//! [`fetch`] is the read-through entry point. It protects the origin from a
//! stampede: concurrent callers that miss the same key in one process wait for
//! one computation instead of each running it. Replicas do not coordinate, so
//! across `N` replicas an expensive computation can run up to `N` times.
//!
//! Cached data is only as trustworthy as whoever can write the cache: do not
//! put the authorization decision of one user under a key another can choose,
//! and build keys with [`key`] (or [`tenant_key`] for tenant data).
//!
//! ```
//! use forge_cache::{Cache, MemoryCache, MemoryOptions, fetch, key};
//!
//! # #[tokio::main(flavor = "current_thread")]
//! # async fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let cache = MemoryCache::new(MemoryOptions::default())?;
//! let price = fetch(&cache, &key("price", &["sku-1"]), None, || async {
//!     Ok::<_, std::convert::Infallible>(b"19.90".to_vec())
//! })
//! .await?;
//! assert_eq!(price, b"19.90");
//! # Ok(())
//! # }
//! ```

#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use std::{error::Error as StdError, sync::Arc, time::Duration};

use async_trait::async_trait;
use forge_config::{CacheConfig, MAX_CACHE_VALUE_BYTES};
use forge_tenancy::TenantContext;
use thiserror::Error;

mod fetch;
mod flight;
mod memory;
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

pub use fetch::{FetchError, fetch, fetch_json, get_json, set_json};
pub use flight::Flight;
#[cfg(any(feature = "mysql", feature = "postgres", feature = "sqlite"))]
pub use forge_db::{MAX_PRUNE_ROUNDS, PruneGate};
pub use memory::{MemoryCache, MemoryOptions};
#[cfg(any(feature = "mysql", feature = "postgres", feature = "sqlite"))]
pub use store::StoreOptions;

/// Longest key in bytes.
pub const MAX_KEY_LENGTH: usize = 1024;
/// Largest `max_value_bytes` a store accepts: 8 MiB, well inside the
/// `MEDIUMBLOB` of MySQL/MariaDB and the packet limits of all three databases.
/// Cache big objects with `forge-storage` instead.
pub const MAX_VALUE_BYTES_LIMIT: usize = MAX_CACHE_VALUE_BYTES;

const DEFAULT_MAX_ENTRIES: usize = 10_000;
const DEFAULT_MAX_VALUE_BYTES: usize = 1 << 20;
const DEFAULT_TTL: Duration = Duration::from_secs(3600);
const MAX_ENTRIES_LIMIT: usize = 100_000_000;
const MIN_TTL: Duration = Duration::from_millis(1);
const MAX_TTL: Duration = Duration::from_secs(365 * 24 * 3600);

/// Why a cache operation was refused or failed.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum CacheError {
    /// The key is empty, longer than [`MAX_KEY_LENGTH`] bytes, or contains NUL.
    #[error("cache key must be 1 to 1024 bytes without NUL")]
    InvalidKey,
    /// The time to live is zero or exceeds one year (`None` selects the default).
    #[error("cache ttl must be between 1ms and one year")]
    InvalidTtl,
    /// The value exceeds `max_value_bytes`.
    #[error("cache value too large")]
    ValueTooLarge,
    /// An option is outside its bounds.
    #[error("invalid cache configuration: {0}")]
    Configuration(&'static str),
    /// The store failed. The source never contains SQL, keys or values.
    #[error("cache store failed")]
    Store(#[source] Box<dyn StdError + Send + Sync>),
}

/// Bounds shared by every cache.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Options {
    max_entries: usize,
    max_value_bytes: usize,
    default_ttl: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            max_entries: DEFAULT_MAX_ENTRIES,
            max_value_bytes: DEFAULT_MAX_VALUE_BYTES,
            default_ttl: DEFAULT_TTL,
        }
    }
}

impl Options {
    /// The options the `FORGE_CACHE_*` variables select.
    #[must_use]
    pub fn from_config(config: &CacheConfig) -> Self {
        Self {
            max_entries: config.max_entries,
            max_value_bytes: config.max_value_bytes,
            default_ttl: config.default_ttl,
        }
    }

    /// Bounds the number of entries (`1..=100_000_000`, default 10,000). The
    /// in-memory cache enforces it on every write; the database stores
    /// enforce it when pruning, so they can exceed it briefly between prunes.
    #[must_use]
    pub const fn with_max_entries(mut self, max_entries: usize) -> Self {
        self.max_entries = max_entries;
        self
    }

    /// Bounds one value (`1..=8 MiB`, default 1 MiB).
    #[must_use]
    pub const fn with_max_value_bytes(mut self, max_value_bytes: usize) -> Self {
        self.max_value_bytes = max_value_bytes;
        self
    }

    /// The time to live used when a write passes none (`1ms..=1 year`,
    /// default one hour).
    #[must_use]
    pub const fn with_default_ttl(mut self, default_ttl: Duration) -> Self {
        self.default_ttl = default_ttl;
        self
    }

    /// Entry bound.
    #[must_use]
    pub const fn max_entries(&self) -> usize {
        self.max_entries
    }

    /// Value size bound.
    #[must_use]
    pub const fn max_value_bytes(&self) -> usize {
        self.max_value_bytes
    }

    /// Default time to live.
    #[must_use]
    pub const fn default_ttl(&self) -> Duration {
        self.default_ttl
    }

    pub(crate) fn validated(self) -> Result<Self, CacheError> {
        if !(1..=MAX_ENTRIES_LIMIT).contains(&self.max_entries) {
            return Err(CacheError::Configuration(
                "cache max entries must be between 1 and 100000000",
            ));
        }
        if !(1..=MAX_VALUE_BYTES_LIMIT).contains(&self.max_value_bytes) {
            return Err(CacheError::Configuration(
                "cache max value bytes must be between 1 and 8388608",
            ));
        }
        if !(MIN_TTL..=MAX_TTL).contains(&self.default_ttl) {
            return Err(CacheError::Configuration(
                "cache default ttl must be between 1ms and one year",
            ));
        }
        Ok(self)
    }

    /// Validates the arguments of a write and returns the effective ttl. The
    /// error never contains the key or the value.
    pub fn check_set(
        &self,
        key: &str,
        value: &[u8],
        ttl: Option<Duration>,
    ) -> Result<Duration, CacheError> {
        check_key(key)?;
        if value.len() > self.max_value_bytes {
            return Err(CacheError::ValueTooLarge);
        }
        self.effective_ttl(ttl)
    }

    /// Resolves a caller's ttl: `None` is the default; zero or more than a
    /// year is an error; less than a millisecond becomes a millisecond.
    pub fn effective_ttl(&self, ttl: Option<Duration>) -> Result<Duration, CacheError> {
        match ttl {
            None => Ok(self.default_ttl),
            Some(ttl) if ttl.is_zero() || ttl > MAX_TTL => Err(CacheError::InvalidTtl),
            Some(ttl) => Ok(ttl.max(MIN_TTL)),
        }
    }
}

/// Validates a key. Stores call it first so they all reject the same inputs.
pub fn check_key(key: &str) -> Result<(), CacheError> {
    if key.is_empty() || key.len() > MAX_KEY_LENGTH || key.bytes().any(|byte| byte == 0) {
        return Err(CacheError::InvalidKey);
    }
    Ok(())
}

/// Stores byte values under keys for a limited time.
///
/// Implementations are safe for concurrent use and copy values on the way in
/// and out. A miss is `Ok(None)`. Use [`fetch`] for read-through with stampede
/// protection.
#[async_trait]
pub trait Cache: Send + Sync {
    /// The value stored under `key`, or `None` when there is none or it
    /// expired.
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, CacheError>;

    /// Stores `value` under `key` for `ttl` (`None` selects the default),
    /// replacing any previous value.
    async fn set(&self, key: &str, value: &[u8], ttl: Option<Duration>) -> Result<(), CacheError>;

    /// Removes `key`. Deleting a missing key is not an error.
    async fn delete(&self, key: &str) -> Result<(), CacheError>;

    /// The per-process stampede protection [`fetch`] uses for this cache.
    fn flight(&self) -> &Flight;
}

#[async_trait]
impl<T: Cache + ?Sized> Cache for Arc<T> {
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, CacheError> {
        (**self).get(key).await
    }

    async fn set(&self, key: &str, value: &[u8], ttl: Option<Duration>) -> Result<(), CacheError> {
        (**self).set(key, value, ttl).await
    }

    async fn delete(&self, key: &str) -> Result<(), CacheError> {
        (**self).delete(key).await
    }

    fn flight(&self) -> &Flight {
        (**self).flight()
    }
}

/// Implemented by caches with persistent storage, which keep expired and
/// surplus entries until pruned.
///
/// Each call removes at most [`MAX_PRUNE_ROUNDS`] bounded batches of each kind
/// (expired entries first, then, above `max_entries`, the oldest writes) and
/// returns how many entries it removed. The stores also prune inline after
/// writes.
#[async_trait]
pub trait Pruner: Send + Sync {
    /// Removes expired and surplus entries and returns how many.
    async fn prune(&self) -> Result<u64, CacheError>;
}

/// Joins a namespace and parts into one unambiguous key: the namespace and
/// every part are length-prefixed (`"2:ns:3:abc"`), so `("a", "bc")` and
/// `("ab", "c")` never collide, a namespace built from a dynamic value
/// (`"tenant:"+id`) cannot be confused with a shorter namespace plus parts,
/// and a part that comes from a user cannot reach into another namespace.
///
/// ```
/// use forge_cache::key;
/// assert_eq!(key("users", &["42", "profile"]), "5:users:2:42:7:profile");
/// assert_ne!(key("ns", &["a", "bc"]), key("ns", &["ab", "c"]));
/// ```
#[must_use]
pub fn key(namespace: &str, parts: &[&str]) -> String {
    let mut key = String::with_capacity(
        namespace.len() + parts.iter().map(|part| part.len() + 8).sum::<usize>() + 8,
    );
    key.push_str(&namespace.len().to_string());
    key.push(':');
    key.push_str(namespace);
    for part in parts {
        key.push(':');
        key.push_str(&part.len().to_string());
        key.push(':');
        key.push_str(part);
    }
    key
}

/// Like [`key`], scoped to the tenant of an authorized [`TenantContext`]: two
/// tenants never share a key, whatever namespace and parts they pass. The
/// cache is not tenant-aware, so tenant data must always be keyed this way.
///
/// The namespace `"forge.tenant"` is reserved for these keys; do not build
/// other keys with it.
#[must_use]
pub fn tenant_key(tenant: &TenantContext, namespace: &str, parts: &[&str]) -> String {
    let tenant_id = tenant.tenant_id().to_string();
    let mut scoped: Vec<&str> = Vec::with_capacity(parts.len() + 2);
    scoped.push(&tenant_id);
    scoped.push(namespace);
    scoped.extend_from_slice(parts);
    key("forge.tenant", &scoped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_unambiguous() {
        assert_ne!(key("ns", &["a", "bc"]), key("ns", &["ab", "c"]));
        assert_ne!(key("ns", &["x:1:y"]), key("ns", &["x", "y"]));
        assert!(check_key(&key("users", &["42", "profile"])).is_ok());
    }

    #[test]
    fn the_namespace_is_length_prefixed_too() {
        // A dynamic namespace ("tenant:" + id) must never equal a shorter
        // namespace plus parts, or one tenant could read another's entries.
        let pairs = [
            (key("a:1:b", &[]), key("a", &["b"])),
            (key("tenant:7", &[]), key("tenant", &["7"])),
            (key("a:1:b", &["c"]), key("a", &["b", "c"])),
            (key("", &[]), key("", &[""])),
            (key("1:a", &[]), key("1", &["a"])),
        ];
        for (index, (left, right)) in pairs.iter().enumerate() {
            assert_ne!(left, right, "pair {index}");
        }
        let mut seen = std::collections::HashMap::new();
        for namespace in ["", "a", "a:1", "a:1:b", "1:a", "tenant:7"] {
            for parts in [
                &[][..],
                &[""],
                &["a"],
                &["b"],
                &["a", "b"],
                &["1:a"],
                &["", ""],
            ] {
                let identity = (namespace, parts.to_vec());
                let derived = key(namespace, parts);
                if let Some(previous) = seen.insert(derived.clone(), identity.clone()) {
                    assert_eq!(previous, identity, "collision on {derived:?}");
                }
            }
        }
    }

    #[test]
    fn invalid_keys_are_rejected_without_echoing_them() {
        for bad in ["", "a\0b", &"k".repeat(MAX_KEY_LENGTH + 1)] {
            let error = check_key(bad).expect_err("invalid key");
            assert!(matches!(error, CacheError::InvalidKey));
            assert!(bad.is_empty() || !error.to_string().contains(bad));
        }
        assert!(check_key(&"k".repeat(MAX_KEY_LENGTH)).is_ok());
    }

    #[test]
    fn ttl_rules() {
        let options = Options::default();
        assert_eq!(options.effective_ttl(None).expect("default"), DEFAULT_TTL);
        assert_eq!(
            options
                .effective_ttl(Some(Duration::from_micros(10)))
                .expect("raised"),
            MIN_TTL
        );
        for bad in [Duration::ZERO, MAX_TTL + Duration::from_secs(1)] {
            assert!(matches!(
                options.effective_ttl(Some(bad)),
                Err(CacheError::InvalidTtl)
            ));
        }
        assert!(options.effective_ttl(Some(MAX_TTL)).is_ok());
    }

    #[test]
    fn options_are_validated() {
        for bad in [
            Options::default().with_max_entries(0),
            Options::default().with_max_entries(MAX_ENTRIES_LIMIT + 1),
            Options::default().with_max_value_bytes(0),
            Options::default().with_max_value_bytes(MAX_VALUE_BYTES_LIMIT + 1),
            Options::default().with_default_ttl(Duration::ZERO),
            Options::default().with_default_ttl(MAX_TTL + Duration::from_secs(1)),
        ] {
            assert!(matches!(bad.validated(), Err(CacheError::Configuration(_))));
        }
        assert!(Options::default().validated().is_ok());
    }

    #[test]
    fn check_set_enforces_key_value_and_ttl() {
        let options = Options::default().with_max_value_bytes(8);
        assert!(options.check_set("k", &[0; 8], None).is_ok());
        assert!(matches!(
            options.check_set("k", &[0; 9], None),
            Err(CacheError::ValueTooLarge)
        ));
        assert!(matches!(
            options.check_set("", &[], None),
            Err(CacheError::InvalidKey)
        ));
        assert!(matches!(
            options.check_set("k", &[], Some(Duration::ZERO)),
            Err(CacheError::InvalidTtl)
        ));
    }
}
