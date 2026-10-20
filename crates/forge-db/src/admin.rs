//! Server-level database administration: whether the application's database
//! exists, and creating or dropping it (`<app> db exists|create|drop`).
//!
//! These calls take the *whole URL of the application's database*, connect to
//! the server's maintenance database with the same credentials and TLS rules
//! (SQLx's [`MigrateDatabase`]), and never echo the URL: errors and
//! [`describe`] name the engine, database and host only. Names are limited to
//! what every engine and tool accepts, and the engines' own system databases
//! are never created or dropped.
//!
//! The role in the URL needs the privilege to create or drop databases
//! (`CREATEDB` on PostgreSQL). PostgreSQL drops terminate other sessions
//! first, so a running development server cannot block them.

use forge_config::{DatabaseScheme, SecretString};
use sqlx::migrate::MigrateError;

use crate::{Backend, DbError};

/// System databases that are never created or dropped.
const RESERVED: &[&str] = &[
    "postgres",
    "template0",
    "template1",
    "mysql",
    "information_schema",
    "performance_schema",
    "sys",
];

/// Whether the database named by `url` exists. Needs no privilege beyond
/// connecting to the server's maintenance database.
pub async fn database_exists<B: Backend>(url: &SecretString) -> Result<bool, DbError> {
    check::<B>(url)?;
    Ok(B::database_exists(url.expose()).await?)
}

/// Creates the database named by `url`. Returns `false`, changing nothing,
/// when it already exists.
pub async fn create_database<B: Backend>(url: &SecretString) -> Result<bool, DbError> {
    check::<B>(url)?;
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
    check::<B>(url)?;
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

/// Names the database for messages: engine, database and host, never a user
/// or password.
#[must_use]
pub fn describe(url: &SecretString) -> String {
    let url = url.expose();
    match DatabaseScheme::of_url(url) {
        Some(DatabaseScheme::Sqlite) => format!("sqlite database {}", sqlite_path(url)),
        Some(scheme) => {
            let engine = match scheme {
                DatabaseScheme::Postgres => "postgres",
                _ => "mysql",
            };
            let name = server_name(url).unwrap_or("?");
            match host_port(url) {
                Some(host) if !host.is_empty() => {
                    format!("{engine} database \"{name}\" on {host}")
                }
                _ => format!("{engine} database \"{name}\""),
            }
        }
        None => "database".to_owned(),
    }
}

/// Whether every server the URL names is on this machine: a SQLite file, a
/// loopback or `localhost` address, or a Unix socket.
#[must_use]
pub fn is_local(url: &SecretString) -> bool {
    let url = url.expose();
    if DatabaseScheme::of_url(url) == Some(DatabaseScheme::Sqlite) {
        return true;
    }
    let Some(authority) = authority(url) else {
        return false;
    };
    let host = host_of(authority);
    host.is_empty()
        || host.starts_with('/')
        || host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

fn check<B: Backend>(url: &SecretString) -> Result<(), DbError> {
    if <B as Backend>::SCHEME == DatabaseScheme::Sqlite {
        return Ok(());
    }
    let name = server_name(url.expose()).ok_or_else(|| {
        DbError::configuration(
            "the database name in the URL must be 1-63 letters, digits, underscores, \
             hyphens or dollar signs, and start with a letter, digit or underscore",
        )
    })?;
    if RESERVED
        .iter()
        .any(|reserved| name.eq_ignore_ascii_case(reserved))
    {
        return Err(DbError::configuration(format!(
            "refusing to touch the \"{name}\" system database"
        )));
    }
    Ok(())
}

/// The part of a server URL between `://` and the first `/`, `?` or `#`.
fn authority(url: &str) -> Option<&str> {
    let (_, rest) = url.split_once("://")?;
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    Some(&rest[..end])
}

/// `host[:port]` without the userinfo.
fn host_port(url: &str) -> Option<&str> {
    let authority = authority(url)?;
    Some(
        authority
            .rsplit_once('@')
            .map_or(authority, |(_, rest)| rest),
    )
}

/// The host of `authority`, without userinfo, port or IPv6 brackets.
fn host_of(authority: &str) -> &str {
    let host_port = authority
        .rsplit_once('@')
        .map_or(authority, |(_, rest)| rest);
    if let Some(bracketed) = host_port.strip_prefix('[') {
        return bracketed.split(']').next().unwrap_or_default();
    }
    host_port.split(':').next().unwrap_or_default()
}

/// The database name of a server URL, when it is acceptable: 1-63 characters
/// of `[A-Za-z0-9_$-]`, not starting with `$` or `-`.
fn server_name(url: &str) -> Option<&str> {
    let (_, rest) = url.split_once("://")?;
    let path = rest.find('/').map(|slash| &rest[slash + 1..])?;
    let name = path.split(['?', '#']).next()?;
    let mut characters = name.chars();
    let first = characters.next()?;
    let acceptable = (first.is_ascii_alphanumeric() || first == '_')
        && name.len() <= 63
        && characters.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '$' | '-'));
    acceptable.then_some(name)
}

