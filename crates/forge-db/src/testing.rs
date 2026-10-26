//! Isolated databases for integration tests (feature `testing`).
//!
//! [`TestDatabase::create`] provisions an empty database named
//! `forge_test_<uuidv7>`, connects to it and applies the migrations. The
//! database is removed when the value is dropped, **including when the test
//! body panics**: the cleanup guard is an ordinary `Drop` that runs during
//! unwinding and performs the `DROP DATABASE` on a short-lived helper thread
//! with its own runtime (a `Drop` cannot await, and the test's runtime may be
//! shutting down). Call [`TestDatabase::cleanup`] to do the same
//! asynchronously and observe failures.
//!
//! | Backend | Administrative URL | Isolation |
//! | --- | --- | --- |
//! | PostgreSQL | `FORGE_TEST_DATABASE_URL` (a role that may create databases) | database `forge_test_<uuidv7>` |
//! | MySQL | `FORGE_TEST_MYSQL_URL` (same) | database `forge_test_<uuidv7>` |
//! | MariaDB | `FORGE_TEST_MARIADB_URL` via [`TestDatabase::create_from_env`] | database `forge_test_<uuidv7>` |
//! | SQLite | none | a file in a unique temporary directory |
//!
//! [`RlsTestDatabase`] (PostgreSQL) adds the migration and runtime roles of
//! a deployment, so repository tests run as the role an application serves
//! with and row-level security applies to them.
//!
//! Failures never print the administrative URL.

#[cfg(any(feature = "postgres", feature = "mysql"))]
use std::{future::Future, pin::Pin};

use forge_config::SecretString;
use sqlx::migrate::Migrator;
#[cfg(any(feature = "postgres", feature = "mysql"))]
use sqlx::{AssertSqlSafe, Connection, Executor};
#[cfg(any(feature = "postgres", feature = "mysql"))]
use tracing::warn;
#[cfg(any(feature = "postgres", feature = "mysql"))]
use uuid::Uuid;

use crate::{Backend, Database, DbError};

/// Environment variable with the administrative URL of a MariaDB server, for
/// [`TestDatabase::create_from_env`] with the MySQL backend.
pub const MARIADB_URL_ENV: &str = "FORGE_TEST_MARIADB_URL";

#[cfg(any(feature = "postgres", feature = "mysql"))]
/// A boxed administrative statement.
#[doc(hidden)]
pub type BoxFuture = Pin<Box<dyn Future<Output = Result<(), DbError>> + Send>>;

#[cfg(any(feature = "postgres", feature = "mysql"))]
type AdminFn = fn(SecretString, String) -> BoxFuture;

/// A freshly provisioned database and the guard that removes it.
#[doc(hidden)]
pub struct Provision {
    url: SecretString,
    cleanup: Cleanup,
}

enum Action {
    #[cfg(any(feature = "postgres", feature = "mysql"))]
    Server {
        admin: SecretString,
        name: String,
        drop: AdminFn,
    },
    #[cfg(feature = "sqlite")]
    Directory(tempfile::TempDir),
}

/// Removes the provisioned database when dropped, panic or not.
struct Cleanup(Option<Action>);

impl Cleanup {
    async fn run(&mut self) -> Result<(), DbError> {
        let Some(action) = self.0.take() else {
            return Ok(());
        };
        match action {
            #[cfg(any(feature = "postgres", feature = "mysql"))]
            Action::Server { admin, name, drop } => drop(admin, name).await,
            #[cfg(feature = "sqlite")]
            Action::Directory(directory) => directory
                .close()
                .map_err(|_| DbError::configuration("could not remove the test directory")),
        }
    }
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        // A directory removes itself when its `TempDir` drops.
        #[cfg(any(feature = "postgres", feature = "mysql"))]
        if let Some(Action::Server { admin, name, drop }) = self.0.take() {
            drop_server(admin, name, drop);
        }
    }
}

