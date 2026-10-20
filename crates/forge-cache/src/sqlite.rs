//! SQLite shared cache. SQLite serves one application instance on one host, so
//! the shared state is what several processes of that host need, and the
//! application clock is the only clock.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use forge_db::{MAX_PRUNE_ROUNDS, PruneGate, SqliteDatabase};
use sqlx::{Row, migrate::Migrator};

use crate::{
    Cache, CacheError, Flight, Pruner, StoreOptions, check_key,
    store::{key_hash, store_error, ttl_micros},
};

/// The `forge_cache_entries` migration. Combine it with the application's own
/// through [`forge_db::combine_migrations`].
pub static MIGRATIONS: Migrator = sqlx::migrate!("migrations/sqlite");

/// A [`Cache`] and [`Pruner`] on SQLite. Safe for concurrent use; starts no
/// task.
#[derive(Debug)]
pub struct SqliteCache {
    db: SqliteDatabase,
    options: StoreOptions,
    flight: Flight,
    gate: PruneGate,
}

impl SqliteCache {
    /// A cache on `db`; the table comes from [`MIGRATIONS`].
    pub fn new(db: SqliteDatabase, options: StoreOptions) -> Result<Self, CacheError> {
        Ok(Self {
            db,
            options: options.validated()?,
            flight: Flight::default(),
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
impl Cache for SqliteCache {
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, CacheError> {
        check_key(key)?;
        let row = sqlx::query(
            "SELECT value FROM forge_cache_entries WHERE key_hash = ?1 AND expires_at > ?2",
        )
        .bind(key_hash(key).as_slice())
        .bind(now_micros())
        .fetch_optional(self.db.pool())
        .await
        .map_err(store_error)?;
        row.map(|row| row.try_get(0).map_err(store_error))
            .transpose()
    }

    async fn set(&self, key: &str, value: &[u8], ttl: Option<Duration>) -> Result<(), CacheError> {
        let ttl = self.options.options().check_set(key, value, ttl)?;
        let now = now_micros();
        sqlx::query(
            "INSERT INTO forge_cache_entries (key_hash, value, created_at, expires_at) \
             VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT (key_hash) DO UPDATE SET \
               value = excluded.value, created_at = excluded.created_at, expires_at = excluded.expires_at",
        )
        .bind(key_hash(key).as_slice())
        .bind(value)
        .bind(now)
        .bind(now.saturating_add(ttl_micros(ttl)))
        .execute(self.db.pool())
        .await
        .map_err(store_error)?;
        if self.gate.due(self.options.interval(), self.options.batch()) {
            let _ = self.prune().await;
        }
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<(), CacheError> {
        check_key(key)?;
        sqlx::query("DELETE FROM forge_cache_entries WHERE key_hash = ?1")
            .bind(key_hash(key).as_slice())
            .execute(self.db.pool())
            .await
            .map_err(store_error)?;
        Ok(())
    }

    fn flight(&self) -> &Flight {
        &self.flight
    }
}

#[async_trait]
impl Pruner for SqliteCache {
    async fn prune(&self) -> Result<u64, CacheError> {
        let batch = i64::from(self.options.batch());
        let now = now_micros();
        let mut removed = 0_u64;
        for _ in 0..MAX_PRUNE_ROUNDS {
            let result = sqlx::query(
                "DELETE FROM forge_cache_entries WHERE key_hash IN (\
                   SELECT key_hash FROM forge_cache_entries WHERE expires_at <= ?1 \
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

        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM forge_cache_entries")
            .fetch_one(self.db.pool())
            .await
            .map_err(store_error)?;
        let mut excess = count.saturating_sub(
            i64::try_from(self.options.options().max_entries()).unwrap_or(i64::MAX),
        );
        for _ in 0..MAX_PRUNE_ROUNDS {
            if excess <= 0 {
                break;
            }
            let result = sqlx::query(
                "DELETE FROM forge_cache_entries WHERE key_hash IN (\
                   SELECT key_hash FROM forge_cache_entries ORDER BY created_at LIMIT ?1)",
            )
            .bind(excess.min(batch))
            .execute(self.db.pool())
            .await
            .map_err(store_error)?;
            if result.rows_affected() == 0 {
                break;
            }
            removed += result.rows_affected();
            excess -= i64::try_from(result.rows_affected()).unwrap_or(i64::MAX);
        }
        Ok(removed)
    }
}
