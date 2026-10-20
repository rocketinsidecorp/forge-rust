//! Parts shared by the database stores.

use std::time::Duration;

use forge_db::DbError;
use sha2::{Digest, Sha256};

use crate::{CacheError, Options};

/// Tuning of a database-backed cache: the shared [`Options`] plus pruning.
///
/// Expired entries and, above `max_entries`, the oldest writes (FIFO by write
/// time, like Solid Cache, which does not track reads) are removed in bounded
/// batches. A store also prunes inline after a write has succeeded, **after
/// every `prune_batch` writes or every `prune_interval`, whichever comes
/// first**, ignoring errors. A time-only trigger was rejected: eviction capped
/// at one batch per interval let a stream of new keys grow the table to
/// thousands of rows over `max_entries`. With the write counter the bound is
/// *soft but tight*: the table stays within about `prune_batch` entries per
/// writing process (plus writes in flight) of `max_entries`. Each prune counts
/// the table, a scan; with millions of rows, raise `prune_batch`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StoreOptions {
    options: Options,
    prune_interval: Option<Duration>,
    prune_batch: u32,
}

const MAX_PRUNE_BATCH: u32 = 100_000;

impl Default for StoreOptions {
    fn default() -> Self {
        Self {
            options: Options::default(),
            prune_interval: Some(Duration::from_secs(60)),
            prune_batch: 500,
        }
    }
}

impl StoreOptions {
    /// The shared bounds (entries, value size, default ttl).
    #[must_use]
    pub const fn with_options(mut self, options: Options) -> Self {
        self.options = options;
        self
    }

    /// Longest time between inline prunes; `None` disables inline pruning
    /// (then call [`Pruner::prune`](crate::Pruner::prune) yourself). Default:
    /// one minute.
    #[must_use]
    pub const fn with_prune_interval(mut self, interval: Option<Duration>) -> Self {
        self.prune_interval = interval;
        self
    }

    /// Size of one prune batch (`1..=100_000`, default 500); one prune runs at
    /// most [`MAX_PRUNE_ROUNDS`](crate::MAX_PRUNE_ROUNDS) batches of each kind.
    #[must_use]
    pub const fn with_prune_batch(mut self, batch: u32) -> Self {
        self.prune_batch = batch;
        self
    }

    pub(crate) fn validated(self) -> Result<Self, CacheError> {
        if !(1..=MAX_PRUNE_BATCH).contains(&self.prune_batch) {
            return Err(CacheError::Configuration(
                "cache prune batch must be between 1 and 100000",
            ));
        }
        Ok(Self {
            options: self.options.validated()?,
            ..self
        })
    }

    pub(crate) const fn options(&self) -> &Options {
        &self.options
    }

    pub(crate) const fn interval(&self) -> Option<Duration> {
        self.prune_interval
    }

    pub(crate) const fn batch(&self) -> u32 {
        self.prune_batch
    }
}

/// The row address of a key: keys are never stored, indexed or logged.
pub(crate) fn key_hash(key: &str) -> [u8; 32] {
    Sha256::digest(key.as_bytes()).into()
}

/// Maps a driver failure to a [`CacheError`] whose text never contains SQL,
/// keys or values ([`DbError`] guarantees that).
pub(crate) fn store_error(error: sqlx::Error) -> CacheError {
    CacheError::Store(Box::new(DbError::from(error)))
}

/// Ttl in microseconds, as every store computes expiry.
pub(crate) fn ttl_micros(ttl: Duration) -> i64 {
    i64::try_from(ttl.as_micros()).unwrap_or(i64::MAX)
}
