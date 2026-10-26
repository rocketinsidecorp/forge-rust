//! The generic database handle: pool, transactions, migrations and readiness.

use std::{fmt, str::FromStr, time::Duration};

use async_trait::async_trait;
use forge_config::{DatabaseScheme, Environment, SecretString};
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

/// Pool sizing shared by every backend, and the PostgreSQL pool's
/// row-level security check.
///
/// The defaults suit a small service: 10 connections, a 5 second wait for
/// one and no statement timeout. A request that cannot get a connection in
/// time fails with the retryable [`DbError::PoolTimeout`] instead of queueing
/// indefinitely. The serving process takes its settings from the
/// configuration ([`PoolSettings::from_config`]).
///
/// # Row-level security (PostgreSQL)
///
/// Every PostgreSQL pool refuses, with [`DbError::RowSecurityBypass`], a
/// role that could escape row-level security (the rules are those of
/// `PgDatabase::require_row_security`) unless its
/// [environment](PoolSettings::with_environment) is development or test. An
/// unset environment is checked: it fails closed. An eager pool
/// ([`Database::connect_with`]) checks once, when it connects; a lazy pool
/// ([`Database::connect_lazy_with`]) checks each connection it opens, before
/// its first use. [`PoolSettings::allow_row_security_bypass`] is the only
/// opt-out (Forge-Go's `postgres.AllowRowSecurityBypass`).
///
/// # Session settings from the environment (PostgreSQL)
///
/// SQLx reads `PGOPTIONS` and `PGAPPNAME` into every connection. A pool
/// refuses to open while `PGOPTIONS` is set (`PGOPTIONS='-c
/// forge.tenant_id=...'` would give every session a default tenant, and
/// SQLx cannot drop it once read), and its `application_name` is the URL's,
/// else `forge`, never `PGAPPNAME`. SQLx never reads `PGTZ`: every session
/// is in UTC. [`PoolSettings::with_environment_session`] restores the
/// driver's behavior, for the migrator only.
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
    min_connections: u32,
    acquire_timeout: Duration,
    max_lifetime: Option<Duration>,
    idle_timeout: Option<Duration>,
    statement_timeout: Option<Duration>,
    job_announcements: bool,
    environment: Option<Environment>,
    row_security_bypass: bool,
    environment_session: bool,
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

    /// Fewest connections the pool keeps open (at most
    /// [`max_connections`](Self::max_connections)); default 0.
    #[must_use]
    pub const fn min_connections(&self) -> u32 {
        self.min_connections
    }

    /// Sets the fewest connections the pool keeps open, opening them in the
    /// background; a value above the maximum is lowered to it.
    #[must_use]
    pub const fn with_min_connections(mut self, min_connections: u32) -> Self {
        self.min_connections = min_connections;
        self
    }

    /// How long a connection lives before the pool replaces it, if bounded.
    #[must_use]
    pub const fn max_lifetime(&self) -> Option<Duration> {
        self.max_lifetime
    }

    /// Bounds how long a connection lives (`None`: forever); default 30
    /// minutes.
    #[must_use]
    pub const fn with_max_lifetime(mut self, max_lifetime: Option<Duration>) -> Self {
        self.max_lifetime = max_lifetime;
        self
    }

    /// How long an idle connection above the minimum stays open, if bounded.
    #[must_use]
    pub const fn idle_timeout(&self) -> Option<Duration> {
        self.idle_timeout
    }

    /// Bounds how long an idle connection above the minimum stays open
    /// (`None`: forever); default 10 minutes.
    #[must_use]
    pub const fn with_idle_timeout(mut self, idle_timeout: Option<Duration>) -> Self {
        self.idle_timeout = idle_timeout;
        self
    }

    /// The environment the pool serves in, if set.
    #[must_use]
    pub const fn environment(&self) -> Option<Environment> {
        self.environment
    }

    /// Sets the environment the pool serves in. Development and test skip
    /// the PostgreSQL row-level security check; production, or no
    /// environment at all, runs it (see the type documentation).
    #[must_use]
    pub const fn with_environment(mut self, environment: Environment) -> Self {
        self.environment = Some(environment);
        self
    }

    /// Accepts any PostgreSQL role, even one that could escape row-level
    /// security: the only opt-out of the check (Forge-Go's
    /// `postgres.AllowRowSecurityBypass`). Use it only for a pool that must
    /// connect as such a role on purpose, such as the migrator or an
    /// administration tool, never for a pool that serves tenants.
    #[must_use]
    pub const fn allow_row_security_bypass(mut self) -> Self {
        self.row_security_bypass = true;
        self
    }

    /// Whether a PostgreSQL pool built from these settings checks its role:
    /// unless [`allow_row_security_bypass`](Self::allow_row_security_bypass)
    /// was called, everywhere but development and test, and when no
    /// environment was set.
    #[must_use]
    pub const fn checks_row_security(&self) -> bool {
        !self.row_security_bypass
            && !matches!(
                self.environment,
                Some(Environment::Development | Environment::Test)
            )
    }

    /// Whether PostgreSQL connections take session settings from
    /// `PGOPTIONS` and `PGAPPNAME`.
    #[must_use]
    pub const fn environment_session(&self) -> bool {
        self.environment_session
    }

    /// Lets `PGOPTIONS` and `PGAPPNAME` reach every PostgreSQL connection,
    /// as for `psql`. Only the migrator does this (`PGOPTIONS='-c
    /// lock_timeout=5s' forge migrate`); a pool without it refuses to open
    /// while `PGOPTIONS` is set and names its sessions after the URL's
    /// `application_name`, else `forge`. Other backends ignore it.
    #[must_use]
    pub const fn with_environment_session(mut self, on: bool) -> Self {
        self.environment_session = on;
        self
    }

    /// The serving pool of `config`: `FORGE_DATABASE_MAX_CONNS`,
    /// `FORGE_DATABASE_MIN_CONNS`, `FORGE_DATABASE_CONNECT_TIMEOUT` (the
    /// acquire timeout, which bounds opening a connection too),
    /// `FORGE_DATABASE_MAX_CONN_LIFETIME`, `FORGE_DATABASE_MAX_CONN_IDLE_TIME`
    /// and `FORGE_DATABASE_STATEMENT_TIMEOUT`. The environment is left unset
    /// (checked); add it with [`with_environment`](Self::with_environment).
    #[must_use]
    pub const fn from_config(config: &forge_config::DatabaseConfig) -> Self {
        Self {
            max_connections: if config.max_connections == 0 {
                1
            } else {
                config.max_connections
            },
            min_connections: config.min_connections,
            acquire_timeout: config.connect_timeout,
            max_lifetime: Some(config.max_conn_lifetime),
            idle_timeout: Some(config.max_conn_idle_time),
            statement_timeout: Some(config.statement_timeout),
            job_announcements: false,
            environment: None,
            row_security_bypass: false,
            environment_session: false,
        }
    }
}

