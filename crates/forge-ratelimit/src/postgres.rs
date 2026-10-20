//! PostgreSQL shared limiter: one atomic `INSERT ... ON CONFLICT` per hit with
//! all window arithmetic on the database clock, so replicas with skewed clocks
//! agree and a hit can neither be lost nor counted twice.

use async_trait::async_trait;
use forge_db::PgDatabase;
use sqlx::{Row, migrate::Migrator};

use crate::{
    Decision, Limit, LimitError, Limiter, MAX_PRUNE_ROUNDS, PruneGate, Pruner, StoreOptions,
    check_key,
    store::{decide, store_error, window_micros},
};

/// The `forge_rate_limits` migration. Combine it with the application's own
/// through [`forge_db::combine_migrations`].
pub static MIGRATIONS: Migrator = sqlx::migrate!("migrations/postgres");

/// A [`Limiter`] and [`Pruner`] on PostgreSQL. Safe for concurrent use; starts
/// no task.
#[derive(Debug)]
pub struct PostgresLimiter {
    db: PgDatabase,
    options: StoreOptions,
    gate: PruneGate,
}

impl PostgresLimiter {
    /// A limiter on `db`; the table comes from [`MIGRATIONS`].
    pub fn new(db: PgDatabase, options: StoreOptions) -> Result<Self, LimitError> {
        Ok(Self {
            db,
            options: options.validated()?,
            gate: PruneGate::new(),
        })
    }
}

#[async_trait]
impl Limiter for PostgresLimiter {
    async fn allow(&self, key: &str, limit: Limit) -> Result<Decision, LimitError> {
        check_key(key)?;
        // now() is the statement's start time, so every comparison in the
        // statement sees the same instant. An expired bucket restarts at 1.
        let row = sqlx::query(
            "INSERT INTO forge_rate_limits AS b (bucket, hits, expires_at) \
             VALUES ($1, 1, now() + ($2::bigint * interval '1 microsecond')) \
             ON CONFLICT (bucket) DO UPDATE SET \
               hits = CASE WHEN b.expires_at <= now() THEN 1 ELSE b.hits + 1 END, \
               expires_at = CASE WHEN b.expires_at <= now() THEN EXCLUDED.expires_at ELSE b.expires_at END \
             RETURNING b.hits, (extract(epoch FROM (b.expires_at - now())) * 1000000)::bigint",
        )
        .bind(key)
        .bind(window_micros(limit))
        .fetch_one(self.db.pool())
        .await
        .map_err(store_error)?;
        let hits: i64 = row.try_get(0).map_err(store_error)?;
        let retry_micros: i64 = row.try_get(1).map_err(store_error)?;
        // Prune after the hit has been decided; its outcome never changes it.
        if self.gate.due(self.options.interval(), self.options.batch()) {
            let _ = self.prune().await;
        }
        Ok(decide(hits, retry_micros, limit))
    }
}

#[async_trait]
impl Pruner for PostgresLimiter {
    async fn prune(&self) -> Result<u64, LimitError> {
        let batch = i64::from(self.options.batch());
        let mut removed = 0_u64;
        for _ in 0..MAX_PRUNE_ROUNDS {
            let result = sqlx::query(
                "DELETE FROM forge_rate_limits WHERE bucket IN (\
                   SELECT bucket FROM forge_rate_limits \
                   WHERE expires_at <= clock_timestamp() \
                   ORDER BY expires_at LIMIT $1 FOR UPDATE SKIP LOCKED)",
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
