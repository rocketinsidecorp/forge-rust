//! SQLite shared limiter. SQLite serves one application instance on one host,
//! so the shared state is what several processes of that host (for example a
//! web server and a worker) need, and the application clock is the only
//! clock. Each hit is one atomic upsert under SQLite's write lock.

use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use forge_db::SqliteDatabase;
use sqlx::{Row, migrate::Migrator};

use crate::{
    Decision, Limit, LimitError, Limiter, MAX_PRUNE_ROUNDS, PruneGate, Pruner, StoreOptions,
    check_key,
    store::{decide, store_error, window_micros},
};

/// The `forge_rate_limits` migration. Combine it with the application's own
/// through [`forge_db::combine_migrations`].
pub static MIGRATIONS: Migrator = sqlx::migrate!("migrations/sqlite");

/// A [`Limiter`] and [`Pruner`] on SQLite. Safe for concurrent use; starts no
/// task.
#[derive(Debug)]
pub struct SqliteLimiter {
    db: SqliteDatabase,
    options: StoreOptions,
    gate: PruneGate,
}

impl SqliteLimiter {
    /// A limiter on `db`; the table comes from [`MIGRATIONS`].
    pub fn new(db: SqliteDatabase, options: StoreOptions) -> Result<Self, LimitError> {
        Ok(Self {
            db,
            options: options.validated()?,
            gate: PruneGate::new(),
        })
    }
}

fn now_micros() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_micros()).unwrap_or(i64::MAX)
        })
}

#[async_trait]
impl Limiter for SqliteLimiter {
    async fn allow(&self, key: &str, limit: Limit) -> Result<Decision, LimitError> {
        check_key(key)?;
        let now = now_micros();
        // In an upsert every right-hand side reads the row as it was before the
        // statement, so both columns see the same expiry.
        let row = sqlx::query(
            "INSERT INTO forge_rate_limits (bucket, hits, expires_at) VALUES (?1, 1, ?2) \
             ON CONFLICT (bucket) DO UPDATE SET \
               hits = CASE WHEN expires_at <= ?3 THEN 1 ELSE hits + 1 END, \
               expires_at = CASE WHEN expires_at <= ?3 THEN ?2 ELSE expires_at END \
             RETURNING hits, expires_at",
        )
        .bind(key)
        .bind(now.saturating_add(window_micros(limit)))
        .bind(now)
        .fetch_one(self.db.pool())
        .await
        .map_err(store_error)?;
        let hits: i64 = row.try_get(0).map_err(store_error)?;
        let expires_at: i64 = row.try_get(1).map_err(store_error)?;
        if self.gate.due(self.options.interval(), self.options.batch()) {
            let _ = self.prune().await;
        }
        Ok(decide(hits, expires_at.saturating_sub(now), limit))
    }
}

#[async_trait]
impl Pruner for SqliteLimiter {
    async fn prune(&self) -> Result<u64, LimitError> {
        let batch = i64::from(self.options.batch());
        let now = now_micros();
        let mut removed = 0_u64;
        for _ in 0..MAX_PRUNE_ROUNDS {
            let result = sqlx::query(
                "DELETE FROM forge_rate_limits WHERE bucket IN (\
                   SELECT bucket FROM forge_rate_limits WHERE expires_at <= ?1 \
                   ORDER BY expires_at LIMIT ?2)",
            )
            .bind(now)
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
