//! Database wiring for the application lifecycle: the declared backend, the
//! pool handed to composition hooks, the readiness probe and the `migrate` /
//! `rollback` commands.
//!
//! [`Declaration`] and [`DatabaseHandle`] have one variant per compiled-in
//! backend (and none without database features), so the rest of the facade
//! handles every backend without feature conditionals.

use std::{fmt, process::ExitCode, sync::Arc, time::Duration};

use forge_config::{DatabaseConfig, DatabaseScheme, Environment, SecretString};
use forge_db::DbError;
#[cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]
use forge_db::Migrator;
use forge_http::{HeaderValue, Method, Response, RouteError, Router, StatusCode, header};
use thiserror::Error;
use tracing::warn;

use crate::{app::ErrorChain, cli::DbCommand};

/// Upper bound for closing the pool after the server has drained.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);
/// Upper bound for one readiness probe, so an orchestrator is never left
/// waiting on a hung database.
const READINESS_TIMEOUT: Duration = Duration::from_secs(3);

/// The database an application declared, with its embedded migrations.
pub(crate) enum Declaration {
    /// Declared with `App::postgres`.
    #[cfg(feature = "postgres")]
    Postgres(Migrator),
    /// Declared with `App::mysql`.
    #[cfg(feature = "mysql")]
    MySql(Migrator),
    /// Declared with `App::sqlite`.
    #[cfg(feature = "sqlite")]
    Sqlite(Migrator),
}

impl Declaration {
    /// The engine `FORGE_DATABASE_URL` must select.
    pub(crate) fn scheme(&self) -> DatabaseScheme {
        match *self {
            #[cfg(feature = "postgres")]
            Self::Postgres(_) => DatabaseScheme::Postgres,
            #[cfg(feature = "mysql")]
            Self::MySql(_) => DatabaseScheme::MySql,
            #[cfg(feature = "sqlite")]
            Self::Sqlite(_) => DatabaseScheme::Sqlite,
        }
    }

