//! The generic database handle: pool, transactions, migrations and readiness.

use std::{fmt, str::FromStr, time::Duration};

use async_trait::async_trait;
use forge_config::{DatabaseScheme, SecretString};
use sqlx::{Connection, Pool, migrate::Migrator, pool::PoolOptions};

use crate::{
    Backend, DatabaseError, DbError, RowsAffected, Transaction, TransactionManager,
    backend::ConnectOptions,
    migrate::{self, RevertScope},
};

/// PostgreSQL database handle.
#[cfg(feature = "postgres")]
pub type PgDatabase = Database<sqlx::Postgres>;
/// MySQL 8.4+ / MariaDB 11.4+ database handle.
#[cfg(feature = "mysql")]
pub type MySqlDatabase = Database<sqlx::MySql>;
/// SQLite database handle.
#[cfg(feature = "sqlite")]
pub type SqliteDatabase = Database<sqlx::Sqlite>;

/// Pool sizing shared by every backend.
///
/// The defaults suit a small service: 10 connections, a 5 second wait for
/// one and no statement timeout. A request that cannot get a connection in
/// time fails with the retryable [`DbError::PoolTimeout`] instead of queueing
/// indefinitely. The serving process takes its settings from the
/// configuration ([`PoolSettings::from_config`]).
///
/// ```
/// use std::time::Duration;
/// use forge_db::PoolSettings;
///
/// let settings = PoolSettings::default()
///     .with_max_connections(20)
///     .with_acquire_timeout(Duration::from_secs(2))
///     .with_statement_timeout(Some(Duration::from_secs(30)));
/// assert_eq!(settings.max_connections(), 20);
/// assert_eq!(settings.statement_timeout(), Some(Duration::from_secs(30)));
/// ```
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct PoolSettings {
    max_connections: u32,
    acquire_timeout: Duration,
    statement_timeout: Option<Duration>,
    job_announcements: bool,
}

impl PoolSettings {
    /// Maximum number of pooled connections (at least 1).
    #[must_use]
    pub const fn max_connections(&self) -> u32 {
        self.max_connections
    }

    /// How long to wait for a free connection.
    #[must_use]
    pub const fn acquire_timeout(&self) -> Duration {
        self.acquire_timeout
    }

    /// Sets the maximum number of pooled connections; zero becomes one.
    #[must_use]
    pub const fn with_max_connections(mut self, max_connections: u32) -> Self {
        self.max_connections = if max_connections == 0 {
            1
        } else {
            max_connections
        };
        self
    }

    /// Sets how long to wait for a free connection.
    #[must_use]
    pub const fn with_acquire_timeout(mut self, acquire_timeout: Duration) -> Self {
        self.acquire_timeout = acquire_timeout;
        self
    }

    /// The server-side bound of every statement, if any.
    #[must_use]
    pub const fn statement_timeout(&self) -> Option<Duration> {
        self.statement_timeout
    }

    /// Bounds every statement on the server, set on each new connection:
    /// `statement_timeout` on PostgreSQL, `max_statement_time` on MariaDB,
    /// `max_execution_time` (read-only `SELECT` only) on MySQL. SQLite has no
    /// server-side bound and ignores it. It counts whole milliseconds; a
    /// shorter duration becomes one millisecond, never "no timeout". `None`
    /// leaves the server's setting.
    #[must_use]
    pub const fn with_statement_timeout(mut self, statement_timeout: Option<Duration>) -> Self {
        self.statement_timeout = statement_timeout;
        self
    }

    /// Whether each new PostgreSQL connection sets `forge.jobs_notify = on`.
    #[must_use]
    pub const fn job_announcements(&self) -> bool {
        self.job_announcements
    }

    /// Sets `forge.jobs_notify = on` on each new PostgreSQL connection, so
    /// every job enqueued through the pool announces itself to listening
    /// workers when it is due (forge-jobs ADR 0046; `forge::App::jobs_notify`
    /// and `FORGE_JOBS_NOTIFY` turn it on for the application's pool).
    /// Other backends ignore it. Default off.
    #[must_use]
    pub const fn with_job_announcements(mut self, on: bool) -> Self {
        self.job_announcements = on;
        self
    }

