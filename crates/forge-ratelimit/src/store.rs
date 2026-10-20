//! Parts shared by the database stores.

use std::time::Duration;

use forge_db::DbError;

use crate::{Decision, Limit, LimitError};

/// Tuning of a database-backed limiter.
///
/// Expired buckets are removed in bounded batches. A store also prunes inline
/// after a hit has been decided, **after every `prune_batch` hits or every
/// `prune_interval`, whichever comes first**, ignoring errors: expired buckets
/// cannot pile up faster than they are removed (a time-only trigger let a
/// stream of distinct keys do exactly that), tables stay small with no
/// scheduler and no background task.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StoreOptions {
    prune_interval: Option<Duration>,
    prune_batch: u32,
}

const MAX_PRUNE_BATCH: u32 = 100_000;

impl Default for StoreOptions {
    fn default() -> Self {
        Self {
            prune_interval: Some(Duration::from_secs(60)),
            prune_batch: 500,
        }
    }
}

impl StoreOptions {
    /// Longest time between inline prunes; `None` disables inline pruning
    /// (then call [`Pruner::prune`](crate::Pruner::prune) yourself). Default:
    /// one minute.
    #[must_use]
    pub const fn with_prune_interval(mut self, interval: Option<Duration>) -> Self {
        self.prune_interval = interval;
        self
    }

    /// Size of one prune batch (`1..=100_000`, default 500); one prune runs at
    /// most [`MAX_PRUNE_ROUNDS`](crate::MAX_PRUNE_ROUNDS) batches.
    #[must_use]
    pub const fn with_prune_batch(mut self, batch: u32) -> Self {
        self.prune_batch = batch;
        self
    }

    pub(crate) fn validated(self) -> Result<Self, LimitError> {
        if !(1..=MAX_PRUNE_BATCH).contains(&self.prune_batch) {
            return Err(LimitError::Configuration(
                "rate limit prune batch must be between 1 and 100000",
            ));
        }
        Ok(self)
    }

    pub(crate) const fn interval(&self) -> Option<Duration> {
        self.prune_interval
    }

    pub(crate) const fn batch(&self) -> u32 {
        self.prune_batch
    }
}

/// Maps a driver failure to a [`LimitError`] whose text never contains SQL,
/// keys or credentials ([`DbError`] guarantees that).
pub(crate) fn store_error(error: sqlx::Error) -> LimitError {
    LimitError::Store(Box::new(DbError::from(error)))
}

/// Window length in microseconds, as every store computes expiry.
pub(crate) fn window_micros(limit: Limit) -> i64 {
    i64::try_from(limit.per().as_micros()).unwrap_or(i64::MAX)
}

/// Turns the stored hit count and the time left in the window into a decision.
pub(crate) fn decide(hits: i64, retry_micros: i64, limit: Limit) -> Decision {
    if hits > i64::from(limit.n()) {
        let micros = u64::try_from(retry_micros).unwrap_or(0);
        return Decision::denied(Duration::from_micros(micros));
    }
    let remaining = i64::from(limit.n()).saturating_sub(hits);
    Decision::allowed(u32::try_from(remaining).unwrap_or(0))
}