    /// The builder method that made this declaration, e.g. `App::postgres`.
    pub(crate) fn method(&self) -> &'static str {
        match self.scheme() {
            DatabaseScheme::Postgres => "App::postgres",
            DatabaseScheme::MySql => "App::mysql",
            DatabaseScheme::Sqlite => "App::sqlite",
        }
    }

    /// Creates the lazily connecting pool for `serve`. Must run inside the
    /// Tokio runtime.
    pub(crate) fn connect_lazy(&self, url: &SecretString) -> Result<DatabaseHandle, DbError> {
        let _ = url;
        match *self {
            #[cfg(feature = "postgres")]
            Self::Postgres(_) => {
                forge_db::Database::connect_lazy(url).map(DatabaseHandle::Postgres)
            }
            #[cfg(feature = "mysql")]
            Self::MySql(_) => forge_db::Database::connect_lazy(url).map(DatabaseHandle::MySql),
            #[cfg(feature = "sqlite")]
            Self::Sqlite(_) => forge_db::Database::connect_lazy(url).map(DatabaseHandle::Sqlite),
        }
    }

    /// Connects eagerly with `url`, for application commands.
    pub(crate) async fn connect(&self, url: &SecretString) -> Result<DatabaseHandle, DbError> {
        let _ = url;
        match *self {
            #[cfg(feature = "postgres")]
            Self::Postgres(_) => forge_db::Database::connect(url)
                .await
                .map(DatabaseHandle::Postgres),
            #[cfg(feature = "mysql")]
            Self::MySql(_) => forge_db::Database::connect(url)
                .await
                .map(DatabaseHandle::MySql),
            #[cfg(feature = "sqlite")]
            Self::Sqlite(_) => forge_db::Database::connect(url)
                .await
                .map(DatabaseHandle::Sqlite),
        }
    }

    /// Connects eagerly with `url` and runs `command`; returns the exit code.
    ///
    /// `framework` holds embedded framework schemas (jobs, outbox) that
    /// `migrate` applies before the application's migrations; each keeps
    /// its own bookkeeping table. `rollback` only reverts application
    /// migrations.
    pub(crate) async fn run_migration_command(
        &self,
        app: &'static str,
        url: &SecretString,
        command: MigrationCommand,
        framework: &[forge_db::Migrator],
    ) -> ExitCode {
        let _ = (app, url, command, framework);
        match *self {
            #[cfg(feature = "postgres")]
            Self::Postgres(ref migrator) => {
                commands::run_migration_command::<forge_db::sqlx::Postgres>(
                    app, url, migrator, command, framework,
                )
                .await
            }
            #[cfg(feature = "mysql")]
            Self::MySql(ref migrator) => {
                commands::run_migration_command::<forge_db::sqlx::MySql>(
                    app, url, migrator, command, framework,
                )
                .await
            }
            #[cfg(feature = "sqlite")]
            Self::Sqlite(ref migrator) => {
                commands::run_migration_command::<forge_db::sqlx::Sqlite>(
                    app, url, migrator, command, framework,
                )
                .await
            }
        }
    }

    /// Runs a `db` command against the database named by `url`; returns the
    /// exit code.
    pub(crate) async fn run_admin_command(
        &self,
        url: &SecretString,
        command: DbCommand,
        environment: Environment,
    ) -> ExitCode {
        let _ = (url, command, environment);
        match *self {
            #[cfg(feature = "postgres")]
            Self::Postgres(_) => {
                commands::run_admin_command::<forge_db::sqlx::Postgres>(url, command, environment)
                    .await
            }
            #[cfg(feature = "mysql")]
            Self::MySql(_) => {
                commands::run_admin_command::<forge_db::sqlx::MySql>(url, command, environment)
                    .await
            }
            #[cfg(feature = "sqlite")]
            Self::Sqlite(_) => {
                commands::run_admin_command::<forge_db::sqlx::Sqlite>(url, command, environment)
                    .await
            }
        }
    }

    /// Checks `config` against the declared backend.
    ///
    /// `serve` needs the runtime URL; `migrate`/`rollback` need the migration
    /// URL. A URL for another engine is rejected before anything connects.
    /// Returns the validated URL.
    pub(crate) fn validate<'c>(
        &self,
        config: &'c DatabaseConfig,
        purpose: UrlPurpose,
    ) -> Result<&'c SecretString, DatabaseConfigError> {
        let (key, url) = match purpose {
            UrlPurpose::Runtime => ("FORGE_DATABASE_URL", config.url.as_ref()),
            UrlPurpose::Migration => (
                "FORGE_MIGRATION_DATABASE_URL",
                config.migration_url.as_ref(),
            ),
        };
        let Some(url) = url else {
            return Err(DatabaseConfigError::Missing {
                key,
                method: self.method(),
            });
        };
        let expected = self.scheme();
        if DatabaseScheme::of_url(url.expose()) == Some(expected) {
            Ok(url)
        } else {
            Err(DatabaseConfigError::SchemeMismatch {
                key,
                method: self.method(),
                accepted: expected.accepted_schemes(),
            })
        }
    }
}

/// Which connection URL a command uses.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum UrlPurpose {
    /// `FORGE_DATABASE_URL`, used by `serve`.
    Runtime,
    /// `FORGE_MIGRATION_DATABASE_URL`, used by `migrate` and `rollback`.
    Migration,
}

/// The configured URL does not fit the declared backend. Never contains the
/// URL itself.
#[derive(Debug, Error, Eq, PartialEq)]
pub(crate) enum DatabaseConfigError {
    /// A required URL is not set.
    #[error("{key} is required by {method}")]
    Missing {
        /// The environment variable.
        key: &'static str,
        /// The declaring builder method.
        method: &'static str,
    },
    /// The URL selects another database engine.
    #[error("{key} must start with {accepted} because the application declared {method}")]
    SchemeMismatch {
        /// The environment variable.
        key: &'static str,
        /// The declaring builder method.
        method: &'static str,
        /// The schemes the declared backend accepts.
        accepted: &'static str,
    },
}

/// The pool of the declared database, as handed to composition hooks.
#[derive(Clone)]
pub(crate) enum DatabaseHandle {
    /// A PostgreSQL pool.
    #[cfg(feature = "postgres")]
    Postgres(forge_db::PgDatabase),
    /// A MySQL/MariaDB pool.
    #[cfg(feature = "mysql")]
    MySql(forge_db::MySqlDatabase),
    /// A SQLite pool.
    #[cfg(feature = "sqlite")]
    Sqlite(forge_db::SqliteDatabase),
}