    /// The serving pool of `config`: `FORGE_DATABASE_MAX_CONNS`,
    /// `FORGE_DATABASE_CONNECT_TIMEOUT` (the acquire timeout, which bounds
    /// opening a connection too) and `FORGE_DATABASE_STATEMENT_TIMEOUT`.
    #[must_use]
    pub const fn from_config(config: &forge_config::DatabaseConfig) -> Self {
        Self {
            max_connections: if config.max_connections == 0 {
                1
            } else {
                config.max_connections
            },
            acquire_timeout: config.connect_timeout,
            statement_timeout: Some(config.statement_timeout),
            job_announcements: false,
        }
    }
}

impl Default for PoolSettings {
    fn default() -> Self {
        Self {
            max_connections: 10,
            acquire_timeout: Duration::from_secs(5),
            statement_timeout: None,
            job_announcements: false,
        }
    }
}

/// Whole milliseconds of a statement timeout, at least 1 (0 means "no
/// timeout" to every server) and at most what PostgreSQL accepts.
pub(crate) fn statement_timeout_millis(timeout: Duration) -> u64 {
    u64::try_from(timeout.as_millis())
        .unwrap_or(u64::MAX)
        .clamp(1, i32::MAX as u64)
}

/// A connection pool for one backend with Forge's error, migration and
/// transaction semantics.
///
/// `Database` is cheap to clone (the pool is reference counted) and its
/// `Debug` output never contains the connection URL.
pub struct Database<B: Backend> {
    pool: Pool<B>,
}

impl<B: Backend> Clone for Database<B> {
    fn clone(&self) -> Self {
        Self {
            pool: self.pool.clone(),
        }
    }
}

impl<B: Backend> fmt::Debug for Database<B> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Database")
            .field("backend", &<B as Backend>::SCHEME)
            .field("connections", &self.pool.size())
            .field("closed", &self.pool.is_closed())
            .finish_non_exhaustive()
    }
}

impl<B: Backend> Database<B> {
    /// Connects eagerly with the default [`PoolSettings`], failing with the
    /// retryable [`DbError::Unavailable`] if the database stays unreachable
    /// for the acquire timeout.
    ///
    /// Fails with [`DbError::Configuration`] when the URL scheme does not
    /// belong to this backend.
    pub async fn connect(url: &SecretString) -> Result<Self, DbError> {
        Self::connect_with(url, PoolSettings::default()).await
    }

    /// Like [`Database::connect`] with explicit pool sizing.
    pub async fn connect_with(url: &SecretString, settings: PoolSettings) -> Result<Self, DbError> {
        let (options, pool_options) = Self::prepare(url, settings)?;
        let pool = pool_options
            .connect_with(options)
            .await
            .map_err(|error| match error {
                // The pool keeps retrying a refused connection until the acquire
                // timeout, so a timeout here means the server cannot be reached.
                sqlx::Error::PoolTimedOut => DbError::Unavailable(error),
                other => DbError::from(other),
            })?;
        Ok(Self { pool })
    }

    /// Creates a pool that connects on first use, so it succeeds while the
    /// database is down (queries then fail with a retryable error). Must be
    /// called inside a Tokio runtime.
    pub fn connect_lazy(url: &SecretString) -> Result<Self, DbError> {
        Self::connect_lazy_with(url, PoolSettings::default())
    }

    /// Like [`Database::connect_lazy`] with explicit pool sizing.
    pub fn connect_lazy_with(url: &SecretString, settings: PoolSettings) -> Result<Self, DbError> {
        let (options, pool_options) = Self::prepare(url, settings)?;
        Ok(Self {
            pool: pool_options.connect_lazy_with(options),
        })
    }

