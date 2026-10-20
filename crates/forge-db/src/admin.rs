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

fn check<B: Backend>(url: &SecretString) -> Result<(), DbError> {
    if <B as Backend>::SCHEME == DatabaseScheme::Sqlite {
        return Ok(());
    }
    database_url::manageable_name(url)
        .map(|_| ())
        .map_err(DbError::configuration)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secret(url: &str) -> SecretString {
        SecretString::new(url)
    }

    #[cfg(feature = "postgres")]
    #[test]
    fn system_databases_and_bad_names_are_refused_before_connecting() {
        for url in [
            "postgres://admin:hunter2@db.internal/postgres",
            "postgres://admin:hunter2@db.internal/bad;name",
        ] {
            let message = check::<sqlx::Postgres>(&secret(url))
                .expect_err("refused")
                .to_string();
            assert!(!message.contains("hunter2") && !message.contains("db.internal"));
        }
        assert!(check::<sqlx::Postgres>(&secret("postgres://u:p@h/shop_dev")).is_ok());
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