impl fmt::Debug for DatabaseHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Without a backend feature the enum is uninhabited and the match has
        // no arms; naming `formatter` first keeps that build warning-free.
        let _ = &formatter;
        match *self {
            #[cfg(feature = "postgres")]
            Self::Postgres(ref database) => database.fmt(formatter),
            #[cfg(feature = "mysql")]
            Self::MySql(ref database) => database.fmt(formatter),
            #[cfg(feature = "sqlite")]
            Self::Sqlite(ref database) => database.fmt(formatter),
        }
    }
}

impl DatabaseHandle {
    /// The backend of this pool.
    #[cfg(feature = "broadcast")]
    pub(crate) fn scheme(&self) -> DatabaseScheme {
        match *self {
            #[cfg(feature = "postgres")]
            Self::Postgres(_) => DatabaseScheme::Postgres,
            #[cfg(feature = "mysql")]
            Self::MySql(_) => DatabaseScheme::MySql,
            #[cfg(feature = "sqlite")]
            Self::Sqlite(_) => DatabaseScheme::Sqlite,
        }
    }

    /// Pings whichever backend this is.
    pub(crate) async fn ping(&self) -> Result<(), DbError> {
        match *self {
            #[cfg(feature = "postgres")]
            Self::Postgres(ref database) => database.ping().await,
            #[cfg(feature = "mysql")]
            Self::MySql(ref database) => database.ping().await,
            #[cfg(feature = "sqlite")]
            Self::Sqlite(ref database) => database.ping().await,
        }
    }

    /// Closes the pool, giving checked-out connections a bounded time to
    /// return.
    pub(crate) async fn close(&self) {
        let closing = async {
            match *self {
                #[cfg(feature = "postgres")]
                Self::Postgres(ref database) => database.close().await,
                #[cfg(feature = "mysql")]
                Self::MySql(ref database) => database.close().await,
                #[cfg(feature = "sqlite")]
                Self::Sqlite(ref database) => database.close().await,
            }
        };
        if tokio::time::timeout(CLOSE_TIMEOUT, closing).await.is_err() {
            warn!(
                timeout = ?CLOSE_TIMEOUT,
                "timed out closing the database pool; abandoning remaining connections"
            );
        }
    }
}

/// Schema command selected on the command line.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MigrationCommand {
    /// Apply every pending migration.
    Migrate,
    /// List every migration and its state.
    Status,
    /// Revert the most recently applied `steps` migrations.
    Rollback {
        /// How many to revert.
        steps: u32,
    },
}

/// Exit code of `db exists` when the database does not exist, so a script (or
/// `forge db prepare`) can tell "no" from a failure.
#[cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]
pub(crate) const EXIT_DATABASE_MISSING: u8 = 3;

