//! SQLite backend.
//!
//! Every connection enables foreign keys, uses write-ahead logging (readers do
//! not block the writer) and waits up to five seconds for a lock before
//! failing with a retryable busy error. File creation follows the URL:
//! `sqlite:path` fails if the file is missing, `?mode=rwc` creates it.

use std::time::Duration;

use forge_config::DatabaseScheme;
use sqlx::{
    Sqlite,
    pool::PoolOptions,
    sqlite::{SqliteConnectOptions, SqliteJournalMode},
};

use crate::{
    Backend,
    backend::{ConnectOptions, sealed::Hooks},
};

/// How long a connection waits for a lock held by another connection.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

impl Hooks for Sqlite {
    const SCHEME: DatabaseScheme = DatabaseScheme::Sqlite;
    const TEST_URL_ENV: &'static str = "";

    fn endpoint(_url: &str) -> Result<crate::admin::Endpoint, crate::DbError> {
        // A file on this machine.
        Ok(crate::admin::Endpoint {
            database: None,
            host: None,
            port: None,
            local: true,
        })
    }

    fn configure(url: &str, options: ConnectOptions<Self>) -> ConnectOptions<Self> {
        let options: SqliteConnectOptions = options.foreign_keys(true).busy_timeout(BUSY_TIMEOUT);
        match Location::of(url) {
            // WAL needs a writable file; memory databases have no journal file.
            Location::File => options.journal_mode(SqliteJournalMode::Wal),
            Location::Memory | Location::ReadOnly => options,
        }
    }

    fn pool_options(url: &str, options: PoolOptions<Self>) -> PoolOptions<Self> {
        match Location::of(url) {
            // A memory database lives only as long as a connection holds it:
            // pin exactly one connection that is never recycled.
            Location::Memory => options
                .max_connections(1)
                .min_connections(1)
                .idle_timeout(None)
                .max_lifetime(None),
            Location::File | Location::ReadOnly => options,
        }
    }

    #[cfg(feature = "testing")]
    fn provision(
        _admin_env: &str,
    ) -> impl std::future::Future<Output = Result<crate::testing::Provision, crate::DbError>> + Send
    {
        std::future::ready(crate::testing::provision_file())
    }
}

impl Backend for Sqlite {
    const SCHEME: DatabaseScheme = DatabaseScheme::Sqlite;
}

/// What kind of database a SQLite URL addresses.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Location {
    File,
    Memory,
    ReadOnly,
}

impl Location {
    fn of(url: &str) -> Self {
        let rest = url.trim_start();
        let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
        let mode = query.split('&').find_map(|pair| pair.strip_prefix("mode="));
        if path.contains(":memory:") || mode == Some("memory") {
            Self::Memory
        } else if mode == Some("ro") {
            Self::ReadOnly
        } else {
            Self::File
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_are_classified_by_location() {
        assert_eq!(Location::of("sqlite::memory:"), Location::Memory);
        assert_eq!(Location::of("sqlite://:memory:"), Location::Memory);
        assert_eq!(Location::of("sqlite:app.db?mode=memory"), Location::Memory);
        assert_eq!(Location::of("sqlite:app.db?mode=ro"), Location::ReadOnly);
        assert_eq!(Location::of("sqlite:app.db?mode=rwc"), Location::File);
        assert_eq!(Location::of("sqlite://data/app.db"), Location::File);
    }
}
