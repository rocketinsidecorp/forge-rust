//! Classified persistence errors shared by every backend.

use forge_core::{ErrorCategory, ForgeError};
use sqlx::{
    error::{DatabaseError, ErrorKind},
    migrate::MigrateError,
};
use thiserror::Error;

use crate::{DatabaseError as ContractError, DatabaseErrorKind};

/// A classified database failure (PostgreSQL, MySQL/MariaDB or SQLite).
///
/// `From<sqlx::Error>` inspects the driver error (portable
/// [`sqlx::error::ErrorKind`], SQLSTATE and backend-specific codes) so callers
/// can react to constraint violations, retryable conditions and
/// optimistic-lock conflicts without parsing driver messages.
///
/// | Variant | PostgreSQL | MySQL / MariaDB | SQLite |
/// | --- | --- | --- | --- |
/// | `UniqueViolation` | `23505` | `1062` and the other duplicate-key errors | `SQLITE_CONSTRAINT_UNIQUE`, `_PRIMARYKEY` |
/// | `ForeignKeyViolation` | `23503` | `1451`, `1452` | `SQLITE_CONSTRAINT_FOREIGNKEY` |
/// | `CheckViolation` | `23514` | `3819` (MySQL), `4025` (MariaDB) | `SQLITE_CONSTRAINT_CHECK` |
/// | `NotNullViolation` | `23502` | `1048`, `1364` | `SQLITE_CONSTRAINT_NOTNULL` |
/// | `SerializationFailure` | `40001`, `40P01` | `1213` deadlock, `1205` lock wait timeout | `SQLITE_BUSY`, `SQLITE_LOCKED` (any extended code) |
/// | `Unavailable` | class `08`, `57P01`-`57P03`, `53300` | class `08`, `1040`, `1053`, `1927`, `4031`, I/O and TLS errors | `SQLITE_IOERR`, `SQLITE_CANTOPEN`, `SQLITE_FULL`, `SQLITE_READONLY` |
///
/// Display output never contains SQL text, row values, credentials or driver
/// messages. A constraint name is exposed through [`DbError::constraint`] when
/// it can be determined: the driver reports it on PostgreSQL; on MySQL/MariaDB
/// and for SQLite `CHECK` constraints it is recovered from the driver message
/// and only accepted if it looks like an SQL identifier. Driver diagnostics are
/// kept as the error `source` for operational logging only for failures that
/// cannot carry row data (availability, migration and generic query errors).
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum DbError {
    /// A unique or primary-key constraint was violated.
    #[error("unique constraint violated")]
    UniqueViolation {
        /// Name of the violated constraint or index, when it is known.
        constraint: Option<String>,
    },
    /// A foreign-key constraint was violated.
    #[error("foreign key constraint violated")]
    ForeignKeyViolation {
        /// Name of the violated constraint, when it is known.
        constraint: Option<String>,
    },
    /// A check constraint was violated.
    #[error("check constraint violated")]
    CheckViolation {
        /// Name of the violated constraint, when it is known.
        constraint: Option<String>,
    },
    /// A `NOT NULL` constraint was violated.
    #[error("not-null constraint violated")]
    NotNullViolation,
    /// The transaction lost a serialization race, was chosen as a deadlock
    /// victim or timed out waiting for a lock (SQLite: the database was busy
    /// or locked). Retrying the whole transaction may succeed.
    #[error("transaction aborted by a serialization failure, deadlock or lock timeout")]
    SerializationFailure,
    /// The requested row does not exist.
    #[error("row not found")]
    NotFound,
    /// The row changed since it was read (optimistic locking).
    #[error("row was modified concurrently")]
    Conflict,
    /// No pooled connection became available within the acquire timeout.
    #[error("timed out waiting for a database connection")]
    PoolTimeout,
    /// The database could not be reached or dropped the connection.
    #[error("database unavailable")]
    Unavailable(#[source] sqlx::Error),
    /// Applying or reverting migrations failed.
    #[error("database migration failed")]
    Migration(#[source] MigrateError),
    /// The most recently applied migration has no down migration.
    #[error("migration {version} cannot be reverted because it has no down migration")]
    IrreversibleMigration {
        /// Version of the most recently applied migration.
        version: i64,
    },
    /// The application `rollback` found no application migration to revert:
    /// only migrations shipped by Forge crates remain applied, and the
    /// application rollback never reverts those (ADR 0030).
    #[error(
        "no application migration is applied; migration {version} belongs to {owner} and is not reverted by the application rollback"
    )]
    FrameworkMigrationOnly {
        /// Newest applied framework-owned version.
        version: i64,
        /// The Forge crate that ships it.
        owner: &'static str,
    },
    /// The connection settings do not match what the operation needs.
    ///
    /// The reason is written by Forge and never contains the connection URL.
    #[error("invalid database configuration: {reason}")]
    Configuration {
        /// Credential-free explanation of what is wrong.
        reason: String,
    },
    /// Any other database failure.
    #[error("database query failed")]
    Query(#[source] sqlx::Error),
}

impl DbError {
    pub(crate) fn configuration(reason: impl Into<String>) -> Self {
        Self::Configuration {
            reason: reason.into(),
        }
    }

    /// Stable machine-readable code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::UniqueViolation { .. } => "unique_violation",
            Self::ForeignKeyViolation { .. } => "foreign_key_violation",
            Self::CheckViolation { .. } => "check_violation",
            Self::NotNullViolation => "not_null_violation",
            Self::SerializationFailure => "serialization_failure",
            Self::NotFound => "not_found",
            Self::Conflict => "conflict",
            Self::PoolTimeout => "pool_timeout",
            Self::Unavailable(_) => "unavailable",
            Self::Migration(_) => "migration_failed",
            Self::IrreversibleMigration { .. } => "irreversible_migration",
            Self::FrameworkMigrationOnly { .. } => "framework_migration_only",
            Self::Configuration { .. } => "invalid_configuration",
            Self::Query(_) => "query_failed",
        }
    }

    /// Coarse classification shared with the Forge database contracts.
    #[must_use]
    pub const fn kind(&self) -> DatabaseErrorKind {
        match self {
            Self::UniqueViolation { .. }
            | Self::ForeignKeyViolation { .. }
            | Self::CheckViolation { .. }
            | Self::NotNullViolation => DatabaseErrorKind::Constraint,
            Self::SerializationFailure | Self::Conflict => DatabaseErrorKind::Conflict,
            Self::PoolTimeout => DatabaseErrorKind::Timeout,
            Self::Unavailable(_) => DatabaseErrorKind::Unavailable,
            Self::Migration(_)
            | Self::IrreversibleMigration { .. }
            | Self::FrameworkMigrationOnly { .. } => DatabaseErrorKind::Migration,
            Self::NotFound | Self::Configuration { .. } | Self::Query(_) => {
                DatabaseErrorKind::Infrastructure
            }
        }
    }

    /// Whether retrying the operation (or the whole transaction) may succeed.
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::SerializationFailure | Self::PoolTimeout | Self::Unavailable(_)
        )
    }

    /// The violated constraint's name, when known.
    #[must_use]
    pub fn constraint(&self) -> Option<&str> {
        match self {
            Self::UniqueViolation { constraint }
            | Self::ForeignKeyViolation { constraint }
            | Self::CheckViolation { constraint } => constraint.as_deref(),
            _ => None,
        }
    }

    const fn category(&self) -> ErrorCategory {
        match self {
            Self::UniqueViolation { .. } | Self::ForeignKeyViolation { .. } | Self::Conflict => {
                ErrorCategory::Conflict
            }
            Self::CheckViolation { .. } | Self::NotNullViolation => ErrorCategory::Validation,
            Self::NotFound => ErrorCategory::NotFound,
            Self::SerializationFailure | Self::PoolTimeout | Self::Unavailable(_) => {
                ErrorCategory::Infrastructure
            }
            Self::Migration(_)
            | Self::IrreversibleMigration { .. }
            | Self::FrameworkMigrationOnly { .. }
            | Self::Configuration { .. }
            | Self::Query(_) => ErrorCategory::Internal,
        }
    }

    fn from_database(database: Box<dyn DatabaseError>) -> Self {
        let class = Classified::of(database.as_ref());
        let constraint = || constraint_name(database.as_ref());
        match class {
            Classified::Unique => Self::UniqueViolation {
                constraint: constraint(),
            },
            Classified::ForeignKey => Self::ForeignKeyViolation {
                constraint: constraint(),
            },
            Classified::Check => Self::CheckViolation {
                constraint: constraint(),
            },
            Classified::NotNull => Self::NotNullViolation,
            #[cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]
            Classified::Serialization => Self::SerializationFailure,
            #[cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]
            Classified::Unavailable => Self::Unavailable(sqlx::Error::Database(database)),
            Classified::Other => Self::Query(sqlx::Error::Database(database)),
        }
    }
}

