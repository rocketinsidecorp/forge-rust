//! Server-level database administration: whether the application's database
//! exists, and creating or dropping it (`<app> db exists|create|drop`).
//!
//! These calls take the *whole URL of the application's database*, connect to
//! the server's maintenance database with the same credentials and TLS rules
//! (SQLx's `MigrateDatabase`), and never echo the URL: errors name the
//! engine, database and host only (see
//! [`forge_config::database_url`]). Names are limited to what every engine and
//! tool accepts, and the engines' own system databases are never created or
//! dropped.
//!
//! The role in the URL needs the privilege to create or drop databases
//! (`CREATEDB` on PostgreSQL). PostgreSQL drops terminate other sessions
//! first, so a running development server cannot block them.

use forge_config::{DatabaseScheme, SecretString, database_url};
use sqlx::migrate::MigrateError;

use crate::{Backend, DbError};

/// What a URL really names after the driver applied its query overrides and
/// environment defaults. Internal to the backends (it appears only in the
/// sealed `Hooks` trait).
#[doc(hidden)]
#[derive(Debug)]
pub struct Endpoint {
    pub(crate) database: Option<String>,
    /// `None` for a Unix socket or a file.
    pub(crate) host: Option<String>,
    pub(crate) port: Option<u16>,
    /// Whether every server touched is on this machine.
    pub(crate) local: bool,
}

/// The database a URL names, as the driver reads it: the host, port and
/// database that `host`, `hostaddr`, `port`, `dbname` or `socket` query
/// parameters may have changed. Decide on this, never on the URL's text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Target {
    description: String,
    database: Option<String>,
    local: bool,
}

impl Target {
    /// Engine, database and host for messages; never a user, password or query.
    #[must_use]
    pub fn describe(&self) -> &str {
        &self.description
    }

    /// Whether the server is this machine: a file, a Unix socket (only a path
    /// starting with `/` is one) or a loopback address.
    #[must_use]
    pub fn is_local(&self) -> bool {
        self.local
    }

    /// The database name the driver would use.
    #[must_use]
    pub fn database(&self) -> Option<&str> {
        self.database.as_deref()
    }
}

/// Resolves `url` with the same parser that will perform the operation.
///
/// # Errors
///
/// [`DbError::Configuration`] (never containing the URL) when the URL is not
/// well formed: unparseable, or with an `@` after its authority, which means
/// an unescaped `/`, `?` or `#` in the password.
pub fn target<B: Backend>(url: &SecretString) -> Result<Target, DbError> {
    if !database_url::is_well_formed(url) {
        return Err(DbError::configuration(database_url::MALFORMED));
    }
    if <B as Backend>::SCHEME == DatabaseScheme::Sqlite {
        return Ok(Target {
            description: database_url::describe(url),
            database: None,
            local: true,
        });
    }
    let endpoint = B::endpoint(url.expose())?;
    let engine = match <B as Backend>::SCHEME {
        DatabaseScheme::Postgres => "postgres",
        _ => "mysql",
    };
    Ok(Target {
        description: database_url::describe_location(
            engine,
            endpoint.database.as_deref(),
            endpoint.host.as_deref(),
            endpoint.port,
        ),
        database: endpoint.database,
        local: endpoint.local,
    })
}

/// Checks the effective database name of a server URL.
fn checked<B: Backend>(url: &SecretString) -> Result<Target, DbError> {
    let target = target::<B>(url)?;
    if <B as Backend>::SCHEME != DatabaseScheme::Sqlite {
        let name = target
            .database()
            .ok_or_else(|| DbError::configuration("the database URL does not name a database"))?;
        database_url::validate_name(name).map_err(DbError::configuration)?;
    }
    Ok(target)
}

/// Whether the database named by `url` exists. Needs no privilege beyond
/// connecting to the server's maintenance database.
pub async fn database_exists<B: Backend>(url: &SecretString) -> Result<bool, DbError> {
    checked::<B>(url)?;
    Ok(B::database_exists(url.expose()).await?)
}

/// Creates the database named by `url`. Returns `false`, changing nothing,
/// when it already exists.
pub async fn create_database<B: Backend>(url: &SecretString) -> Result<bool, DbError> {
    checked::<B>(url)?;
    if B::database_exists(url.expose()).await? {
        return Ok(false);
    }
    B::create_database(url.expose()).await?;
    Ok(true)
}

