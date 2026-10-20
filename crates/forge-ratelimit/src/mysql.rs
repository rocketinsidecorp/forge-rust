//! MySQL and MariaDB shared limiter. A hit is one transaction: an
//! `INSERT ... ON DUPLICATE KEY UPDATE` that leaves the bucket's row locked,
//! then a read of that row, so the count a caller sees is its own even under
//! concurrency. All window arithmetic uses the database clock
//! (`UTC_TIMESTAMP`), so replicas with skewed clocks agree.

use async_trait::async_trait;
use forge_db::{DbError, MySqlDatabase};
use sqlx::{Row, migrate::Migrator};

use crate::{
    Decision, Limit, LimitError, Limiter, MAX_PRUNE_ROUNDS, PruneGate, Pruner, StoreOptions,
    check_key,
    store::{decide, store_error, window_micros},
};

/// The `forge_rate_limits` migration. Combine it with the application's own
/// through [`forge_db::combine_migrations`].
pub static MIGRATIONS: Migrator = sqlx::migrate!("migrations/mysql");

/// A transaction that lost a deadlock is rerun this many times.
const HIT_ATTEMPTS: u32 = 3;

/// A [`Limiter`] and [`Pruner`] on MySQL 8.4+ or MariaDB 11.4+. Safe for
/// concurrent use; starts no task.
#[derive(Debug)]
pub struct MySqlLimiter {
    db: MySqlDatabase,
    options: StoreOptions,
    gate: PruneGate,
}

impl MySqlLimiter {
    /// A limiter on `db`; the table comes from [`MIGRATIONS`].
    pub fn new(db: MySqlDatabase, options: StoreOptions) -> Result<Self, LimitError> {
        Ok(Self {
            db,
            options: options.validated()?,
            gate: PruneGate::new(),
        })
    }

    async fn count(&self, key: &str, micros: i64) -> Result<(i64, i64), sqlx::Error> {
        let mut transaction = self.db.pool().begin().await?;
        // Assignments run left to right and both read the expiry as stored
        // before this statement changes it: `hits` is assigned first,
        // `expires_at` second, and neither reads the other's column.
        sqlx::query(
            "INSERT INTO forge_rate_limits (bucket, hits, expires_at) \
             VALUES (?, 1, DATE_ADD(UTC_TIMESTAMP(6), INTERVAL ? MICROSECOND)) \
             ON DUPLICATE KEY UPDATE \
               hits = IF(expires_at <= UTC_TIMESTAMP(6), 1, hits + 1), \
               expires_at = IF(expires_at <= UTC_TIMESTAMP(6), \
                   DATE_ADD(UTC_TIMESTAMP(6), INTERVAL ? MICROSECOND), expires_at)",
        )
        .bind(key)
        .bind(micros)
        .bind(micros)
        .execute(&mut *transaction)
        .await?;
        // The upsert holds the row's lock until commit: this read is our own
        // write, not a later hit's.
        let row = sqlx::query(
            "SELECT hits, TIMESTAMPDIFF(MICROSECOND, UTC_TIMESTAMP(6), expires_at) \
             FROM forge_rate_limits WHERE bucket = ?",
        )
        .bind(key)
        .fetch_one(&mut *transaction)
        .await?;
        let counted = (row.try_get(0)?, row.try_get(1)?);
        transaction.commit().await?;
        Ok(counted)
    }
}

#[async_trait]
impl Limiter for MySqlLimiter {
    async fn allow(&self, key: &str, limit: Limit) -> Result<Decision, LimitError> {
        check_key(key)?;
        let micros = window_micros(limit);
        let mut attempt = 1;
        let (hits, retry_micros) = loop {
            match self.count(key, micros).await {
                Ok(counted) => break counted,
                Err(error) => {
                    let error = DbError::from(error);
                    if attempt < HIT_ATTEMPTS && matches!(error, DbError::SerializationFailure) {
                        attempt += 1;
                        continue;
                    }
                    return Err(LimitError::Store(Box::new(error)));
                }
            }
        };
        if self.gate.due(self.options.interval(), self.options.batch()) {
            let _ = self.prune().await;
        }
        Ok(decide(hits, retry_micros, limit))
    }
}

#[async_trait]
impl Pruner for MySqlLimiter {
    async fn prune(&self) -> Result<u64, LimitError> {
        let batch = i64::from(self.options.batch());
        let mut removed = 0_u64;
        for _ in 0..MAX_PRUNE_ROUNDS {
            let result = sqlx::query(
                "DELETE FROM forge_rate_limits WHERE expires_at <= UTC_TIMESTAMP(6) \
                 ORDER BY expires_at LIMIT ?",
            )
            .bind(batch)
            .execute(self.db.pool())
            .await
            .map_err(store_error)?;
            removed += result.rows_affected();
            if result.rows_affected() < u64::from(self.options.batch()) {
                break;
            }
        }
        Ok(removed)
    }
}