impl From<DbError> for ForgeError {
    /// Maps to a stable, caller-safe [`ForgeError`]; the [`DbError`] stays
    /// attached as the diagnostic source.
    fn from(error: DbError) -> Self {
        let mut forge = Self::new(error.category(), error.code(), error.to_string())
            .with_retryable(error.is_retryable());
        if let Some(constraint) = error.constraint() {
            forge = forge.with_metadata("constraint", constraint.to_owned());
        }
        forge.with_source(error)
    }
}

impl From<DbError> for ContractError {
    /// Maps to the allocation-free contract error with a fixed message.
    fn from(error: DbError) -> Self {
        let message = match error.kind() {
            DatabaseErrorKind::Constraint => "database constraint violated",
            DatabaseErrorKind::Conflict => "database write conflict",
            DatabaseErrorKind::Timeout => "database operation timed out",
            DatabaseErrorKind::Unavailable => "database unavailable",
            DatabaseErrorKind::Migration => "database migration failed",
            DatabaseErrorKind::Infrastructure => "database operation failed",
        };
        Self::new(error.kind(), message, error.is_retryable())
    }
}

impl From<MigrateError> for DbError {
    fn from(error: MigrateError) -> Self {
        Self::Migration(error)
    }
}

impl From<sqlx::Error> for DbError {
    fn from(error: sqlx::Error) -> Self {
        match error {
            sqlx::Error::RowNotFound => Self::NotFound,
            sqlx::Error::PoolTimedOut => Self::PoolTimeout,
            sqlx::Error::Migrate(error) => Self::Migration(*error),
            sqlx::Error::Configuration(_) => Self::configuration("invalid connection settings"),
            sqlx::Error::PoolClosed
            | sqlx::Error::WorkerCrashed
            | sqlx::Error::Io(_)
            | sqlx::Error::Tls(_)
            | sqlx::Error::Protocol(_) => Self::Unavailable(error),
            sqlx::Error::Database(database) => Self::from_database(database),
            other => Self::Query(other),
        }
    }
}