/// Drops the database named by `url` and everything in it. Returns `false`
/// when there was none. The caller owns the safety decision: this function
/// does not ask.
pub async fn drop_database<B: Backend>(url: &SecretString) -> Result<bool, DbError> {
    checked::<B>(url)?;
    if !B::database_exists(url.expose()).await? {
        return Ok(false);
    }
    match B::force_drop_database(url.expose()).await {
        Err(sqlx::Error::Migrate(error)) if matches!(*error, MigrateError::ForceNotSupported) => {
            B::drop_database(url.expose()).await?;
        }
        other => other?,
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secret(url: &str) -> SecretString {
        SecretString::new(url)
    }

    #[cfg(feature = "postgres")]
    mod postgres {
        use super::*;

        type Pg = sqlx::Postgres;

        #[test]
        fn names_come_from_the_driver_not_the_path() {
            // `dbname` overrides the path: the effective name is checked.
            for url in [
                "postgres://admin:hunter2@db.internal/shop?dbname=postgres",
                "postgres://admin:hunter2@db.internal/shop?dbname=template1",
                "postgres://admin:hunter2@db.internal/shop?dbname=a%3Bb",
                "postgres://admin:hunter2@db.internal/postgres",
                "postgres://admin:hunter2@db.internal/bad;name",
                "postgres://admin:hunter2@db.internal",
            ] {
                let error = checked::<Pg>(&secret(url)).expect_err(url).to_string();
                assert!(
                    !error.contains("hunter2") && !error.contains("db.internal"),
                    "{error}"
                );
            }
            let ok = checked::<Pg>(&secret("postgres://u:p@h/postgres?dbname=shop_dev"))
                .expect("the effective name is fine");
            assert_eq!(ok.database(), Some("shop_dev"));
        }

        #[test]
        fn query_parameters_cannot_make_a_remote_server_look_local() {
            for url in [
                "postgres://m:pw@localhost/shop?host=prod.db.internal",
                "postgres://m:pw@127.0.0.1/shop?hostaddr=10.0.0.5",
                "postgres:///shop?host=prod",
                "postgres:///shop?host=",
                "postgres://m:pw@localhost.evil.test/shop",
                "postgres://m:pw@10.0.0.5/shop",
            ] {
                let target = target::<Pg>(&secret(url)).expect(url);
                assert!(!target.is_local(), "{url}: {}", target.describe());
            }
        }

        #[test]
        fn an_empty_host_with_userinfo_is_refused() {
            // Not a valid URL; the driver would not read it either.
            assert!(target::<Pg>(&secret("postgres://m:pw@/shop?host=prod.db.internal")).is_err());
        }

        #[test]
        fn loopback_and_real_sockets_are_local() {
            for url in [
                "postgres://m:pw@localhost/shop",
                "postgres://m:pw@LOCALHOST:5433/shop",
                "postgres://m:pw@127.0.0.1/shop",
                "postgres://m:pw@[::1]/shop",
                "postgres:///shop?host=/var/run/postgresql",
                "postgres://m:pw@prod/shop?host=/var/run/postgresql",
                "postgres://%2Fvar%2Frun%2Fpostgresql/shop",
            ] {
                assert!(target::<Pg>(&secret(url)).expect(url).is_local(), "{url}");
            }
            // A host that merely looks like a path is not a socket.
            assert!(
                !target::<Pg>(&secret("postgres://m:pw@localhost/shop?host=var/run"))
                    .expect("parses")
                    .is_local()
            );
        }

        #[test]
        fn the_description_names_the_effective_server() {
            let target = target::<Pg>(&secret(
                "postgres://m:pw@localhost/shop?host=prod.db.internal&port=6543",
            ))
            .expect("parses");
            assert_eq!(
                target.describe(),
                "postgres database \"shop\" on prod.db.internal:6543"
            );
        }

        #[test]
        fn malformed_urls_are_refused_without_echoing_them() {
            for url in [
                "postgres://admin:s3cr/et@db.internal/shop",
                "postgres://admin:123/et@db.internal/shop",
                "postgres://admin:123?et@db.internal/shop",
                "postgres://admin:123#et@db.internal/shop",
            ] {
                let error = target::<Pg>(&secret(url)).expect_err(url).to_string();
                assert!(error.contains("malformed"), "{error}");
                for word in ["admin", "s3cr", "123", "et@", "db.internal"] {
                    assert!(!error.contains(word), "{word} in {error}");
                }
            }
        }
    }

    #[cfg(feature = "mysql")]
    #[test]
    fn mysql_sockets_are_local_and_other_hosts_are_not() {
        type My = sqlx::MySql;
        for url in [
            "mysql://u:p@localhost/shop",
            "mysql://u:p@prod/shop?socket=/tmp/mysql.sock",
        ] {
            assert!(target::<My>(&secret(url)).expect(url).is_local(), "{url}");
        }
        for url in [
            "mysql://u:p@db.internal/shop",
            "mariadb://u:p@10.0.0.5:3307/shop",
        ] {
            assert!(!target::<My>(&secret(url)).expect(url).is_local(), "{url}");
        }
        assert!(checked::<My>(&secret("mysql://u:p@h/mysql")).is_err());
    }

    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn sqlite_databases_are_created_found_and_dropped() {
        let directory = tempfile::tempdir().expect("tempdir");
        let url = secret(&format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("dev.sqlite3").display()
        ));
        assert!(target::<sqlx::Sqlite>(&url).expect("target").is_local());

        assert!(!database_exists::<sqlx::Sqlite>(&url).await.expect("exists"));
        assert!(create_database::<sqlx::Sqlite>(&url).await.expect("create"));
        assert!(database_exists::<sqlx::Sqlite>(&url).await.expect("exists"));
        assert!(
            !create_database::<sqlx::Sqlite>(&url).await.expect("again"),
            "creating twice changes nothing"
        );
        assert!(drop_database::<sqlx::Sqlite>(&url).await.expect("drop"));
        assert!(!database_exists::<sqlx::Sqlite>(&url).await.expect("exists"));
        assert!(
            !drop_database::<sqlx::Sqlite>(&url).await.expect("again"),
            "dropping a missing database changes nothing"
        );
    }
}