impl Default for PoolSettings {
    fn default() -> Self {
        Self {
            max_connections: 10,
            min_connections: 0,
            acquire_timeout: Duration::from_secs(5),
            max_lifetime: Some(Duration::from_secs(30 * 60)),
            idle_timeout: Some(Duration::from_secs(10 * 60)),
            statement_timeout: None,
            job_announcements: false,
            environment: None,
            row_security_bypass: false,
            environment_session: false,
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
    /// belong to this backend. The default settings name no environment, so
    /// a PostgreSQL pool is checked for row-level security (see
    /// [`PoolSettings`]); pass
    /// `PoolSettings::default().with_environment(...)` to
    /// [`Database::connect_with`] for development and test.
    pub async fn connect(url: &SecretString) -> Result<Self, DbError> {
        Self::connect_with(url, PoolSettings::default()).await
    }

    /// Like [`Database::connect`] with explicit settings. A PostgreSQL pool
    /// that [checks row-level security](PoolSettings::checks_row_security)
    /// runs the check once it is connected and is closed when the role is
    /// refused.
    pub async fn connect_with(url: &SecretString, settings: PoolSettings) -> Result<Self, DbError> {
        let (options, pool_options) = Self::prepare(url, settings, false)?;
        let pool = pool_options
            .connect_with(options)
            .await
            .map_err(|error| match error {
                // The pool keeps retrying a refused connection until the acquire
                // timeout, so a timeout here means the server cannot be reached.
                sqlx::Error::PoolTimedOut => DbError::Unavailable(error),
                other => DbError::from(other),
            })?;
        if let Err(error) = <B as crate::backend::sealed::Hooks>::verify(&pool, &settings).await {
            pool.close().await;
            return Err(error);
        }
        Ok(Self { pool })
    }

    /// Creates a pool that connects on first use, so it succeeds while the
    /// database is down (queries then fail with a retryable error). Must be
    /// called inside a Tokio runtime.
    pub fn connect_lazy(url: &SecretString) -> Result<Self, DbError> {
        Self::connect_lazy_with(url, PoolSettings::default())
    }

    /// Like [`Database::connect_lazy`] with explicit settings. A PostgreSQL
    /// pool that [checks row-level security](PoolSettings::checks_row_security)
    /// checks every connection it opens before its first use. SQLx retries
    /// a refused connection until the acquire timeout, so the caller sees
    /// [`DbError::PoolTimeout`], and the refusal is logged at error level;
    /// `PgDatabase::require_row_security` reports it directly.
    pub fn connect_lazy_with(url: &SecretString, settings: PoolSettings) -> Result<Self, DbError> {
        let (options, pool_options) = Self::prepare(url, settings, true)?;
        Ok(Self {
            pool: pool_options.connect_lazy_with(options),
        })
    }

    fn prepare(
        url: &SecretString,
        settings: PoolSettings,
        lazy: bool,
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
        let options = <B as crate::backend::sealed::Hooks>::session(raw, options, &settings)?;
        let pool_options = PoolOptions::<B>::new()
            .max_connections(settings.max_connections())
            .min_connections(settings.min_connections().min(settings.max_connections()))
            .acquire_timeout(settings.acquire_timeout())
            .max_lifetime(settings.max_lifetime())
            .idle_timeout(settings.idle_timeout());
        let pool_options = <B as crate::backend::sealed::Hooks>::pool_options(raw, pool_options);
        let pool_options =
            <B as crate::backend::sealed::Hooks>::connection_setup(pool_options, &settings, lazy);
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

    #[test]
    fn the_serving_pool_takes_the_size_and_lifetimes_of_the_configuration() {
        let config = forge_config::DatabaseConfig {
            min_connections: 2,
            max_conn_lifetime: Duration::from_secs(600),
            max_conn_idle_time: Duration::from_secs(60),
            ..forge_config::DatabaseConfig::default()
        };
        let settings = PoolSettings::from_config(&config);
        assert_eq!(settings.min_connections(), 2);
        assert_eq!(settings.max_lifetime(), Some(Duration::from_secs(600)));
        assert_eq!(settings.idle_timeout(), Some(Duration::from_secs(60)));
        let defaults = PoolSettings::from_config(&forge_config::DatabaseConfig::default());
        assert_eq!(defaults.min_connections(), 0);
        assert_eq!(defaults.max_lifetime(), Some(Duration::from_secs(30 * 60)));
        assert_eq!(defaults.idle_timeout(), Some(Duration::from_secs(5 * 60)));
    }

    #[test]
    fn the_row_security_check_runs_unless_development_test_or_the_opt_out() {
        use forge_config::Environment;
        // No environment fails closed: checked.
        assert!(PoolSettings::default().checks_row_security());
        assert!(
            PoolSettings::from_config(&forge_config::DatabaseConfig::default())
                .checks_row_security()
        );
        assert!(
            PoolSettings::default()
                .with_environment(Environment::Production)
                .checks_row_security()
        );
        for skipped in [Environment::Development, Environment::Test] {
            assert!(
                !PoolSettings::default()
                    .with_environment(skipped)
                    .checks_row_security()
            );
        }
        assert!(
            !PoolSettings::default()
                .with_environment(Environment::Production)
                .allow_row_security_bypass()
                .checks_row_security()
        );
        assert!(!PoolSettings::default().environment_session());
        assert!(
            PoolSettings::default()
                .with_environment_session(true)
                .environment_session()
        );
    }
}