/// Backend-independent classification of a driver error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Classified {
    Unique,
    ForeignKey,
    Check,
    NotNull,
    #[cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]
    Serialization,
    #[cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]
    Unavailable,
    Other,
}

impl Classified {
    /// Backend-specific codes win over the portable SQLx kind.
    fn of(error: &dyn DatabaseError) -> Self {
        #[cfg(feature = "postgres")]
        if let Some(error) = error.try_downcast_ref::<sqlx::postgres::PgDatabaseError>()
            && let Some(class) = classify_postgres(error.code())
        {
            return class;
        }
        #[cfg(feature = "mysql")]
        if let Some(error) = error.try_downcast_ref::<sqlx::mysql::MySqlDatabaseError>()
            && let Some(class) = classify_mysql(error.number(), error.code())
        {
            return class;
        }
        #[cfg(feature = "sqlite")]
        if let Some(error) = error.try_downcast_ref::<sqlx::sqlite::SqliteError>()
            && let Some(class) = error.code().and_then(classify_sqlite)
        {
            return class;
        }
        match error.kind() {
            ErrorKind::UniqueViolation => Self::Unique,
            ErrorKind::ForeignKeyViolation => Self::ForeignKey,
            ErrorKind::CheckViolation => Self::Check,
            ErrorKind::NotNullViolation => Self::NotNull,
            _ => Self::Other,
        }
    }
}

/// PostgreSQL SQLSTATE rules not covered by the portable kinds.
#[cfg(feature = "postgres")]
fn classify_postgres(code: &str) -> Option<Classified> {
    match code {
        "40001" | "40P01" => Some(Classified::Serialization),
        "53300" | "57P01" | "57P02" | "57P03" => Some(Classified::Unavailable),
        code if code.starts_with("08") => Some(Classified::Unavailable),
        _ => None,
    }
}

