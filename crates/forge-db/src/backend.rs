//! Supported database backends.
//!
//! [`Backend`] is implemented for [`sqlx::Postgres`], [`sqlx::MySql`] and
//! [`sqlx::Sqlite`], each behind the Cargo feature of the same name. The trait
//! is sealed: the backend-specific behavior (connection settings, pool sizing
//! rules and test database provisioning) lives in one small module per backend
//! so [`Database`](crate::Database) needs no bounds beyond `B: Backend`.
//!
//! MariaDB speaks the MySQL protocol and is served by the same backend:
//! [`sqlx::MySql`] covers MySQL 8.4+ and MariaDB 11.4+.

use forge_config::DatabaseScheme;
use sqlx::{Connection, migrate::Migrate};

/// A database Forge can connect to: PostgreSQL, MySQL/MariaDB or SQLite.
///
/// Implemented for [`sqlx::Postgres`] (feature `postgres`), [`sqlx::MySql`]
/// (feature `mysql`; MySQL 8.4+ and MariaDB 11.4+) and [`sqlx::Sqlite`]
/// (feature `sqlite`). The trait is sealed.
pub trait Backend: sealed::Hooks {
    /// The engine selected by the `FORGE_DATABASE_URL` schemes this backend
    /// accepts.
    const SCHEME: DatabaseScheme;
}

/// A statement result that reports the number of affected rows.
///
/// Implemented for the query results of every enabled backend so that
/// [`expect_rows`](crate::expect_rows) accepts any of them. Sealed.
pub trait RowsAffected: sealed::QueryResult {
    /// Number of rows inserted, updated or deleted by the statement.
    ///
    /// MySQL/MariaDB report *matched* rows (SQLx connects with the
    /// `FOUND_ROWS` capability), like PostgreSQL and SQLite.
    fn rows_affected(&self) -> u64;
}

/// The connection options type of a backend.
pub(crate) type ConnectOptions<B> = <<B as sqlx::Database>::Connection as Connection>::Options;

/// Implementation details of `forge-db`, unreachable by other crates.
pub(crate) mod sealed {
    use forge_config::DatabaseScheme;

    use sqlx::pool::PoolOptions;

    use super::{ConnectOptions, Migrate};

    /// Per-backend behavior behind [`super::Backend`].
    pub trait Hooks: sqlx::Database<Connection: Migrate> + sqlx::migrate::MigrateDatabase {
        /// The engine this backend implements.
        const SCHEME: DatabaseScheme;

        /// Environment variable holding the administrative URL used by
        /// `testing::TestDatabase` (unused for SQLite).
        const TEST_URL_ENV: &'static str;

        /// Applies Forge's connection settings to options parsed from `url`.
        fn configure(url: &str, options: ConnectOptions<Self>) -> ConnectOptions<Self> {
            let _ = url;
            options
        }

        /// What `url` really names once the driver has applied its query
        /// overrides and environment defaults: the database, the server and
        /// whether that is this machine. Never echoes the URL in an error.
        fn endpoint(url: &str) -> Result<crate::admin::Endpoint, crate::DbError>;

        /// Adjusts the pool built from the common [`PoolSettings`]; the
        /// default keeps it.
        fn pool_options(url: &str, options: PoolOptions<Self>) -> PoolOptions<Self> {
            let _ = url;
            options
        }

        /// Bounds every statement of each new connection to `millis`
        /// (1 to `i32::MAX`) on the server. The default, for a backend
        /// without a server-side bound (SQLite), keeps the pool.
        fn bound_statements(options: PoolOptions<Self>, millis: u64) -> PoolOptions<Self> {
            let _ = millis;
            options
        }

        /// Prepares each new connection of a pool built from `settings`:
        /// the statement bound, and on PostgreSQL the job announcements.
        /// The default applies only the statement bound.
        fn connection_setup(
            options: PoolOptions<Self>,
            settings: &crate::PoolSettings,
        ) -> PoolOptions<Self> {
            match settings.statement_timeout() {
                Some(timeout) => Self::bound_statements(
                    options,
                    crate::database::statement_timeout_millis(timeout),
                ),
                None => options,
            }
        }

        /// Creates an empty, uniquely named database for a test. Server
        /// backends use the administrative URL in the environment variable
        /// `admin_env`; file backends ignore it.
        #[cfg(feature = "testing")]
        fn provision(
            admin_env: &str,
        ) -> impl std::future::Future<Output = Result<crate::testing::Provision, crate::DbError>> + Send;
    }

    /// Query results accepted by `expect_rows`.
    pub trait QueryResult {}
}

macro_rules! rows_affected {
    ($feature:literal, $result:ty) => {
        #[cfg(feature = $feature)]
        impl sealed::QueryResult for $result {}

        #[cfg(feature = $feature)]
        impl RowsAffected for $result {
            fn rows_affected(&self) -> u64 {
                <$result>::rows_affected(self)
            }
        }
    };
}

rows_affected!("postgres", sqlx::postgres::PgQueryResult);
rows_affected!("mysql", sqlx::mysql::MySqlQueryResult);
rows_affected!("sqlite", sqlx::sqlite::SqliteQueryResult);