/// Registers the standard health routes. With a database, `/health/ready`
/// pings it and answers `503` while it is unreachable; without one it is
/// always ready, exactly like [`Router::standard_health_routes`].
pub(crate) fn register_health(
    router: &mut Router,
    database: Option<DatabaseHandle>,
) -> Result<(), RouteError> {
    let Some(database) = database else {
        router.standard_health_routes()?;
        return Ok(());
    };
    router.route(Method::GET, "/health", |_| async {
        json(StatusCode::OK, r#"{"status":"ok"}"#)
    })?;
    router.route(Method::GET, "/health/live", |_| async {
        json(StatusCode::OK, r#"{"status":"live"}"#)
    })?;
    let database = Arc::new(database);
    router.route(Method::GET, "/health/ready", move |_| {
        let database = Arc::clone(&database);
        async move { readiness(&database).await }
    })?;
    Ok(())
}

/// Answers a readiness probe by pinging the database within a bounded time.
async fn readiness(database: &DatabaseHandle) -> Response {
    match tokio::time::timeout(READINESS_TIMEOUT, database.ping()).await {
        Ok(Ok(())) => json(StatusCode::OK, r#"{"status":"ready"}"#),
        Ok(Err(error)) => {
            warn!(error = %ErrorChain(&error), "database readiness check failed");
            unavailable()
        }
        Err(_) => {
            warn!(timeout = ?READINESS_TIMEOUT, "database readiness check timed out");
            unavailable()
        }
    }
}

fn unavailable() -> Response {
    json(
        StatusCode::SERVICE_UNAVAILABLE,
        r#"{"status":"unavailable","checks":{"database":"unavailable"}}"#,
    )
}

fn json(status: StatusCode, body: &'static str) -> Response {
    let mut response = Response::new(status, body);
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

/// The generic command runners; only reachable through a declared backend.
#[cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]
mod commands {
    use std::process::ExitCode;

    use forge_config::{Environment, SecretString};
    use forge_db::{Backend, Database, Migrator};
    use tracing::{error, info, warn};

    use super::{CLOSE_TIMEOUT, DbCommand, EXIT_DATABASE_MISSING, ErrorChain, MigrationCommand};
    use crate::app::{write_stderr, write_stdout};

    pub(super) async fn run_migration_command<B: Backend>(
        app: &'static str,
        url: &SecretString,
        migrator: &Migrator,
        command: MigrationCommand,
        framework: &[Migrator],
    ) -> ExitCode {
        let database = match Database::<B>::connect(url).await {
            Ok(database) => database,
            Err(error) => {
                error!(app, error = %ErrorChain(&error), "failed to connect to the database");
                return ExitCode::FAILURE;
            }
        };
        let code = match command {
            MigrationCommand::Migrate => {
                let mut code = ExitCode::SUCCESS;
                for schema in framework {
                    code = migrate(app, &database, schema).await;
                    if code != ExitCode::SUCCESS {
                        break;
                    }
                }
                if code == ExitCode::SUCCESS {
                    migrate(app, &database, migrator).await
                } else {
                    code
                }
            }
            MigrationCommand::Status => {
                let mut code = ExitCode::SUCCESS;
                for schema in framework.iter().chain([migrator]) {
                    code = status(app, &database, schema).await;
                    if code != ExitCode::SUCCESS {
                        break;
                    }
                }
                code
            }
            MigrationCommand::Rollback { steps } => rollback(app, &database, migrator, steps).await,
        };
        if tokio::time::timeout(CLOSE_TIMEOUT, database.close())
            .await
            .is_err()
        {
            warn!(app, "timed out closing the migration connection pool");
        }
        code
    }

    async fn migrate<B: Backend>(
        app: &'static str,
        database: &Database<B>,
        migrator: &Migrator,
    ) -> ExitCode {
        match database.migrate(migrator).await {
            Ok(applied) if applied.is_empty() => {
                info!(app, "no pending migrations");
                ExitCode::SUCCESS
            }
            Ok(applied) => {
                for version in &applied {
                    info!(
                        app,
                        version,
                        description = description(migrator, *version),
                        "applied migration"
                    );
                }
                info!(app, count = applied.len(), "migrations applied");
                ExitCode::SUCCESS
            }
            Err(error) => {
                error!(app, error = %ErrorChain(&error), "migration failed");
                ExitCode::FAILURE
            }
        }
    }

    /// Prints one line per migration: `state version_description`.
    async fn status<B: Backend>(
        app: &'static str,
        database: &Database<B>,
        migrator: &Migrator,
    ) -> ExitCode {
        match database.migration_status(migrator).await {
            Ok(statuses) => {
                let mut text = String::new();
                if statuses
                    .iter()
                    .all(|status| status.state == forge_db::MigrationState::Pending)
                {
                    text.push_str("no migrations applied\n");
                }
                for status in statuses {
                    let name = status.description.replace(' ', "_");
                    text.push_str(&format!(
                        "{:<9} {}_{}\n",
                        status.state.as_str(),
                        status.version,
                        name
                    ));
                }
                write_stdout(&text)
            }
            Err(error) => {
                error!(app, error = %ErrorChain(&error), "reading the migration status failed");
                ExitCode::FAILURE
            }
        }
    }

    /// Reverts up to `steps` migrations, newest first, and stops at the first
    /// failure or when nothing is left to revert.
    async fn rollback<B: Backend>(
        app: &'static str,
        database: &Database<B>,
        migrator: &Migrator,
        steps: u32,
    ) -> ExitCode {
        for _ in 0..steps {
            match database.rollback(migrator).await {
                Ok(Some(version)) => {
                    info!(
                        app,
                        version,
                        description = description(migrator, version),
                        "reverted migration"
                    );
                }
                Ok(None) => {
                    info!(app, "no applied migrations to revert");
                    return ExitCode::SUCCESS;
                }
                Err(error) => {
                    error!(app, error = %ErrorChain(&error), "rollback failed");
                    return ExitCode::FAILURE;
                }
            }
        }
        ExitCode::SUCCESS
    }

    /// Why a drop needs `--force-production`, or `None` when it does not:
    /// only a local database in development or test is unguarded. `local`
    /// comes from the driver's reading of the URL, so a `host` query
    /// parameter cannot make a remote server look local.
    fn protection(environment: Environment, local: bool) -> Option<String> {
        match environment {
            Environment::Production => Some("FORGE_ENV is production".to_owned()),
            Environment::Development | Environment::Test if !local => {
                Some("the database server is not on this machine".to_owned())
            }
            Environment::Development | Environment::Test => None,
        }
    }

    pub(super) async fn run_admin_command<B: Backend>(
        url: &SecretString,
        command: DbCommand,
        environment: Environment,
    ) -> ExitCode {
        // Every decision below is made on what the driver would connect to.
        let target = match forge_db::target::<B>(url) {
            Ok(target) => target,
            Err(error) => {
                write_stderr(&format!("error: {}\n", ErrorChain(&error)));
                return ExitCode::FAILURE;
            }
        };
        let described = target.describe().to_owned();
        let failed = |action: &str, error: &(dyn std::error::Error + 'static)| {
            write_stderr(&format!(
                "error: could not {action} {described}: {}\n",
                ErrorChain(error)
            ));
            ExitCode::FAILURE
        };
        match command {
            DbCommand::Create => match forge_db::create_database::<B>(url).await {
                Ok(true) => write_stdout(&format!("created {described}\n")),
                Ok(false) => write_stdout(&format!("{described} already exists\n")),
                Err(error) => failed("create", &error),
            },
            DbCommand::Exists => match forge_db::database_exists::<B>(url).await {
                Ok(true) => write_stdout(&format!("{described} exists\n")),
                Ok(false) => {
                    let _ = write_stdout(&format!("{described} does not exist\n"));
                    ExitCode::from(EXIT_DATABASE_MISSING)
                }
                Err(error) => failed("look for", &error),
            },
            DbCommand::Drop { force_production } => {
                if let Some(reason) = protection(environment, target.is_local())
                    && !force_production
                {
                    write_stderr(&format!(
                        "error: refusing to drop {described}: {reason}; pass --force-production\n"
                    ));
                    return ExitCode::FAILURE;
                }
                match forge_db::drop_database::<B>(url).await {
                    Ok(true) => write_stdout(&format!("dropped {described}\n")),
                    Ok(false) => write_stdout(&format!("{described} does not exist\n")),
                    Err(error) => failed("drop", &error),
                }
            }
        }
    }

    fn description(migrator: &Migrator, version: i64) -> &str {
        migrator
            .iter()
            .find(|migration| {
                migration.version == version && migration.migration_type.is_up_migration()
            })
            .map_or("", |migration| migration.description.as_ref())
    }
    #[cfg(all(test, feature = "postgres"))]
    mod tests {
        use super::*;

        fn guarded(environment: Environment, url: &str) -> Option<String> {
            let target = forge_db::target::<forge_db::sqlx::Postgres>(&SecretString::new(url))
                .expect("well formed");
            protection(environment, target.is_local())
        }

        #[test]
        fn a_drop_is_guarded_unless_the_driver_would_connect_to_this_machine() {
            // Development and test are free only for a local server.
            for url in [
                "postgres://m:pw@localhost/shop",
                "postgres://m:pw@127.0.0.1/shop",
                "postgres:///shop?host=/var/run/postgresql",
            ] {
                assert_eq!(guarded(Environment::Development, url), None, "{url}");
                assert_eq!(guarded(Environment::Test, url), None, "{url}");
            }
            // Query parameters that move the connection elsewhere do not hide it.
            for url in [
                "postgres://m:pw@localhost/shop?host=prod.db.internal",
                "postgres://m:pw@localhost/shop?hostaddr=10.0.0.5",
                "postgres:///shop?host=prod",
                "postgres:///shop?host=",
                "postgres://m:pw@db.internal/shop",
            ] {
                let reason = guarded(Environment::Development, url).expect(url);
                assert!(reason.contains("not on this machine"), "{url}: {reason}");
            }
            // Production is guarded whatever the host.
            assert!(guarded(Environment::Production, "postgres://m:pw@localhost/shop").is_some());
        }

        #[test]
        fn a_database_name_override_cannot_reach_a_system_database() {
            let url = SecretString::new("postgres://m:pw@localhost/shop?dbname=postgres");
            let code = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime")
                .block_on(run_admin_command::<forge_db::sqlx::Postgres>(
                    &url,
                    DbCommand::Drop {
                        force_production: true,
                    },
                    Environment::Development,
                ));
            assert_eq!(code, ExitCode::FAILURE, "refused before any connection");
        }
    }
}
