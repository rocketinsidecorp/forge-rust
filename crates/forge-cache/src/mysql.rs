//! MySQL and MariaDB shared cache: entries live in a table addressed by the
//! SHA-256 of the key, all expiry arithmetic uses the database clock
//! (`UTC_TIMESTAMP`), and pruning removes expired entries and, above
//! `max_entries`, the oldest writes, in bounded batches.

use std::time::Duration;

use async_trait::async_trait;
use forge_db::{MAX_PRUNE_ROUNDS, MySqlDatabase, PruneGate};
use sqlx::{Row, migrate::Migrator};

use crate::{
    Cache, CacheError, Flight, Pruner, StoreOptions, check_key,
    store::{key_hash, store_error, ttl_micros},
};

/// The `forge_cache_entries` migration. Combine it with the application's own
/// through [`forge_db::combine_migrations`].
pub static MIGRATIONS: Migrator = sqlx::migrate!("migrations/mysql");

/// A [`Cache`] and [`Pruner`] on MySQL 8.4+ or MariaDB 11.4+. Safe for concurrent use; starts no
/// task.
#[derive(Debug)]
pub struct MySqlCache {
    db: MySqlDatabase,
    options: StoreOptions,
    flight: Flight,
    gate: PruneGate,
}

impl MySqlCache {
    /// A cache on `db`; the table comes from [`MIGRATIONS`].
    pub fn new(db: MySqlDatabase, options: StoreOptions) -> Result<Self, CacheError> {
        Ok(Self {
            db,
            options: options.validated()?,
            flight: Flight::default(),
            gate: PruneGate::new(),
        })
    }
}

#[async_trait]
impl Cache for MySqlCache {
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, CacheError> {
        check_key(key)?;
        let row = sqlx::query(
            "SELECT value FROM forge_cache_entries \
             WHERE key_hash = ? AND expires_at > UTC_TIMESTAMP(6)",
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
            // VALUES(col) is deprecated in MySQL 8.0.20+ in favor of a row alias,
            // which MariaDB does not support; VALUES() works on both.
            "INSERT INTO forge_cache_entries (key_hash, value, created_at, expires_at) \
             VALUES (?, ?, UTC_TIMESTAMP(6), DATE_ADD(UTC_TIMESTAMP(6), INTERVAL ? MICROSECOND)) \
             ON DUPLICATE KEY UPDATE \
               value = VALUES(value), created_at = VALUES(created_at), expires_at = VALUES(expires_at)",
        )
        .bind(key_hash(key).as_slice())
        .bind(value)
        .bind(ttl_micros(ttl))
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
        sqlx::query("DELETE FROM forge_cache_entries WHERE key_hash = ?")
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
impl Pruner for MySqlCache {
    async fn prune(&self) -> Result<u64, CacheError> {
        let batch = i64::from(self.options.batch());
        let mut removed = 0_u64;
        for _ in 0..MAX_PRUNE_ROUNDS {
            let result = sqlx::query(
                "DELETE FROM forge_cache_entries WHERE expires_at <= UTC_TIMESTAMP(6) \
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
            let result = sqlx::query("DELETE FROM forge_cache_entries ORDER BY created_at LIMIT ?")
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