/// MySQL/MariaDB rules keyed on the numeric error code (and SQLSTATE class).
///
/// MySQL and MariaDB share the codes below; MariaDB-only codes (`4025`
/// check-constraint failure, `1927`, `4031`) cannot collide with a different
/// MySQL meaning that matters here (`4025` is disambiguated by SQLSTATE).
#[cfg(feature = "mysql")]
fn classify_mysql(number: u16, sqlstate: Option<&str>) -> Option<Classified> {
    match number {
        // ER_LOCK_DEADLOCK, ER_LOCK_WAIT_TIMEOUT
        1213 | 1205 => Some(Classified::Serialization),
        // ER_CON_COUNT_ERROR, ER_SERVER_SHUTDOWN, ER_CONNECTION_KILLED,
        // ER_CONNECTION_TIMEOUT (MariaDB idle timeout)
        1040 | 1053 | 1927 | 4031 => Some(Classified::Unavailable),
        // ER_BAD_NULL_ERROR, ER_NO_DEFAULT_FOR_FIELD
        1048 | 1364 => Some(Classified::NotNull),
        // ER_CHECK_CONSTRAINT_VIOLATED (MySQL), ER_CONSTRAINT_FAILED (MariaDB)
        3819 => Some(Classified::Check),
        4025 if sqlstate == Some("23000") => Some(Classified::Check),
        _ if sqlstate.is_some_and(|state| state.starts_with("08")) => Some(Classified::Unavailable),
        _ => None,
    }
}

/// SQLite rules: the code is the extended result code; the primary code is its
/// low byte.
#[cfg(feature = "sqlite")]
fn classify_sqlite(code: std::borrow::Cow<'_, str>) -> Option<Classified> {
    const CONSTRAINT_CHECK: i32 = 275;
    const CONSTRAINT_FOREIGNKEY: i32 = 787;
    const CONSTRAINT_NOTNULL: i32 = 1299;
    const CONSTRAINT_PRIMARYKEY: i32 = 1555;
    const CONSTRAINT_UNIQUE: i32 = 2067;

    let extended: i32 = code.parse().ok()?;
    match extended {
        CONSTRAINT_UNIQUE | CONSTRAINT_PRIMARYKEY => return Some(Classified::Unique),
        CONSTRAINT_FOREIGNKEY => return Some(Classified::ForeignKey),
        CONSTRAINT_CHECK => return Some(Classified::Check),
        CONSTRAINT_NOTNULL => return Some(Classified::NotNull),
        _ => {}
    }
    // SQLITE_BUSY = 5, SQLITE_LOCKED = 6, SQLITE_READONLY = 8,
    // SQLITE_IOERR = 10, SQLITE_FULL = 13, SQLITE_CANTOPEN = 14
    match extended & 0xff {
        5 | 6 => Some(Classified::Serialization),
        8 | 10 | 13 | 14 => Some(Classified::Unavailable),
        _ => None,
    }
}

/// Name of the violated constraint: reported by the driver (PostgreSQL) or
/// recovered from the message (MySQL/MariaDB, SQLite `CHECK`).
fn constraint_name(error: &dyn DatabaseError) -> Option<String> {
    if let Some(name) = error.constraint() {
        return Some(name.to_owned());
    }
    let message = error.message();
    #[cfg(feature = "mysql")]
    if error
        .try_downcast_ref::<sqlx::mysql::MySqlDatabaseError>()
        .is_some()
    {
        return mysql_constraint(message);
    }
    #[cfg(feature = "sqlite")]
    if error
        .try_downcast_ref::<sqlx::sqlite::SqliteError>()
        .is_some()
    {
        return between(message, "CHECK constraint failed: ", None).and_then(identifier);
    }
    let _ = message;
    None
}