/// Drops a server database from a helper thread with its own runtime.
#[cfg(any(feature = "postgres", feature = "mysql"))]
fn drop_server(admin: SecretString, name: String, drop: AdminFn) {
    let database = name.clone();
    let outcome = std::thread::Builder::new()
        .name("forge-test-db-cleanup".to_owned())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|_| DbError::configuration("could not start the cleanup runtime"))?;
            runtime.block_on(drop(admin, name))
        })
        .map_err(|_| DbError::configuration("could not start the cleanup thread"))
        .and_then(|handle| {
            handle
                .join()
                .map_err(|_| DbError::configuration("the cleanup thread panicked"))?
        });
    if let Err(error) = outcome {
        warn!(database, %error, "failed to drop the test database");
    }
}

#[cfg(any(feature = "postgres", feature = "mysql"))]
/// Reads the administrative URL, creates `forge_test_<uuidv7>` and returns the
/// URL of the new database plus its cleanup guard.
pub(crate) async fn provision_server(
    admin_env: &str,
    create: AdminFn,
    drop: AdminFn,
) -> Result<Provision, DbError> {
    let admin = std::env::var(admin_env)
        .map(SecretString::new)
        .map_err(|_| DbError::configuration(format!("{admin_env} is not set")))?;
    let name = format!("forge_test_{}", Uuid::now_v7().simple());
    // Arm the guard before creating so a failure half way still cleans up.
    let cleanup = Cleanup(Some(Action::Server {
        admin: admin.clone(),
        name: name.clone(),
        drop,
    }));
    create(admin.clone(), name.clone()).await?;
    let url = with_database(admin.expose(), &name)
        .ok_or_else(|| DbError::configuration(format!("{admin_env} is not a valid URL")))?;
    Ok(Provision {
        url: SecretString::new(url),
        cleanup,
    })
}

#[cfg(any(feature = "postgres", feature = "mysql"))]
/// Replaces the database path of a `scheme://authority/database?query` URL,
/// keeping credentials, host and query parameters.
fn with_database(url: &str, name: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let (location, query) = match rest.split_once('?') {
        Some((location, query)) => (location, Some(query)),
        None => (rest, None),
    };
    let authority = location.split('/').next().filter(|a| !a.is_empty())?;
    let query = query.map(|query| format!("?{query}")).unwrap_or_default();
    Some(format!("{scheme}://{authority}/{name}{query}"))
}

/// A SQLite database in a unique temporary directory.
#[cfg(feature = "sqlite")]
pub(crate) fn provision_file() -> Result<Provision, DbError> {
    let directory = tempfile::Builder::new()
        .prefix("forge-test-")
        .tempdir()
        .map_err(|_| DbError::configuration("could not create a temporary directory"))?;
    let url = SecretString::new(format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("test.db").display()
    ));
    Ok(Provision {
        url,
        cleanup: Cleanup(Some(Action::Directory(directory))),
    })
}

#[cfg(any(feature = "postgres", feature = "mysql"))]
/// The pool settings of every test database: the test environment, so an
/// administrator may connect (no row-level security check).
#[must_use]
pub fn test_settings() -> crate::PoolSettings {
    crate::PoolSettings::default().with_environment(forge_config::Environment::Test)
}

/// Runs one administrative statement on a dedicated connection.
///
/// `sql` embeds only an identifier generated locally from UUID hex, never
/// caller input.
pub(crate) async fn execute_admin<B>(admin: &SecretString, sql: String) -> Result<(), DbError>
where
    B: Backend,
    for<'c> &'c mut <B as sqlx::Database>::Connection: Executor<'c, Database = B>,
    <B as sqlx::Database>::Arguments: sqlx::IntoArguments<B>,
{
    let mut connection = <<B as sqlx::Database>::Connection as Connection>::connect(admin.expose())
        .await
        .map_err(DbError::from)?;
    let result = sqlx::query(AssertSqlSafe(sql))
        .execute(&mut connection)
        .await
        .map(|_| ())
        .map_err(DbError::from);
    // The statement result matters; a failed close of a one-shot connection does not.
    drop(connection.close().await);
    result
}

/// An isolated, migrated database for one test.
///
/// See the [module documentation](self) for provisioning and cleanup.
pub struct TestDatabase<B: Backend> {
    database: Database<B>,
    url: SecretString,
    cleanup: Cleanup,
}

