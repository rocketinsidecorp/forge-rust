//! MySQL 8.4+ and MariaDB 11.4+ backend (one wire protocol, one backend).

use std::str::FromStr as _;

use forge_config::{DatabaseScheme, database_url, is_loopback_host};
use sqlx::MySql;

use crate::{Backend, backend::sealed::Hooks};

impl Hooks for MySql {
    const SCHEME: DatabaseScheme = DatabaseScheme::MySql;
    const TEST_URL_ENV: &'static str = "FORGE_TEST_MYSQL_URL";

    fn bound_statements(
        options: sqlx::pool::PoolOptions<Self>,
        millis: u64,
    ) -> sqlx::pool::PoolOptions<Self> {
        options.after_connect(move |connection, _| {
            Box::pin(async move {
                // MariaDB reports e.g. "11.8.2-MariaDB-ubu2404", MySQL "8.4.5".
                let version: String = sqlx::query_scalar("SELECT VERSION()")
                    .fetch_one(&mut *connection)
                    .await?;
                // Audited: `millis` is an integer Forge computed, not input.
                sqlx::Executor::execute(
                    connection,
                    sqlx::AssertSqlSafe(statement_timeout_assignment(&version, millis)),
                )
                .await
                .map(|_| ())
            })
        })
    }

    fn endpoint(url: &str) -> Result<crate::admin::Endpoint, crate::DbError> {
        // The parser that connects: the `socket` query parameter counts.
        let options = sqlx::mysql::MySqlConnectOptions::from_str(url)
            .map_err(|_| crate::DbError::configuration(database_url::MALFORMED))?;
        let socket = options.get_socket().is_some();
        let host = options.get_host();
        Ok(crate::admin::Endpoint {
            database: options.get_database().map(str::to_owned),
            host: (!socket).then(|| host.to_owned()),
            port: (!socket).then(|| options.get_port()),
            local: socket || is_loopback_host(host),
        })
    }

    #[cfg(feature = "testing")]
    fn provision(
        admin_env: &str,
    ) -> impl std::future::Future<Output = Result<crate::testing::Provision, crate::DbError>> + Send
    {
        crate::testing::provision_server(admin_env, create_database, drop_database)
    }
}

impl Backend for MySql {
    const SCHEME: DatabaseScheme = DatabaseScheme::MySql;
}

/// The session statement bound of the server that reported `version`
/// (Forge-Go's `mysql.timeoutAssignment`): MariaDB's `max_statement_time`
/// (seconds) applies to every statement, MySQL's `max_execution_time`
/// (milliseconds) only to read-only `SELECT`s.
fn statement_timeout_assignment(version: &str, millis: u64) -> String {
    if version.to_ascii_lowercase().contains("mariadb") {
        format!(
            "SET SESSION max_statement_time = {}.{:03}",
            millis / 1000,
            millis % 1000
        )
    } else {
        format!("SET SESSION max_execution_time = {millis}")
    }
}

#[cfg(feature = "testing")]
fn create_database(admin: forge_config::SecretString, name: String) -> crate::testing::BoxFuture {
    Box::pin(async move {
        crate::testing::execute_admin::<MySql>(
            &admin,
            format!("CREATE DATABASE `{name}` CHARACTER SET utf8mb4"),
        )
        .await
    })
}

#[cfg(feature = "testing")]
fn drop_database(admin: forge_config::SecretString, name: String) -> crate::testing::BoxFuture {
    Box::pin(async move {
        crate::testing::execute_admin::<MySql>(&admin, format!("DROP DATABASE IF EXISTS `{name}`"))
            .await
    })
}

#[cfg(test)]
mod tests {
    use super::statement_timeout_assignment;

    #[test]
    fn the_statement_bound_follows_the_server_flavor() {
        assert_eq!(
            statement_timeout_assignment("11.8.2-MariaDB-ubu2404", 30_000),
            "SET SESSION max_statement_time = 30.000"
        );
        assert_eq!(
            statement_timeout_assignment("10.11.6-mariadb", 1),
            "SET SESSION max_statement_time = 0.001"
        );
        assert_eq!(
            statement_timeout_assignment("8.4.5", 1_500),
            "SET SESSION max_execution_time = 1500"
        );
    }
}