    fn prepare(
        url: &SecretString,
        settings: PoolSettings,
    ) -> Result<(ConnectOptions<B>, PoolOptions<B>), DbError> {
        let raw = url.expose();
        if DatabaseScheme::of_url(raw) != Some(<B as Backend>::SCHEME) {
            return Err(DbError::configuration(format!(
                "{} requires a {} URL",
                <B as Backend>::SCHEME.name(),
                <B as Backend>::SCHEME.accepted_schemes()
            )));
        }
        let options = <ConnectOptions<B> as FromStr>::from_str(raw)
            .map_err(|_| DbError::configuration("the database URL could not be parsed"))?;
        let options = <B as crate::backend::sealed::Hooks>::configure(raw, options);
        let pool_options = PoolOptions::<B>::new()
            .max_connections(settings.max_connections())
            .acquire_timeout(settings.acquire_timeout());
        let pool_options = <B as crate::backend::sealed::Hooks>::pool_options(raw, pool_options);
        let pool_options =
            <B as crate::backend::sealed::Hooks>::connection_setup(pool_options, &settings);
        Ok((options, pool_options))
    }

    /// The underlying SQLx pool, for adapters that run queries.
    #[must_use]
    pub fn pool(&self) -> &Pool<B> {
        &self.pool
    }

    /// The engine this handle talks to.
    #[must_use]
    pub fn scheme(&self) -> DatabaseScheme {
        <B as Backend>::SCHEME
    }

    /// Checks that a connection can be acquired and answers a ping. Used by
    /// readiness probes.
    pub async fn ping(&self) -> Result<(), DbError> {
        let mut connection = self.pool.acquire().await?;
        connection.ping().await?;
        Ok(())
    }

    /// Starts a transaction. Dropping it without committing rolls it back.
    pub async fn begin(&self) -> Result<DbTransaction<B>, DbError> {
        Ok(DbTransaction {
            inner: self.pool.begin().await?,
        })
    }

    /// Applies every pending migration and returns the versions applied, in
    /// ascending order (empty when the schema is current).
    ///
    /// Concurrent callers are serialized by the backend's migration lock
    /// (PostgreSQL advisory lock, MySQL/MariaDB named lock; SQLite has a
    /// single writer). Applied migrations are checksum-verified and a
    /// previously failed (dirty) migration is rejected. An applied version the
    /// migrator does not contain fails with [`MigrateError::VersionMissing`](sqlx::migrate::MigrateError::VersionMissing),
    /// unless it is a framework migration
    /// ([`FRAMEWORK_MIGRATIONS`](crate::FRAMEWORK_MIGRATIONS)) whose feature
    /// was turned off (ADR 0030).
    pub async fn migrate(&self, migrator: &Migrator) -> Result<Vec<i64>, DbError> {
        migrate::apply_pending(&self.pool, migrator).await
    }

    /// Lists the migrations of `migrator`, and the applied versions it does
    /// not contain, ascending, with their [`MigrationState`](crate::MigrationState).
    ///
    /// It only reads: a database without the bookkeeping table has nothing
    /// applied, and the table is not created.
    pub async fn migration_status(
        &self,
        migrator: &Migrator,
    ) -> Result<Vec<crate::MigrationStatus>, DbError> {
        migrate::status(&self.pool, migrator).await
    }

    /// Reverts the application's newest applied migration with its down
    /// migration and returns its version, or `None` when nothing is applied.
    ///
    /// Migrations shipped by Forge crates ([`FRAMEWORK_MIGRATIONS`](crate::FRAMEWORK_MIGRATIONS), merged
    /// with [`combine_migrations`](crate::combine_migrations)) are outside
    /// this scope even when their version is higher (ADR 0030): when only
    /// they remain applied the call fails with
    /// [`DbError::FrameworkMigrationOnly`] and reverts nothing. Fails with
    /// [`DbError::IrreversibleMigration`] when the migration has no down
    /// migration.
    pub async fn rollback(&self, migrator: &Migrator) -> Result<Option<i64>, DbError> {
        migrate::revert_latest(&self.pool, migrator, RevertScope::Application).await
    }

    /// Reverts the newest applied framework migration that `migrator`
    /// contains ([`FRAMEWORK_MIGRATIONS`](crate::FRAMEWORK_MIGRATIONS)) and returns its version, or `None`
    /// when none is applied; application migrations are never touched.
    ///
    /// This is the explicit way to drop a framework table such as
    /// `forge_cache_entries`; the application's `rollback` command never does.
    /// Pass the same migrator the application migrates with, so every applied
    /// version is still recognized.
    pub async fn rollback_framework(&self, migrator: &Migrator) -> Result<Option<i64>, DbError> {
        migrate::revert_latest(&self.pool, migrator, RevertScope::Framework).await
    }