impl<B: Backend> std::fmt::Debug for TestDatabase<B> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TestDatabase")
            .field("database", &self.database)
            .finish_non_exhaustive()
    }
}

impl<B: Backend> TestDatabase<B> {
    /// Provisions a database for this backend and applies `migrator`, using
    /// the backend's default administrative variable (`FORGE_TEST_DATABASE_URL`
    /// for PostgreSQL, `FORGE_TEST_MYSQL_URL` for MySQL; none for SQLite).
    pub async fn create(migrator: &Migrator) -> Result<Self, DbError> {
        Self::create_from_env(<B as crate::backend::sealed::Hooks>::TEST_URL_ENV, migrator).await
    }

    /// Like [`TestDatabase::create`] with an explicit administrative
    /// variable, e.g. [`MARIADB_URL_ENV`] to test the MySQL backend against
    /// MariaDB.
    pub async fn create_from_env(admin_env: &str, migrator: &Migrator) -> Result<Self, DbError> {
        let test = Self::create_empty_from_env(admin_env).await?;
        test.database.migrate(migrator).await?;
        Ok(test)
    }

    /// Provisions an empty database (no migrations) with the default
    /// administrative variable.
    pub async fn create_empty() -> Result<Self, DbError> {
        Self::create_empty_from_env(<B as crate::backend::sealed::Hooks>::TEST_URL_ENV).await
    }

    /// Provisions an empty database with an explicit administrative variable.
    pub async fn create_empty_from_env(admin_env: &str) -> Result<Self, DbError> {
        let Provision { url, cleanup } =
            <B as crate::backend::sealed::Hooks>::provision(admin_env).await?;
        // From here on `cleanup` removes the database on every exit path.
        // A test database is the test environment's: an administrator may
        // connect (no row-level security check).
        let database = Database::connect_with(&url, test_settings()).await?;
        Ok(Self {
            database,
            url,
            cleanup,
        })
    }

    /// The migrated database.
    #[must_use]
    pub fn database(&self) -> &Database<B> {
        &self.database
    }

    /// The URL of the isolated database (contains credentials; handle as a
    /// secret).
    #[must_use]
    pub fn url(&self) -> &SecretString {
        &self.url
    }

    /// Closes the pool and removes the database, reporting failures.
    pub async fn cleanup(mut self) -> Result<(), DbError> {
        self.database.close().await;
        self.cleanup.run().await
    }
}

/// The database name prefix of [`provision_server`].
#[cfg(feature = "postgres")]
const DATABASE_PREFIX: &str = "forge_test_";

/// The migration, runtime and worker role names of an [`RlsTestDatabase`]
/// whose database is `database`, derived from its unique suffix so the
/// cleanup guard can drop them from the name alone.
#[cfg(feature = "postgres")]
pub(crate) fn rls_role_names(database: &str) -> Option<[String; 3]> {
    let suffix = database.strip_prefix(DATABASE_PREFIX)?;
    let generated = !suffix.is_empty()
        && suffix
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    generated.then(|| {
        ["migrator", "runtime", "worker"].map(|kind| format!("forge_test_{kind}_{suffix}"))
    })
}

