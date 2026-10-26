//! PostgreSQL backend.

use std::str::FromStr as _;

use forge_config::{DatabaseScheme, database_url, is_loopback_host};
use forge_tenancy::TenantId;
use sqlx::Postgres;

use crate::{Backend, DbError, DbTransaction, backend::sealed::Hooks};

impl Hooks for Postgres {
    const SCHEME: DatabaseScheme = DatabaseScheme::Postgres;
    const TEST_URL_ENV: &'static str = "FORGE_TEST_DATABASE_URL";

    fn bound_statements(
        options: sqlx::pool::PoolOptions<Self>,
        millis: u64,
    ) -> sqlx::pool::PoolOptions<Self> {
        options.after_connect(move |connection, _| {
            Box::pin(async move {
                // Audited: `millis` is an integer Forge computed, not input.
                sqlx::Executor::execute(
                    connection,
                    sqlx::AssertSqlSafe(format!("SET statement_timeout = {millis}")),
                )
                .await
                .map(|_| ())
            })
        })
    }

    fn endpoint(url: &str) -> Result<crate::admin::Endpoint, crate::DbError> {
        // The parser that connects: `host`, `hostaddr`, `port` and `dbname`
        // query parameters, and PG* environment defaults, count.
        let options = sqlx::postgres::PgConnectOptions::from_str(url)
            .map_err(|_| crate::DbError::configuration(database_url::MALFORMED))?;
        Ok(endpoint_of(&options))
    }

    #[cfg(feature = "testing")]
    fn provision(
        admin_env: &str,
    ) -> impl std::future::Future<Output = Result<crate::testing::Provision, DbError>> + Send {
        crate::testing::provision_server(admin_env, create_database, drop_database)
    }
}

/// What `options` connect to. A host that starts with `/` is a socket
/// directory, whether it came from the URL, `PGHOST` or SQLx's default for a
/// URL without a host (`postgres:///shop` with no `PGHOST` resolves to the
/// first of `/var/run/postgresql`, `/private/tmp` and `/tmp` that holds a
/// server's socket, else `localhost`): all of these are this machine.
fn endpoint_of(options: &sqlx::postgres::PgConnectOptions) -> crate::admin::Endpoint {
    let host = options.get_host();
    // The connection prefers the socket whenever one is set.
    let socket = options.get_socket().is_some() || host.starts_with('/');
    crate::admin::Endpoint {
        database: options.get_database().map(str::to_owned),
        host: (!socket).then(|| host.to_owned()),
        port: (!socket).then(|| options.get_port()),
        local: socket || is_loopback_host(host),
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
pub(crate) fn create_database(
    admin: forge_config::SecretString,
    name: String,
) -> crate::testing::BoxFuture {
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

/// Drops a database of [`RlsTestDatabase`](crate::testing::RlsTestDatabase),
/// then its roles (a role cannot be dropped while a database refers to it).
/// Every statement is attempted; the first failure is returned.
#[cfg(feature = "testing")]
pub(crate) fn drop_rls_database(
    admin: forge_config::SecretString,
    name: String,
) -> crate::testing::BoxFuture {
    Box::pin(async move {
        let mut statements = vec![format!("DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)")];
        for role in crate::testing::rls_role_names(&name)
            .into_iter()
            .flatten()
            .rev()
        {
            statements.push(format!("DROP ROLE IF EXISTS \"{role}\""));
        }
        let mut outcome = Ok(());
        for statement in statements {
            let result = crate::testing::execute_admin::<Postgres>(&admin, statement).await;
            if outcome.is_ok() {
                outcome = result;
            }
        }
        outcome
    })
}

#[cfg(test)]
mod tests {
    use sqlx::postgres::PgConnectOptions;

    use super::*;

    /// The options of `url` as if `PGHOST` (or SQLx's default for a URL with
    /// no host) were `host`; `None` leaves what the URL says.
    fn options(url: &str, host: Option<&str>) -> PgConnectOptions {
        let options = PgConnectOptions::from_str(url).expect("parses");
        match host {
            Some(host) => options.host(host),
            None => options,
        }
    }

    #[test]
    fn a_url_without_a_host_is_local_when_the_driver_resolves_it_to_a_socket_or_localhost() {
        for host in [
            // SQLx's default when a socket file exists, and PGHOST=<dir>.
            "/var/run/postgresql",
            "/private/tmp",
            "/tmp",
            // SQLx's fallback when none does.
            "localhost",
            "127.0.0.1",
            "[::1]",
        ] {
            let endpoint = endpoint_of(&options("postgres:///shop", Some(host)));
            assert!(endpoint.local, "{host}");
            assert_eq!(endpoint.database.as_deref(), Some("shop"));
            if host.starts_with('/') {
                assert_eq!((endpoint.host, endpoint.port), (None, None), "{host}");
            }
        }
    }

    #[test]
    fn a_url_without_a_host_is_remote_when_pghost_names_another_machine() {
        for host in [
            "db.internal",
            "10.0.0.5",
            "localhost.evil.test",
            // Only a path starting with a slash is a socket.
            "var/run/postgresql",
        ] {
            let endpoint = endpoint_of(&options("postgres:///shop", Some(host)));
            assert!(!endpoint.local, "{host}");
            assert_eq!(endpoint.host.as_deref(), Some(host));
        }
    }

    #[test]
    fn explicit_hosts_keep_deciding_for_themselves() {
        assert!(!endpoint_of(&options("postgres://m:pw@db.internal/shop", None)).local);
        assert!(endpoint_of(&options("postgres://m:pw@127.0.0.1/shop", None)).local);
        assert!(endpoint_of(&options("postgres:///shop?host=/var/run/postgresql", None)).local);
        // A `host` query beats whatever PGHOST or the default would say.
        assert!(!endpoint_of(&options("postgres:///shop?host=prod", None)).local);
    }
}
