//! A PostgreSQL test database with least-privilege roles (feature
//! `testing`).
//!
//! [`RlsTestDatabase::create`] provisions an isolated database through
//! [`TestDatabase`] (`FORGE_TEST_DATABASE_URL`), then creates two
//! uniquely named roles, exactly like ADR 0011 deployments:
//!
//! - a **migrator** that owns the database and applies the migrations;
//! - a **runtime** role: `NOSUPERUSER NOBYPASSRLS`, not an owner, named in
//!   the database's `forge.runtime_role` setting so the migrations grant it
//!   its privileges.
//!
//! Row-level security therefore applies to [`RlsTestDatabase::runtime`]
//! exactly as in production. The database and both roles are removed by
//! [`RlsTestDatabase::cleanup`], or when the value is dropped (also during a
//! panic).

use forge_config::SecretString;
use forge_db::{DbError, Migrator, PgDatabase, testing::TestDatabase};
use sqlx::{AssertSqlSafe, Postgres};
use uuid::Uuid;

/// See the [module documentation](self).
#[derive(Debug)]
pub struct RlsTestDatabase {
    // Field order is drop order: the database goes before its roles.
    test: TestDatabase<Postgres>,
    migrator: PgDatabase,
    runtime: PgDatabase,
    runtime_role: String,
    roles: RoleGuard,
}

impl RlsTestDatabase {
    /// Provisions the database and roles and applies `migrators` in order
    /// as the migrator role.
    pub async fn create(migrators: &[Migrator]) -> Result<Self, DbError> {
        let admin_url = std::env::var("FORGE_TEST_DATABASE_URL")
            .map(SecretString::new)
            .map_err(|_| configuration("FORGE_TEST_DATABASE_URL is not set"))?;
        let test = TestDatabase::<Postgres>::create_empty().await?;
        let suffix = Uuid::now_v7().simple().to_string();
        // The whole UUID: its leading digits are mostly the timestamp, and
        // fixtures created in the same millisecond must not collide.
        let migrator_role = format!("forge_test_migrator_{suffix}");
        let runtime_role = format!("forge_test_runtime_{suffix}");
        let password = Uuid::now_v7().simple().to_string();
        let roles = RoleGuard {
            admin: admin_url,
            names: vec![migrator_role.clone(), runtime_role.clone()],
            dropped: false,
        };
        let admin = test.database();
        let database: String = sqlx::query_scalar("SELECT current_database()")
            .fetch_one(admin.pool())
            .await?;
        // Identifiers and the password are generated above, never input.
        for statement in [
            format!(
                "CREATE ROLE \"{migrator_role}\" LOGIN PASSWORD '{password}' \
                 NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS"
            ),
            format!(
                "CREATE ROLE \"{runtime_role}\" LOGIN PASSWORD '{password}' \
                 NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS"
            ),
            format!("ALTER DATABASE \"{database}\" OWNER TO \"{migrator_role}\""),
            format!("ALTER DATABASE \"{database}\" SET forge.runtime_role = '{runtime_role}'"),
        ] {
            sqlx::query(AssertSqlSafe(statement))
                .execute(admin.pool())
                .await?;
        }
        let url = |role: &str| {
            with_credentials(test.url().expose(), role, &password)
                .map(SecretString::new)
                .ok_or_else(|| configuration("FORGE_TEST_DATABASE_URL is not a valid URL"))
        };
        let migrator = PgDatabase::connect(&url(&migrator_role)?).await?;
        for migrations in migrators {
            migrator.migrate(migrations).await?;
        }
        let runtime = PgDatabase::connect(&url(&runtime_role)?).await?;
        Ok(Self {
            test,
            migrator,
            runtime,
            runtime_role,
            roles,
        })
    }

    /// A superuser connection to the test database, for inspection.
    #[must_use]
    pub fn admin(&self) -> &PgDatabase {
        self.test.database()
    }

    /// The schema owner's pool (the role that applied the migrations).
    #[must_use]
    pub const fn migrator(&self) -> &PgDatabase {
        &self.migrator
    }

    /// The least-privilege runtime pool.
    #[must_use]
    pub const fn runtime(&self) -> &PgDatabase {
        &self.runtime
    }

    /// The runtime role's name.
    #[must_use]
    pub fn runtime_role(&self) -> &str {
        &self.runtime_role
    }

    /// Closes the pools, drops the database and then the roles.
    pub async fn cleanup(self) -> Result<(), DbError> {
        let Self {
            test,
            migrator,
            runtime,
            runtime_role: _,
            mut roles,
        } = self;
        migrator.close().await;
        runtime.close().await;
        test.cleanup().await?;
        roles.drop_roles().await
    }
}

/// Drops the roles, from a helper thread when dropped without `cleanup`.
#[derive(Debug)]
struct RoleGuard {
    admin: SecretString,
    names: Vec<String>,
    dropped: bool,
}

impl RoleGuard {
    async fn drop_roles(&mut self) -> Result<(), DbError> {
        if self.dropped {
            return Ok(());
        }
        self.dropped = true;
        let admin = PgDatabase::connect(&self.admin).await?;
        let mut outcome = Ok(());
        for name in &self.names {
            let statement = format!("DROP ROLE IF EXISTS \"{name}\"");
            if let Err(error) = sqlx::query(AssertSqlSafe(statement))
                .execute(admin.pool())
                .await
            {
                outcome = Err(DbError::from(error));
            }
        }
        admin.close().await;
        outcome
    }
}

impl Drop for RoleGuard {
    fn drop(&mut self) {
        if self.dropped {
            return;
        }
        let mut guard = Self {
            admin: self.admin.clone(),
            names: std::mem::take(&mut self.names),
            dropped: false,
        };
        self.dropped = true;
        // A `Drop` cannot await and the test runtime may be shutting down.
        let joined = std::thread::Builder::new()
            .name("forge-test-role-cleanup".to_owned())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|_| configuration("could not start the cleanup runtime"))?;
                runtime.block_on(guard.drop_roles())
            })
            .map(std::thread::JoinHandle::join);
        if !matches!(joined, Ok(Ok(Ok(())))) {
            tracing::warn!("failed to drop the test roles");
        }
    }
}

fn configuration(reason: &str) -> DbError {
    DbError::Configuration {
        reason: reason.to_owned(),
    }
}

/// Replaces the user and password of a `scheme://user:pass@host/...` URL.
fn with_credentials(url: &str, user: &str, password: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let location = rest.rsplit_once('@').map_or(rest, |(_, location)| location);
    Some(format!("{scheme}://{user}:{password}@{location}"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn credentials_are_replaced() {
        assert_eq!(
            super::with_credentials("postgres://postgres:pw@127.0.0.1:5432/db?x=1", "u", "p")
                .as_deref(),
            Some("postgres://u:p@127.0.0.1:5432/db?x=1")
        );
        assert_eq!(
            super::with_credentials("postgres://127.0.0.1/db", "u", "p").as_deref(),
            Some("postgres://u:p@127.0.0.1/db")
        );
        assert_eq!(super::with_credentials("nonsense", "u", "p"), None);
    }
}