/// `url` (`scheme://[user[:password]@]host[:port][/database][?query]`) with
/// its user-info replaced. Query parameters are kept; one that names the user
/// is caught by [`RlsTestDatabase`]'s login check.
#[cfg(feature = "postgres")]
fn with_credentials(url: &str, user: &str, password: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let end = rest.find(['/', '?']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    Some(format!("{scheme}://{user}:{password}@{host}{tail}"))
}

/// A migrated PostgreSQL test database with the roles of a deployment that
/// isolates tenants with row-level security (ADR 0011, ADR 0044), so tests
/// see the policies exactly as the serving application does. Forge-Go's
/// `postgrestest.NewRLS`.
///
/// [`RlsTestDatabase::create`] provisions `forge_test_<uuidv7>` with
/// `FORGE_TEST_DATABASE_URL` (a superuser, as in the generated Compose
/// service and CI: it creates roles and sets their default privileges), then
/// creates login roles named after the database, with a random password:
///
/// - `forge_test_migrator_<suffix>` owns the database and the `public`
///   schema and applies the migrations, like `FORGE_MIGRATION_DATABASE_URL`'s
///   role ([`migrator`](Self::migrator));
/// - `forge_test_runtime_<suffix>` is what an application serves with
///   (`FORGE_DATABASE_URL`, [`runtime`](Self::runtime)): `NOSUPERUSER
///   NOBYPASSRLS NOCREATEDB NOCREATEROLE`, owns nothing, may not create
///   temporary tables, and holds `SELECT, INSERT, UPDATE, DELETE` on the
///   tables and `USAGE, SELECT, UPDATE` on the sequences the migrator
///   creates, through the migrator's default privileges, as the generated
///   `docker/postgres/init.sql` grants them to `app_runtime`. The database
///   setting `forge.runtime_role` names it, so framework migrations (jobs,
///   outbox) grant it what they grant `app_runtime` and revoke the rest;
/// - with [`create_with_worker`](Self::create_with_worker),
///   `forge_test_worker_<suffix>`, named in `forge.worker_role`, receives
///   the worker and operator functions instead of the runtime role
///   ([`worker`](Self::worker)).
///
/// The database and the roles are dropped when the value is dropped, even
/// when the test panics, or by [`cleanup`](Self::cleanup).
///
/// Migrations that grant to `app_runtime` by name (the generated identity
/// tables narrow its privileges that way) do not reach the per-test runtime
/// role, which keeps the default privileges on those tables: a test can only
/// be more permissive than production there, never stricter.
#[cfg(feature = "postgres")]
pub struct RlsTestDatabase {
    admin: Database<sqlx::Postgres>,
    migrator: Database<sqlx::Postgres>,
    runtime: Database<sqlx::Postgres>,
    worker: Option<Database<sqlx::Postgres>>,
    migrator_url: SecretString,
    runtime_url: SecretString,
    migrator_role: String,
    runtime_role: String,
    cleanup: Cleanup,
}

#[cfg(feature = "postgres")]
impl std::fmt::Debug for RlsTestDatabase {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RlsTestDatabase")
            .field("migrator_role", &self.migrator_role)
            .field("runtime_role", &self.runtime_role)
            .field("separate_worker", &self.worker.is_some())
            .finish_non_exhaustive()
    }
}

