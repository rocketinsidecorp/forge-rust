//! PostgreSQL backend.

use std::str::FromStr as _;

use forge_config::{DatabaseScheme, database_url, is_loopback_host};
use forge_tenancy::TenantId;
use sqlx::Postgres;

use crate::{Backend, DbError, DbTransaction, backend::sealed::Hooks};

impl Hooks for Postgres {
    const SCHEME: DatabaseScheme = DatabaseScheme::Postgres;
    const TEST_URL_ENV: &'static str = "FORGE_TEST_DATABASE_URL";

    fn endpoint(url: &str) -> Result<crate::admin::Endpoint, crate::DbError> {
        // The parser that connects: `host`, `hostaddr`, `port` and `dbname`
        // query parameters, and PG* environment defaults, count.
        let options = sqlx::postgres::PgConnectOptions::from_str(url)
            .map_err(|_| crate::DbError::configuration(database_url::MALFORMED))?;
        let socket = options.get_socket().is_some();
        let host = options.get_host();
        Ok(crate::admin::Endpoint {
            database: options.get_database().map(str::to_owned),
            host: (!socket).then(|| host.to_owned()),
            port: (!socket).then(|| options.get_port()),
            // The connection prefers the socket whenever one is set.
            local: socket || is_loopback_host(host),
        })
    }

    #[cfg(feature = "testing")]
    fn provision(
        admin_env: &str,
    ) -> impl std::future::Future<Output = Result<crate::testing::Provision, DbError>> + Send {
        crate::testing::provision_server(admin_env, create_database, drop_database)
    }
}

impl Backend for Postgres {
    const SCHEME: DatabaseScheme = DatabaseScheme::Postgres;
}

/// A stable 64-bit key for [`DbTransaction::advisory_xact_lock`], derived from
/// a name with FNV-1a.
///
/// The same name yields the same key in every process and release, so
/// independent services can agree on a lock by name. It is a lock identifier,
/// not a security primitive.
///
/// ```
/// # #[cfg(feature = "postgres")] {
/// use forge_db::advisory_key;
///
/// assert_eq!(advisory_key("billing:close-month"), advisory_key("billing:close-month"));
/// assert_ne!(advisory_key("a"), advisory_key("b"));
/// # }
/// ```
#[must_use]
pub fn advisory_key(name: &str) -> i64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let hash = name.bytes().fold(OFFSET, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(PRIME)
    });
    i64::from_ne_bytes(hash.to_ne_bytes())
}

impl DbTransaction<Postgres> {
    /// Takes a transaction-scoped advisory lock (`pg_advisory_xact_lock`),
    /// waiting until it is available. The lock is released automatically when
    /// the transaction commits or rolls back, so it cannot leak through the
    /// pool. PostgreSQL only; see [`advisory_key`].
    pub async fn advisory_xact_lock(&mut self, key: i64) -> Result<(), DbError> {
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(key)
            .execute(self.connection())
            .await?;
        Ok(())
    }

    /// Installs `tenant` as this transaction's `forge.tenant_id` setting
    /// (`set_config(..., true)`, ADR 0011), the value the generated
    /// row-level security policies compare with. The setting is
    /// transaction-local, so it never leaks to the next user of the pooled
    /// connection. Call it before the first tenant-scoped statement.
    /// PostgreSQL only.
    pub async fn set_tenant(&mut self, tenant: TenantId) -> Result<(), DbError> {
        sqlx::query("SELECT set_config('forge.tenant_id', $1, true)")
            .bind(tenant.to_string())
            .execute(self.connection())
            .await?;
        Ok(())
    }
}

#[cfg(feature = "testing")]
fn create_database(admin: forge_config::SecretString, name: String) -> crate::testing::BoxFuture {
    Box::pin(async move {
        crate::testing::execute_admin::<Postgres>(&admin, format!("CREATE DATABASE \"{name}\""))
            .await
    })
}

#[cfg(feature = "testing")]
fn drop_database(admin: forge_config::SecretString, name: String) -> crate::testing::BoxFuture {
    Box::pin(async move {
        // FORCE terminates connections still held by a pool that outlived a
        // panicking test (PostgreSQL 13+).
        crate::testing::execute_admin::<Postgres>(
            &admin,
            format!("DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)"),
        )
        .await
    })
}