/// The file of a `sqlite:` URL, without scheme or query.
fn sqlite_path(url: &str) -> &str {
    let rest = url.split_once(':').map_or(url, |(_, rest)| rest);
    let rest = rest.strip_prefix("//").unwrap_or(rest);
    rest.split('?').next().unwrap_or(rest)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secret(url: &str) -> SecretString {
        SecretString::new(url)
    }

    #[test]
    fn names_are_restricted_to_what_every_tool_accepts() {
        for good in ["app", "app_dev", "App-1", "_x", "a$b", &"a".repeat(63)] {
            let url = format!("postgres://u:p@h/{good}?sslmode=require");
            assert_eq!(server_name(&url), Some(good), "{good}");
        }
        for bad in [
            "",
            "-x",
            "$x",
            "a b",
            "a\"b",
            "a;b",
            "a%20b",
            &"a".repeat(64),
        ] {
            let url = format!("postgres://u:p@h/{bad}");
            assert_eq!(server_name(&url), None, "{bad:?}");
        }
        assert_eq!(server_name("postgres://u:p@h"), None);
    }

    #[cfg(feature = "postgres")]
    #[test]
    fn system_databases_and_bad_names_are_refused_without_echoing_the_url() {
        for name in ["postgres", "TEMPLATE1", "template0"] {
            let url = secret(&format!("postgres://admin:hunter2@db.internal/{name}"));
            let error = check::<sqlx::Postgres>(&url).expect_err("reserved");
            let message = error.to_string();
            assert!(message.contains("system database"), "{message}");
            assert!(!message.contains("hunter2") && !message.contains("db.internal"));
        }
        let url = secret("postgres://admin:hunter2@db.internal/bad;name");
        let message = check::<sqlx::Postgres>(&url).expect_err("name").to_string();
        assert!(message.contains("1-63 letters"), "{message}");
        assert!(!message.contains("hunter2"));
        assert!(check::<sqlx::Postgres>(&secret("postgres://u:p@h/shop_dev")).is_ok());
    }

    #[test]
    fn describe_never_includes_credentials_or_query() {
        let url = secret(
            "postgres://admin:hunter2@db.internal:5433/shop?password=hunter3&sslmode=require",
        );
        let text = describe(&url);
        assert_eq!(text, "postgres database \"shop\" on db.internal:5433");
        assert!(!text.contains("hunter"));
        assert_eq!(
            describe(&secret("mariadb://u:p@[::1]:3306/shop")),
            "mysql database \"shop\" on [::1]:3306"
        );
        assert_eq!(
            describe(&secret("sqlite://storage/dev.sqlite3?mode=rwc")),
            "sqlite database storage/dev.sqlite3"
        );
        assert_eq!(describe(&secret("sqlite:dev.db")), "sqlite database dev.db");
    }

    #[test]
    fn only_this_machine_is_local() {
        for local in [
            "postgres://u:p@localhost/app",
            "postgres://u:p@LOCALHOST:5432/app",
            "postgres://u:p@127.0.0.1/app",
            "postgres://u:p@[::1]:5432/app",
            "postgres:///app?host=/var/run/postgresql",
            "mysql://u:p@127.0.0.1:3306/app",
            "sqlite:storage/dev.db",
        ] {
            assert!(is_local(&secret(local)), "{local}");
        }
        for remote in [
            "postgres://u:p@db.example.com/app",
            "postgres://u:p@10.0.0.5/app",
            "postgres://u:p@localhost.evil.test/app",
            "postgres://localhost:5432@evil.test/app",
            "mysql://u:p@[2001:db8::1]/app",
        ] {
            assert!(!is_local(&secret(remote)), "{remote}");
        }
    }

    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn sqlite_databases_are_created_found_and_dropped() {
        let directory = tempfile::tempdir().expect("tempdir");
        let url = secret(&format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("dev.sqlite3").display()
        ));

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