#[cfg(feature = "postgres")]
impl RlsTestDatabase {
    /// Provisions the database and roles and applies `migrators` in order
    /// as the migration role: `create([&MIGRATOR])`, or
    /// `create(&[framework, application])`.
    pub async fn create<'a>(
        migrators: impl IntoIterator<Item = &'a Migrator>,
    ) -> Result<Self, DbError> {
        Self::provision(migrators.into_iter().collect(), false).await
    }

    /// Like [`RlsTestDatabase::create`] with a separate worker role.
    pub async fn create_with_worker<'a>(
        migrators: impl IntoIterator<Item = &'a Migrator>,
    ) -> Result<Self, DbError> {
        Self::provision(migrators.into_iter().collect(), true).await
    }

    async fn provision(migrators: Vec<&Migrator>, separate_worker: bool) -> Result<Self, DbError> {
        let admin_env = <sqlx::Postgres as crate::backend::sealed::Hooks>::TEST_URL_ENV;
        let Provision { url, cleanup } = provision_server(
            admin_env,
            crate::postgres::create_database,
            crate::postgres::drop_rls_database,
        )
        .await?;
        // From here on `cleanup` drops the database and the roles.
        let admin = Database::<sqlx::Postgres>::connect_with(&url, test_settings()).await?;
        let name: String = sqlx::query_scalar("SELECT current_database()")
            .fetch_one(admin.pool())
            .await?;
        let [migrator_role, runtime_role, worker_role] = rls_role_names(&name)
            .ok_or_else(|| DbError::configuration("unexpected test database name"))?;
        let password = Uuid::now_v7().simple().to_string();
        let create_role = |role: &str| {
            format!(
                "CREATE ROLE \"{role}\" LOGIN PASSWORD '{password}' \
                 NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS"
            )
        };
        // Audited: names and the password are generated above, never input.
        let mut statements = vec![
            create_role(&migrator_role),
            create_role(&runtime_role),
            format!("ALTER DATABASE \"{name}\" SET forge.runtime_role = '{runtime_role}'"),
            format!("ALTER DATABASE \"{name}\" OWNER TO \"{migrator_role}\""),
            format!("REVOKE CONNECT, TEMPORARY ON DATABASE \"{name}\" FROM PUBLIC"),
            format!(
                "GRANT CONNECT ON DATABASE \"{name}\" TO \"{migrator_role}\", \"{runtime_role}\""
            ),
            "REVOKE ALL ON SCHEMA public FROM PUBLIC".to_owned(),
            format!("GRANT ALL ON SCHEMA public TO \"{migrator_role}\""),
            format!("GRANT USAGE ON SCHEMA public TO \"{runtime_role}\""),
            format!(
                "ALTER DEFAULT PRIVILEGES FOR ROLE \"{migrator_role}\" IN SCHEMA public \
                 GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO \"{runtime_role}\""
            ),
            format!(
                "ALTER DEFAULT PRIVILEGES FOR ROLE \"{migrator_role}\" IN SCHEMA public \
                 GRANT USAGE, SELECT, UPDATE ON SEQUENCES TO \"{runtime_role}\""
            ),
        ];
        if separate_worker {
            // Only what reaching the functions the migrations grant it needs.
            statements.extend([
                create_role(&worker_role),
                format!("ALTER DATABASE \"{name}\" SET forge.worker_role = '{worker_role}'"),
                format!("GRANT CONNECT ON DATABASE \"{name}\" TO \"{worker_role}\""),
                format!("GRANT USAGE ON SCHEMA public TO \"{worker_role}\""),
            ]);
        }
        for statement in statements {
            sqlx::query(AssertSqlSafe(statement))
                .execute(admin.pool())
                .await?;
        }
        let login = |role: &str| {
            with_credentials(url.expose(), role, &password)
                .map(SecretString::new)
                .ok_or_else(|| DbError::configuration(format!("{admin_env} is not a valid URL")))
        };
        let (migrator_url, runtime_url) = (login(&migrator_role)?, login(&runtime_role)?);
        let migrator = connect_as(admin_env, &migrator_url, &migrator_role).await?;
        for migrations in migrators {
            migrator.migrate(migrations).await?;
        }
        let runtime = connect_as(admin_env, &runtime_url, &runtime_role).await?;
        let worker = if separate_worker {
            Some(connect_as(admin_env, &login(&worker_role)?, &worker_role).await?)
        } else {
            None
        };
        Ok(Self {
            admin,
            migrator,
            runtime,
            worker,
            migrator_url,
            runtime_url,
            migrator_role,
            runtime_role,
            cleanup,
        })
    }

    /// The test server's own user on the database, for setup and inspection;
    /// it bypasses row-level security.
    #[must_use]
    pub fn admin(&self) -> &Database<sqlx::Postgres> {
        &self.admin
    }

    /// The migration role, owner of the database, its schema and every table.
    #[must_use]
    pub fn migrator(&self) -> &Database<sqlx::Postgres> {
        &self.migrator
    }

    /// The runtime role an application serves with.
    #[must_use]
    pub fn runtime(&self) -> &Database<sqlx::Postgres> {
        &self.runtime
    }

    /// The role that runs workers and operator commands: the separate worker
    /// role with [`RlsTestDatabase::create_with_worker`], the runtime role
    /// otherwise.
    #[must_use]
    pub fn worker(&self) -> &Database<sqlx::Postgres> {
        self.worker.as_ref().unwrap_or(&self.runtime)
    }

    /// The migration role's URL (contains credentials; handle as a secret).
    #[must_use]
    pub fn migrator_url(&self) -> &SecretString {
        &self.migrator_url
    }

    /// The runtime role's URL, for code that opens its own pool (contains
    /// credentials; handle as a secret).
    #[must_use]
    pub fn runtime_url(&self) -> &SecretString {
        &self.runtime_url
    }

    /// The migration role's name (lowercase letters, digits and `_`).
    #[must_use]
    pub fn migrator_role(&self) -> &str {
        &self.migrator_role
    }

    /// The runtime role's name (lowercase letters, digits and `_`).
    #[must_use]
    pub fn runtime_role(&self) -> &str {
        &self.runtime_role
    }

    /// Closes the pools and drops the database and the roles, reporting
    /// failures.
    pub async fn cleanup(mut self) -> Result<(), DbError> {
        for database in [&self.runtime, &self.migrator, &self.admin]
            .into_iter()
            .chain(self.worker.as_ref())
        {
            database.close().await;
        }
        self.cleanup.run().await
    }
}