    /// Closes the pool, waiting for checked-out connections to return.
    pub async fn close(&self) {
        self.pool.close().await;
    }
}

/// A database transaction. Dropping it without [`commit`](Self::commit) rolls
/// it back.
///
/// Run queries with `query.execute(tx.connection())`.
pub struct DbTransaction<B: Backend> {
    inner: sqlx::Transaction<'static, B>,
}

impl<B: Backend> fmt::Debug for DbTransaction<B> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DbTransaction")
            .field("backend", &<B as Backend>::SCHEME)
            .finish_non_exhaustive()
    }
}

impl<B: Backend> DbTransaction<B> {
    /// The transaction's connection, to pass to SQLx queries.
    pub fn connection(&mut self) -> &mut B::Connection {
        &mut self.inner
    }

    /// Commits the transaction.
    pub async fn commit(self) -> Result<(), DbError> {
        self.inner.commit().await.map_err(DbError::from)
    }

    /// Explicitly rolls the transaction back.
    pub async fn rollback(self) -> Result<(), DbError> {
        self.inner.rollback().await.map_err(DbError::from)
    }
}

#[async_trait]
impl<B: Backend> Transaction for DbTransaction<B> {
    async fn commit(self) -> Result<(), DatabaseError> {
        Self::commit(self).await.map_err(DatabaseError::from)
    }

    async fn rollback(self) -> Result<(), DatabaseError> {
        Self::rollback(self).await.map_err(DatabaseError::from)
    }
}

#[async_trait]
impl<B: Backend> TransactionManager for Database<B> {
    type Transaction<'a>
        = DbTransaction<B>
    where
        Self: 'a;

    async fn begin(&self) -> Result<Self::Transaction<'_>, DatabaseError> {
        Self::begin(self).await.map_err(DatabaseError::from)
    }
}

/// Fails with [`DbError::Conflict`] unless `result` affected exactly
/// `expected` rows.
///
/// The usual use is optimistic locking: `UPDATE ... WHERE id = ? AND version =
/// ?` matches zero rows when another writer got there first. Works with the
/// query result of every enabled backend.
pub fn expect_rows(result: impl RowsAffected, expected: u64) -> Result<(), DbError> {
    if result.rows_affected() == expected {
        Ok(())
    } else {
        Err(DbError::Conflict)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{PoolSettings, statement_timeout_millis};

    #[test]
    fn statement_timeouts_are_whole_milliseconds_never_zero() {
        assert_eq!(statement_timeout_millis(Duration::ZERO), 1);
        assert_eq!(statement_timeout_millis(Duration::from_micros(999)), 1);
        assert_eq!(statement_timeout_millis(Duration::from_millis(1500)), 1500);
        assert_eq!(
            statement_timeout_millis(Duration::from_secs(u64::MAX)),
            i32::MAX as u64
        );
    }

    #[test]
    fn the_serving_pool_follows_the_configuration() {
        let defaults = PoolSettings::from_config(&forge_config::DatabaseConfig::default());
        assert_eq!(defaults.max_connections(), 10);
        assert_eq!(defaults.acquire_timeout(), Duration::from_secs(5));
        assert_eq!(defaults.statement_timeout(), Some(Duration::from_secs(30)));
        let config = forge_config::DatabaseConfig {
            max_connections: 40,
            connect_timeout: Duration::from_secs(2),
            statement_timeout: Duration::from_millis(250),
            ..forge_config::DatabaseConfig::default()
        };
        let settings = PoolSettings::from_config(&config);
        assert_eq!(settings.max_connections(), 40);
        assert_eq!(settings.acquire_timeout(), Duration::from_secs(2));
        assert_eq!(
            settings.statement_timeout(),
            Some(Duration::from_millis(250))
        );
        // Commands and tests keep the old defaults: no statement bound.
        assert_eq!(PoolSettings::default().statement_timeout(), None);
    }
}
