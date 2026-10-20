//! PostgreSQL shared cache (the Solid Cache equivalent): entries live in a
//! table addressed by the SHA-256 of the key, all expiry arithmetic uses the
//! database clock, and pruning removes expired entries and, above
//! `max_entries`, the oldest writes, in bounded batches.

use std::time::Duration;

use async_trait::async_trait;
use forge_db::{MAX_PRUNE_ROUNDS, PgDatabase, PruneGate};
use sqlx::{Row, migrate::Migrator};

use crate::{
    Cache, CacheError, Flight, Pruner, StoreOptions, check_key,
    store::{key_hash, store_error, ttl_micros},
};

/// The `forge_cache_entries` migration. Combine it with the application's own
/// through [`forge_db::combine_migrations`].
pub static MIGRATIONS: Migrator = sqlx::migrate!("migrations/postgres");

/// A [`Cache`] and [`Pruner`] on PostgreSQL. Safe for concurrent use; starts
/// no task.
#[derive(Debug)]
pub struct PostgresCache {
    db: PgDatabase,
    options: StoreOptions,
    flight: Flight,
    gate: PruneGate,
}

impl PostgresCache {
    /// A cache on `db`; the table comes from [`MIGRATIONS`].
    pub fn new(db: PgDatabase, options: StoreOptions) -> Result<Self, CacheError> {
        Ok(Self {
            db,
            options: options.validated()?,
            flight: Flight::default(),
            gate: PruneGate::new(),
        })
    }
}

#[async_trait]
impl Cache for PostgresCache {
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, CacheError> {
        check_key(key)?;
        let row = sqlx::query(
            "SELECT value FROM forge_cache_entries \
             WHERE key_hash = $1 AND expires_at > clock_timestamp()",
        )
        .bind(key_hash(key).as_slice())
        .fetch_optional(self.db.pool())
        .await
        .map_err(store_error)?;
        row.map(|row| row.try_get(0).map_err(store_error))
            .transpose()
    }

    async fn set(&self, key: &str, value: &[u8], ttl: Option<Duration>) -> Result<(), CacheError> {
        let ttl = self.options.options().check_set(key, value, ttl)?;
        sqlx::query(
            "INSERT INTO forge_cache_entries (key_hash, value, created_at, expires_at) \
             VALUES ($1, $2, now(), now() + ($3::bigint * interval '1 microsecond')) \
             ON CONFLICT (key_hash) DO UPDATE SET \
               value = EXCLUDED.value, created_at = EXCLUDED.created_at, expires_at = EXCLUDED.expires_at",
        )
        .bind(key_hash(key).as_slice())
        .bind(value)
        .bind(ttl_micros(ttl))
        .execute(self.db.pool())
        .await
        .map_err(store_error)?;
        // The write has succeeded; pruning never changes its outcome.
        if self.gate.due(self.options.interval(), self.options.batch()) {
            let _ = self.prune().await;
        }
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<(), CacheError> {
        check_key(key)?;
        sqlx::query("DELETE FROM forge_cache_entries WHERE key_hash = $1")
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
impl Pruner for PostgresCache {
    async fn prune(&self) -> Result<u64, CacheError> {
        let batch = i64::from(self.options.batch());
        let mut removed = 0_u64;
        for _ in 0..MAX_PRUNE_ROUNDS {
            let result = sqlx::query(
                "DELETE FROM forge_cache_entries WHERE key_hash IN (\
                   SELECT key_hash FROM forge_cache_entries \
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

        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM forge_cache_entries")
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
            let limit = excess.min(batch);
            let result = sqlx::query(
                "DELETE FROM forge_cache_entries WHERE key_hash IN (\
                   SELECT key_hash FROM forge_cache_entries \
                   ORDER BY created_at LIMIT $1 FOR UPDATE SKIP LOCKED)",
            )
            .bind(limit)
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