/// Connects with `url` and checks that the session logged in as `role`: a
/// `user` query parameter in the administrative URL would otherwise win over
/// the replaced user-info and leave the test running as the administrator.
#[cfg(feature = "postgres")]
async fn connect_as(
    admin_env: &str,
    url: &SecretString,
    role: &str,
) -> Result<Database<sqlx::Postgres>, DbError> {
    let database = Database::<sqlx::Postgres>::connect_with(url, test_settings()).await?;
    let login: String = sqlx::query_scalar("SELECT session_user")
        .fetch_one(database.pool())
        .await?;
    if login != role {
        database.close().await;
        return Err(DbError::configuration(format!(
            "{admin_env} must name its user in the URL's user-info, not in a query parameter"
        )));
    }
    Ok(database)
}

#[cfg(all(test, any(feature = "postgres", feature = "mysql")))]
mod tests {
    use super::*;

    #[test]
    fn database_path_is_replaced_and_the_rest_kept() {
        assert_eq!(
            with_database(
                "postgres://u:p%40@h:5432/postgres?sslmode=disable",
                "forge_test_1"
            )
            .as_deref(),
            Some("postgres://u:p%40@h:5432/forge_test_1?sslmode=disable")
        );
        assert_eq!(
            with_database("mariadb://root:forge@127.0.0.1:3306", "x").as_deref(),
            Some("mariadb://root:forge@127.0.0.1:3306/x")
        );
        assert_eq!(with_database("sqlite:file.db", "x"), None);
        assert_eq!(with_database("postgres:///db", "x"), None);
    }

    #[cfg(feature = "postgres")]
    #[test]
    fn credentials_are_replaced_and_the_rest_kept() {
        assert_eq!(
            with_credentials(
                "postgres://u:p%40@h:5432/forge_test_1?sslmode=disable",
                "r",
                "pw"
            )
            .as_deref(),
            Some("postgres://r:pw@h:5432/forge_test_1?sslmode=disable")
        );
        assert_eq!(
            with_credentials("postgres://h/db", "r", "pw").as_deref(),
            Some("postgres://r:pw@h/db")
        );
        assert_eq!(
            with_credentials("postgresql://u@h?dbname=x", "r", "pw").as_deref(),
            Some("postgresql://r:pw@h?dbname=x")
        );
        assert_eq!(with_credentials("sqlite:file.db", "r", "pw"), None);
    }

    #[cfg(feature = "postgres")]
    #[test]
    fn role_names_derive_only_from_generated_database_names() {
        let suffix = Uuid::now_v7().simple().to_string();
        let [migrator, runtime, worker] =
            rls_role_names(&format!("forge_test_{suffix}")).expect("generated name");
        assert_eq!(migrator, format!("forge_test_migrator_{suffix}"));
        assert_eq!(runtime, format!("forge_test_runtime_{suffix}"));
        assert_eq!(worker, format!("forge_test_worker_{suffix}"));
        assert!(migrator.len() <= 63, "PostgreSQL truncates longer names");
        for name in [
            "postgres",
            "forge_test_",
            "forge_test_X1",
            "forge_test_a\"b",
        ] {
            assert_eq!(rls_role_names(name), None, "{name}");
        }
    }
}