/// MySQL: `Duplicate entry '..' for key 'table.index'` (8.0.19+) or
/// `for key 'index'`, `... CONSTRAINT `fk` FOREIGN KEY ...`,
/// `Check constraint 'name' is violated.`. MariaDB: `... for key 'index'`,
/// `CONSTRAINT `name` failed for `db`.`table``.
#[cfg(feature = "mysql")]
fn mysql_constraint(message: &str) -> Option<String> {
    let candidate = between(message, "for key '", Some('\''))
        .map(|key| key.rsplit('.').next().unwrap_or(key))
        .or_else(|| between(message, "CONSTRAINT `", Some('`')))
        .or_else(|| between(message, "Check constraint '", Some('\'')))?;
    identifier(candidate)
}

#[cfg(any(feature = "mysql", feature = "sqlite"))]
/// Returns the text after the last `start` and up to `end` (or the end of the
/// message).
fn between<'a>(message: &'a str, start: &str, end: Option<char>) -> Option<&'a str> {
    let rest = &message[message.rfind(start)? + start.len()..];
    Some(match end {
        Some(end) => rest.split(end).next()?,
        None => rest.trim(),
    })
}

#[cfg(any(feature = "mysql", feature = "sqlite"))]
/// Accepts `name` only if it looks like an SQL identifier, so text recovered
/// from a driver message can never smuggle SQL or row data into error
/// metadata.
fn identifier(name: &str) -> Option<String> {
    let valid = !name.is_empty()
        && name.len() <= 128
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '$'));
    valid.then(|| name.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(feature = "mysql", feature = "sqlite"))]
    #[test]
    fn only_identifier_like_names_are_accepted() {
        assert_eq!(
            identifier("owners_email_key").as_deref(),
            Some("owners_email_key")
        );
        assert_eq!(identifier("t$1").as_deref(), Some("t$1"));
        for rejected in ["", "length(title) > 0", "a;b", "a-b", "émail", "a b"] {
            assert_eq!(identifier(rejected), None, "{rejected}");
        }
        assert_eq!(identifier(&"x".repeat(129)), None);
    }

    #[cfg(any(feature = "mysql", feature = "sqlite"))]
    #[test]
    fn between_uses_the_last_marker() {
        assert_eq!(
            between(
                "Duplicate entry 'for key 'x'' for key 'owners.email'",
                "for key '",
                Some('\'')
            ),
            Some("owners.email")
        );
        assert_eq!(between("no marker", "for key '", Some('\'')), None);
        assert_eq!(
            between(
                "CHECK constraint failed: title_not_blank ",
                "CHECK constraint failed: ",
                None
            ),
            Some("title_not_blank")
        );
    }

    #[cfg(feature = "mysql")]
    #[test]
    fn mysql_constraint_names_are_recovered_from_messages() {
        let cases = [
            (
                "Duplicate entry 'a@b.c' for key 'owners.owners_email_key'",
                Some("owners_email_key"),
            ),
            (
                "Duplicate entry 'a@b.c' for key 'owners_email_key'",
                Some("owners_email_key"),
            ),
            (
                "Cannot add or update a child row: a foreign key constraint fails (`app`.`notes`, CONSTRAINT `notes_owner_fk` FOREIGN KEY (`owner_id`) REFERENCES `owners` (`id`))",
                Some("notes_owner_fk"),
            ),
            (
                "Check constraint 'title_not_blank' is violated.",
                Some("title_not_blank"),
            ),
            (
                "CONSTRAINT `title_not_blank` failed for `app`.`notes`",
                Some("title_not_blank"),
            ),
            ("Column 'title' cannot be null", None),
        ];
        for (message, expected) in cases {
            assert_eq!(mysql_constraint(message).as_deref(), expected, "{message}");
        }
    }

    #[cfg(feature = "mysql")]
    #[test]
    fn mysql_codes_are_classified() {
        assert_eq!(
            classify_mysql(1213, Some("40001")),
            Some(Classified::Serialization)
        );
        assert_eq!(
            classify_mysql(1205, Some("HY000")),
            Some(Classified::Serialization)
        );
        assert_eq!(
            classify_mysql(1040, Some("08004")),
            Some(Classified::Unavailable)
        );
        assert_eq!(
            classify_mysql(1927, Some("70100")),
            Some(Classified::Unavailable)
        );
        assert_eq!(
            classify_mysql(1048, Some("23000")),
            Some(Classified::NotNull)
        );
        assert_eq!(classify_mysql(3819, Some("HY000")), Some(Classified::Check));
        assert_eq!(classify_mysql(4025, Some("23000")), Some(Classified::Check));
        // 4025 means something else on MySQL, with a different SQLSTATE.
        assert_eq!(classify_mysql(4025, Some("HY000")), None);
        assert_eq!(
            classify_mysql(2000, Some("08S01")),
            Some(Classified::Unavailable)
        );
        assert_eq!(classify_mysql(1064, Some("42000")), None);
    }

    #[cfg(feature = "postgres")]
    #[test]
    fn postgres_codes_are_classified() {
        assert_eq!(classify_postgres("40001"), Some(Classified::Serialization));
        assert_eq!(classify_postgres("40P01"), Some(Classified::Serialization));
        assert_eq!(classify_postgres("08006"), Some(Classified::Unavailable));
        assert_eq!(classify_postgres("57P01"), Some(Classified::Unavailable));
        assert_eq!(classify_postgres("53300"), Some(Classified::Unavailable));
        assert_eq!(classify_postgres("23505"), None);
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn sqlite_extended_codes_are_classified() {
        let code = |value: &str| classify_sqlite(value.into());
        assert_eq!(code("2067"), Some(Classified::Unique));
        assert_eq!(code("1555"), Some(Classified::Unique));
        assert_eq!(code("787"), Some(Classified::ForeignKey));
        assert_eq!(code("275"), Some(Classified::Check));
        assert_eq!(code("1299"), Some(Classified::NotNull));
        // SQLITE_BUSY and its extended codes, SQLITE_LOCKED.
        assert_eq!(code("5"), Some(Classified::Serialization));
        assert_eq!(code("261"), Some(Classified::Serialization));
        assert_eq!(code("6"), Some(Classified::Serialization));
        assert_eq!(code("14"), Some(Classified::Unavailable));
        assert_eq!(code("1"), None);
        assert_eq!(code("not a number"), None);
    }

    #[test]
    fn public_messages_never_contain_driver_text() {
        let error = DbError::from(sqlx::Error::Protocol("secret row value".to_owned()));
        assert!(!error.to_string().contains("secret"));
        assert!(error.is_retryable());

        let forge = ForgeError::from(DbError::UniqueViolation {
            constraint: Some("owners_email_key".to_owned()),
        });
        assert_eq!(forge.code(), "unique_violation");
        assert_eq!(forge.category(), ErrorCategory::Conflict);
        assert_eq!(
            forge.metadata().get("constraint").and_then(|v| v.as_str()),
            Some("owners_email_key")
        );
    }

    #[test]
    fn variants_map_to_contract_kinds_and_retry_semantics() {
        let cases: [(DbError, DatabaseErrorKind, bool); 7] = [
            (
                DbError::NotNullViolation,
                DatabaseErrorKind::Constraint,
                false,
            ),
            (
                DbError::SerializationFailure,
                DatabaseErrorKind::Conflict,
                true,
            ),
            (DbError::Conflict, DatabaseErrorKind::Conflict, false),
            (DbError::PoolTimeout, DatabaseErrorKind::Timeout, true),
            (
                DbError::IrreversibleMigration { version: 1 },
                DatabaseErrorKind::Migration,
                false,
            ),
            (
                DbError::FrameworkMigrationOnly {
                    version: 20_261_019_000_100,
                    owner: "forge-cache",
                },
                DatabaseErrorKind::Migration,
                false,
            ),
            (DbError::NotFound, DatabaseErrorKind::Infrastructure, false),
        ];
        for (error, kind, retryable) in cases {
            assert_eq!(error.kind(), kind, "{error}");
            assert_eq!(error.is_retryable(), retryable, "{error}");
            let contract = ContractError::from(error);
            assert_eq!(contract.kind(), kind);
            assert_eq!(contract.retryable(), retryable);
        }
    }

    #[test]
    fn row_not_found_and_pool_timeout_are_mapped() {
        assert!(matches!(
            DbError::from(sqlx::Error::RowNotFound),
            DbError::NotFound
        ));
        assert!(matches!(
            DbError::from(sqlx::Error::PoolTimedOut),
            DbError::PoolTimeout
        ));
        assert!(matches!(
            DbError::from(sqlx::Error::PoolClosed),
            DbError::Unavailable(_)
        ));
    }
}
