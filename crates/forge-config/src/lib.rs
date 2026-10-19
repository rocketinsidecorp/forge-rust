//! Typed runtime configuration with explicit defaults and validation.
//!
//! Environment access is isolated at the process boundary. Application code
//! receives a validated [`AppConfig`] and never needs to parse ad-hoc strings.

use std::{
    collections::HashMap, ffi::OsString, fmt, net::SocketAddr, str::FromStr, time::Duration,
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

const DEFAULT_BIND: &str = "127.0.0.1:3000";
const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 30;
const DEFAULT_SHUTDOWN_GRACE_SECS: u64 = 15;
const DEFAULT_MAX_BODY_BYTES: usize = 1_048_576;
const DEFAULT_MAX_CONNECTIONS: usize = 10_000;
const DEFAULT_LOG_FILTER: &str = "info";
const REDACTED: &str = "[REDACTED]";
const DATABASE_URL_CONTRACT: &str =
    "a connection URL starting with postgres://, postgresql://, mysql://, mariadb:// or sqlite:";

/// Prefix reserved for Forge configuration keys.
///
/// Every environment variable starting with this prefix must be one of
/// [`KNOWN_KEYS`]; anything else is rejected by [`AppConfig::from_env`].
const KEY_PREFIX: &str = "FORGE_";

/// Every environment variable understood by [`AppConfig::from_env`].
pub const KNOWN_KEYS: &[&str] = &[
    "FORGE_ENV",
    "FORGE_BIND",
    "FORGE_REQUEST_TIMEOUT_SECS",
    "FORGE_SHUTDOWN_GRACE_SECS",
    "FORGE_MAX_BODY_BYTES",
    "FORGE_MAX_CONNECTIONS",
    "FORGE_LOG",
    "FORGE_LOG_FORMAT",
    "FORGE_DATABASE_URL",
    "FORGE_MIGRATION_DATABASE_URL",
    SETUP_PASSWORD_KEY,
];

/// The owner password the `setup` command of generated applications reads
/// when it is not typed at a terminal or piped with `--password-stdin`.
///
/// It is a known key so configuration loading accepts it, but it is never
/// part of [`AppConfig`]: only the command that asks for it reads it, and
/// it should never be set for the serving process.
pub const SETUP_PASSWORD_KEY: &str = "FORGE_SETUP_PASSWORD";

/// Runtime environment controls safety-sensitive defaults.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Environment {
    /// Local developer workstation.
    #[default]
    Development,
    /// Automated test process.
    Test,
    /// Internet-facing production process.
    Production,
}

impl FromStr for Environment {
    type Err = ConfigError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "development" | "dev" => Ok(Self::Development),
            "test" => Ok(Self::Test),
            "production" | "prod" => Ok(Self::Production),
            _ => Err(ConfigError::InvalidValue {
                key: "FORGE_ENV",
                value: value.to_owned(),
                expected: "development, test, or production",
            }),
        }
    }
}

/// HTTP server settings after validation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ServerConfig {
    /// Address on which the process accepts connections.
    pub bind: SocketAddr,
    /// Maximum time allowed for one request.
    #[serde(with = "duration_seconds")]
    pub request_timeout: Duration,
    /// Time allowed for in-flight work during shutdown.
    #[serde(with = "duration_seconds")]
    pub shutdown_grace: Duration,
    /// Maximum buffered request body size.
    pub max_body_bytes: usize,
    /// Maximum number of concurrently open client connections.
    pub max_connections: usize,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: DEFAULT_BIND
                .parse()
                .unwrap_or_else(|error| unreachable!("valid built-in bind address: {error}")),
            request_timeout: Duration::from_secs(DEFAULT_REQUEST_TIMEOUT_SECS),
            shutdown_grace: Duration::from_secs(DEFAULT_SHUTDOWN_GRACE_SECS),
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
            max_connections: DEFAULT_MAX_CONNECTIONS,
        }
    }
}

/// Log output encoding.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    /// One JSON object per line, suitable for log collectors.
    Json,
    /// Human-readable text for local development.
    #[default]
    Text,
}

impl LogFormat {
    /// Default format for an environment: JSON in production, text elsewhere.
    #[must_use]
    pub fn default_for(environment: Environment) -> Self {
        match environment {
            Environment::Production => Self::Json,
            Environment::Development | Environment::Test => Self::Text,
        }
    }
}

impl FromStr for LogFormat {
    type Err = ConfigError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "json" => Ok(Self::Json),
            "text" => Ok(Self::Text),
            _ => Err(ConfigError::InvalidValue {
                key: "FORGE_LOG_FORMAT",
                value: value.to_owned(),
                expected: "json or text",
            }),
        }
    }
}

/// Logging settings after validation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LogConfig {
    /// Filter directives in `tracing-subscriber` `EnvFilter` syntax, e.g. `info,my_app=debug`.
    pub filter: String,
    /// Output encoding.
    pub format: LogFormat,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            filter: DEFAULT_LOG_FILTER.to_owned(),
            format: LogFormat::default_for(Environment::default()),
        }
    }
}

/// The database engine a connection URL selects.
///
/// `FORGE_DATABASE_URL` and `FORGE_MIGRATION_DATABASE_URL` accept
/// `postgres://` and `postgresql://` (PostgreSQL), `mysql://` and
/// `mariadb://` (MySQL 8.4+ and MariaDB 11.4+, one wire protocol and one
/// backend) and `sqlite:` (a file path, `sqlite::memory:` or a `sqlite://`
/// URL). The scheme is matched case-insensitively.
///
/// ```
/// use forge_config::DatabaseScheme;
///
/// assert_eq!(DatabaseScheme::of_url("postgresql://db/app"), Some(DatabaseScheme::Postgres));
/// assert_eq!(DatabaseScheme::of_url("mariadb://db/app"), Some(DatabaseScheme::MySql));
/// assert_eq!(DatabaseScheme::of_url("sqlite::memory:"), Some(DatabaseScheme::Sqlite));
/// assert_eq!(DatabaseScheme::of_url("redis://cache"), None);
/// ```
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DatabaseScheme {
    /// PostgreSQL: `postgres://` or `postgresql://`.
    Postgres,
    /// MySQL or MariaDB: `mysql://` or `mariadb://`.
    MySql,
    /// SQLite: `sqlite:`.
    Sqlite,
}

impl DatabaseScheme {
    /// Returns the engine selected by the scheme of `url`, or `None` when the
    /// scheme is missing or not supported. The URL is only inspected up to its
    /// first `:`, so no part of it can leak through this function.
    #[must_use]
    pub fn of_url(url: &str) -> Option<Self> {
        let (scheme, _) = url.trim_start().split_once(':')?;
        match scheme.to_ascii_lowercase().as_str() {
            "postgres" | "postgresql" => Some(Self::Postgres),
            "mysql" | "mariadb" => Some(Self::MySql),
            "sqlite" => Some(Self::Sqlite),
            _ => None,
        }
    }

    /// Human-readable engine name, e.g. for log lines and diagnostics.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Postgres => "PostgreSQL",
            Self::MySql => "MySQL/MariaDB",
            Self::Sqlite => "SQLite",
        }
    }

    /// The URL schemes that select this engine, formatted for messages.
    #[must_use]
    pub const fn accepted_schemes(self) -> &'static str {
        match self {
            Self::Postgres => "postgres:// or postgresql://",
            Self::MySql => "mysql:// or mariadb://",
            Self::Sqlite => "sqlite:",
        }
    }
}

impl fmt::Display for DatabaseScheme {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

/// A secret configuration value.
///
/// Debug and serialization are deliberately redacted so credentials cannot be
/// exposed by ordinary diagnostics. Call expose only at the adapter boundary
/// that needs the underlying value.
#[derive(Clone, Eq, PartialEq, Deserialize)]
#[serde(transparent)]
pub struct SecretString(String);

impl SecretString {
    /// Wraps a secret value.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Exposes the secret to a concrete adapter.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for SecretString {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("[REDACTED]")
    }
}

impl Serialize for SecretString {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str("[REDACTED]")
    }
}

/// Database settings after validation.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct DatabaseConfig {
    /// Least-privilege runtime URL (PostgreSQL, MySQL/MariaDB or SQLite).
    /// Optional for database-free apps.
    pub url: Option<SecretString>,
    /// Privileged URL used only by migrate/rollback commands.
    pub migration_url: Option<SecretString>,
}

impl DatabaseConfig {
    /// The engine selected by the runtime URL, if one is configured.
    #[must_use]
    pub fn scheme(&self) -> Option<DatabaseScheme> {
        self.url
            .as_ref()
            .and_then(|url| DatabaseScheme::of_url(url.expose()))
    }

    /// The engine selected by the migration URL, if one is configured.
    #[must_use]
    pub fn migration_scheme(&self) -> Option<DatabaseScheme> {
        self.migration_url
            .as_ref()
            .and_then(|url| DatabaseScheme::of_url(url.expose()))
    }
}

/// Complete process configuration.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct AppConfig {
    /// Deployment environment.
    pub environment: Environment,
    /// HTTP server settings.
    pub server: ServerConfig,
    /// Logging settings.
    pub log: LogConfig,
    /// Database settings.
    pub database: DatabaseConfig,
}

impl AppConfig {
    /// Loads configuration from process environment variables.
    ///
    /// Supported keys are listed in [`KNOWN_KEYS`]. Loading fails closed: any
    /// other variable starting with `FORGE_` is rejected with
    /// [`ConfigError::UnknownKey`], so a typo such as `FORGE_BIDN` cannot
    /// silently fall back to a default.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_vars(std::env::vars_os())
    }

    /// Loads configuration from an explicit set of environment variables.
    ///
    /// Variables without the `FORGE_` prefix are ignored. Unknown `FORGE_*`
    /// keys and known keys whose value is not valid UTF-8 are rejected.
    pub fn from_vars(
        vars: impl IntoIterator<Item = (OsString, OsString)>,
    ) -> Result<Self, ConfigError> {
        let mut values: HashMap<&'static str, String> = HashMap::new();
        for (key, value) in vars {
            let key_text = key.to_string_lossy();
            if !key_text.starts_with(KEY_PREFIX) {
                continue;
            }
            let Some(known) = KNOWN_KEYS
                .iter()
                .copied()
                .find(|known| **known == *key_text)
            else {
                return Err(ConfigError::UnknownKey {
                    key: key_text.into_owned(),
                });
            };
            let value = value
                .into_string()
                .map_err(|_| ConfigError::NotUnicode { key: known })?;
            values.insert(known, value);
        }
        Self::from_lookup(|key| values.get(key).cloned())
    }

    /// Loads configuration using a caller-provided environment lookup.
    ///
    /// Only known keys are looked up, so unknown-key detection is the
    /// responsibility of [`AppConfig::from_vars`]. This is useful for
    /// deterministic tests and configuration adapters.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let mut config = Self::default();

        if let Some(value) = lookup("FORGE_ENV") {
            config.environment = value.parse()?;
        }
        if let Some(value) = lookup("FORGE_BIND") {
            config.server.bind = parse_value("FORGE_BIND", &value, "a socket address")?;
        }
        if let Some(value) = lookup("FORGE_REQUEST_TIMEOUT_SECS") {
            config.server.request_timeout =
                Duration::from_secs(parse_positive("FORGE_REQUEST_TIMEOUT_SECS", &value)?);
        }
        if let Some(value) = lookup("FORGE_SHUTDOWN_GRACE_SECS") {
            config.server.shutdown_grace =
                Duration::from_secs(parse_positive("FORGE_SHUTDOWN_GRACE_SECS", &value)?);
        }
        if let Some(value) = lookup("FORGE_MAX_BODY_BYTES") {
            config.server.max_body_bytes = parse_positive("FORGE_MAX_BODY_BYTES", &value)?;
        }
        if let Some(value) = lookup("FORGE_MAX_CONNECTIONS") {
            config.server.max_connections = parse_positive("FORGE_MAX_CONNECTIONS", &value)?;
        }
        if let Some(value) = lookup("FORGE_LOG") {
            let filter = value.trim();
            if filter.is_empty() {
                return Err(ConfigError::InvalidValue {
                    key: "FORGE_LOG",
                    value,
                    expected: "a non-empty log filter such as \"info\"",
                });
            }
            config.log.filter = filter.to_owned();
        }
        if let Some(value) = lookup("FORGE_DATABASE_URL") {
            config.database.url = Some(parse_secret_url("FORGE_DATABASE_URL", value)?);
        }
        if let Some(value) = lookup("FORGE_MIGRATION_DATABASE_URL") {
            config.database.migration_url =
                Some(parse_secret_url("FORGE_MIGRATION_DATABASE_URL", value)?);
        }
        // The format default depends on the environment, so derive it only
        // after FORGE_ENV has been applied.
        config.log.format = match lookup("FORGE_LOG_FORMAT") {
            Some(value) => value.parse()?,
            None => LogFormat::default_for(config.environment),
        };

        Ok(config)
    }
}

fn parse_value<T>(key: &'static str, value: &str, expected: &'static str) -> Result<T, ConfigError>
where
    T: FromStr,
{
    value.parse().map_err(|_| ConfigError::InvalidValue {
        key,
        value: value.to_owned(),
        expected,
    })
}

fn parse_positive<T>(key: &'static str, value: &str) -> Result<T, ConfigError>
where
    T: FromStr + Default + PartialOrd,
{
    let parsed = parse_value(key, value, "a positive integer")?;
    if parsed <= T::default() {
        return Err(ConfigError::InvalidValue {
            key,
            value: value.to_owned(),
            expected: "a positive integer",
        });
    }
    Ok(parsed)
}

fn parse_secret_url(key: &'static str, value: String) -> Result<SecretString, ConfigError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(ConfigError::InvalidValue {
            key,
            value,
            expected: DATABASE_URL_CONTRACT,
        });
    }
    if DatabaseScheme::of_url(trimmed).is_none() {
        // The rejected value is a credential-bearing URL: never echo it.
        return Err(ConfigError::InvalidValue {
            key,
            value: REDACTED.to_owned(),
            expected: DATABASE_URL_CONTRACT,
        });
    }
    Ok(SecretString::new(trimmed))
}

/// Configuration loading or validation failure.
#[derive(Debug, Error, Eq, PartialEq)]
pub enum ConfigError {
    /// A known key contains a value outside its contract.
    #[error("invalid value for {key}: {value:?}; expected {expected}")]
    InvalidValue {
        /// Configuration key.
        key: &'static str,
        /// Rejected, non-secret value.
        value: String,
        /// Human-readable contract.
        expected: &'static str,
    },
    /// A `FORGE_*` variable is not a known key, usually a typo.
    #[error("unknown configuration key {key}; expected one of: {known}", known = KNOWN_KEYS.join(", "))]
    UnknownKey {
        /// Rejected variable name.
        key: String,
    },
    /// A known key contains a value that is not valid UTF-8.
    #[error("invalid value for {key}: value is not valid UTF-8")]
    NotUnicode {
        /// Configuration key.
        key: &'static str,
    },
}

mod duration_seconds {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(value: &Duration, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_u64(value.as_secs())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Duration, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(Duration::from_secs(u64::deserialize(deserializer)?))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    #[test]
    fn defaults_are_safe_for_local_development() {
        let config = AppConfig::from_lookup(|_| None).expect("defaults must be valid");

        assert_eq!(config.environment, Environment::Development);
        assert!(config.server.bind.ip().is_loopback());
        assert_eq!(config.server.max_body_bytes, 1_048_576);
    }

    #[test]
    fn provided_values_override_defaults() {
        let values = HashMap::from([
            ("FORGE_ENV", "production"),
            ("FORGE_BIND", "0.0.0.0:8080"),
            ("FORGE_REQUEST_TIMEOUT_SECS", "12"),
            ("FORGE_SHUTDOWN_GRACE_SECS", "7"),
            ("FORGE_MAX_BODY_BYTES", "4096"),
        ]);

        let config = AppConfig::from_lookup(|key| values.get(key).map(ToString::to_string))
            .expect("fixture must be valid");

        assert_eq!(config.environment, Environment::Production);
        assert_eq!(
            config.server.bind,
            "0.0.0.0:8080".parse().expect("valid address")
        );
        assert_eq!(config.server.request_timeout, Duration::from_secs(12));
        assert_eq!(config.server.shutdown_grace, Duration::from_secs(7));
        assert_eq!(config.server.max_body_bytes, 4096);
    }

    #[test]
    fn zero_limits_fail_closed() {
        let error =
            AppConfig::from_lookup(|key| (key == "FORGE_MAX_BODY_BYTES").then(|| "0".to_owned()))
                .expect_err("zero limit must be rejected");

        assert!(matches!(
            error,
            ConfigError::InvalidValue {
                key: "FORGE_MAX_BODY_BYTES",
                ..
            }
        ));
    }

    #[test]
    fn unknown_environment_is_rejected() {
        let error =
            AppConfig::from_lookup(|key| (key == "FORGE_ENV").then(|| "staging-ish".to_owned()))
                .expect_err("unknown environment must be rejected");

        assert!(matches!(
            error,
            ConfigError::InvalidValue {
                key: "FORGE_ENV",
                ..
            }
        ));
    }
    fn vars(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        pairs
            .iter()
            .map(|(key, value)| (OsString::from(key), OsString::from(value)))
            .collect()
    }

    #[test]
    fn new_defaults_are_applied() {
        let config = AppConfig::from_vars(Vec::new()).expect("defaults must be valid");

        assert_eq!(config.server.max_connections, 10_000);
        assert_eq!(config.log.filter, "info");
        assert_eq!(config.log.format, LogFormat::Text);
        assert_eq!(config, AppConfig::default());
    }

    #[test]
    fn max_connections_is_parsed_and_must_be_positive() {
        let config = AppConfig::from_vars(vars(&[("FORGE_MAX_CONNECTIONS", "64")]))
            .expect("fixture must be valid");
        assert_eq!(config.server.max_connections, 64);

        for invalid in ["0", "-1", "many"] {
            let error = AppConfig::from_vars(vars(&[("FORGE_MAX_CONNECTIONS", invalid)]))
                .expect_err("invalid connection limit must be rejected");
            assert!(matches!(
                error,
                ConfigError::InvalidValue {
                    key: "FORGE_MAX_CONNECTIONS",
                    ..
                }
            ));
        }
    }

    #[test]
    fn log_filter_is_trimmed_and_must_not_be_blank() {
        let config = AppConfig::from_vars(vars(&[("FORGE_LOG", "  warn,my_app=debug ")]))
            .expect("fixture must be valid");
        assert_eq!(config.log.filter, "warn,my_app=debug");

        for blank in ["", "   "] {
            let error = AppConfig::from_vars(vars(&[("FORGE_LOG", blank)]))
                .expect_err("blank filter must be rejected");
            assert!(matches!(
                error,
                ConfigError::InvalidValue {
                    key: "FORGE_LOG",
                    ..
                }
            ));
        }
    }

    #[test]
    fn log_format_defaults_follow_environment() {
        let production = AppConfig::from_vars(vars(&[("FORGE_ENV", "production")]))
            .expect("fixture must be valid");
        assert_eq!(production.log.format, LogFormat::Json);

        let test =
            AppConfig::from_vars(vars(&[("FORGE_ENV", "test")])).expect("fixture must be valid");
        assert_eq!(test.log.format, LogFormat::Text);
    }

    #[test]
    fn explicit_log_format_overrides_environment_default() {
        let config =
            AppConfig::from_vars(vars(&[("FORGE_LOG_FORMAT", "text"), ("FORGE_ENV", "prod")]))
                .expect("fixture must be valid");
        assert_eq!(config.log.format, LogFormat::Text);

        let config = AppConfig::from_vars(vars(&[("FORGE_LOG_FORMAT", " JSON ")]))
            .expect("fixture must be valid");
        assert_eq!(config.log.format, LogFormat::Json);
    }

    #[test]
    fn unknown_log_format_is_rejected() {
        let error = AppConfig::from_vars(vars(&[("FORGE_LOG_FORMAT", "yaml")]))
            .expect_err("unknown format must be rejected");

        assert!(matches!(
            error,
            ConfigError::InvalidValue {
                key: "FORGE_LOG_FORMAT",
                ..
            }
        ));
    }

    #[test]
    fn unknown_forge_keys_fail_closed() {
        let error = AppConfig::from_vars(vars(&[("FORGE_BIDN", "0.0.0.0:8080")]))
            .expect_err("typo must be rejected");

        assert_eq!(
            error,
            ConfigError::UnknownKey {
                key: "FORGE_BIDN".to_owned()
            }
        );
        assert!(error.to_string().contains("FORGE_BIND"));
    }

    #[test]
    fn unrelated_variables_are_ignored() {
        let config = AppConfig::from_vars(vars(&[
            ("PATH", "/usr/bin"),
            ("forge_bidn", "lowercase is not the Forge namespace"),
            ("MY_FORGE_THING", "1"),
            ("FORGE_BIND", "0.0.0.0:9000"),
        ]))
        .expect("unrelated variables must not fail loading");

        assert_eq!(
            config.server.bind,
            "0.0.0.0:9000".parse().expect("valid address")
        );
    }

    #[test]
    fn database_urls_are_available_but_redacted() {
        let config = AppConfig::from_vars(vars(&[
            (
                "FORGE_DATABASE_URL",
                "postgres://runtime:runtime-secret@localhost/app",
            ),
            (
                "FORGE_MIGRATION_DATABASE_URL",
                "postgres://migrator:migration-secret@localhost/app",
            ),
        ]))
        .expect("database URLs should be accepted");

        assert_eq!(
            config
                .database
                .url
                .as_ref()
                .expect("runtime database URL should be present")
                .expose(),
            "postgres://runtime:runtime-secret@localhost/app"
        );
        assert_eq!(
            config
                .database
                .migration_url
                .as_ref()
                .expect("migration database URL should be present")
                .expose(),
            "postgres://migrator:migration-secret@localhost/app"
        );

        let debug = format!("{config:?}");
        assert!(!debug.contains("runtime-secret"));
        assert!(!debug.contains("migration-secret"));
        assert!(debug.contains("[REDACTED]"));

        let json = serde_json::to_string(&config).expect("config should serialize");
        assert!(!json.contains("runtime-secret"));
        assert!(!json.contains("migration-secret"));
        assert!(json.contains("[REDACTED]"));
    }

    #[test]
    fn blank_database_urls_are_rejected() {
        for key in ["FORGE_DATABASE_URL", "FORGE_MIGRATION_DATABASE_URL"] {
            let error = AppConfig::from_vars(vars(&[(key, "   ")]))
                .expect_err("blank database URL must be rejected");

            assert!(matches!(
                error,
                ConfigError::InvalidValue { key: actual, .. } if actual == key
            ));
        }
    }

    #[test]
    fn database_url_schemes_select_the_engine() {
        let cases = [
            ("postgres://u:p@h/db", DatabaseScheme::Postgres),
            ("postgresql://u:p@h/db", DatabaseScheme::Postgres),
            ("POSTGRES://u:p@h/db", DatabaseScheme::Postgres),
            ("mysql://u:p@h/db", DatabaseScheme::MySql),
            ("mariadb://u:p@h/db", DatabaseScheme::MySql),
            ("sqlite:data/app.db", DatabaseScheme::Sqlite),
            ("sqlite://data/app.db?mode=rwc", DatabaseScheme::Sqlite),
            ("sqlite::memory:", DatabaseScheme::Sqlite),
        ];
        for (url, expected) in cases {
            let config = AppConfig::from_vars(vars(&[
                ("FORGE_DATABASE_URL", url),
                ("FORGE_MIGRATION_DATABASE_URL", url),
            ]))
            .expect("supported database URL must be accepted");
            assert_eq!(config.database.scheme(), Some(expected), "{url}");
            assert_eq!(config.database.migration_scheme(), Some(expected), "{url}");
        }
        assert_eq!(AppConfig::default().database.scheme(), None);
    }

    #[test]
    fn unsupported_database_schemes_are_rejected_without_echoing_the_url() {
        for key in ["FORGE_DATABASE_URL", "FORGE_MIGRATION_DATABASE_URL"] {
            for url in [
                "redis://user:hunter2@cache/0",
                "mssql://sa:hunter2@host/db",
                "user:hunter2@host/db",
                "hunter2",
            ] {
                let error = AppConfig::from_vars(vars(&[(key, url)]))
                    .expect_err("unsupported scheme must be rejected");
                let rendered = format!("{error} {error:?}");
                assert!(!rendered.contains("hunter2"), "{key}: {rendered}");
                assert!(rendered.contains("mariadb://"), "{rendered}");
            }
        }
    }

    #[test]
    fn scheme_helpers_describe_each_engine() {
        assert_eq!(
            DatabaseScheme::MySql.accepted_schemes(),
            "mysql:// or mariadb://"
        );
        assert_eq!(DatabaseScheme::Postgres.to_string(), "PostgreSQL");
        assert_eq!(
            DatabaseScheme::of_url("  sqlite:x"),
            Some(DatabaseScheme::Sqlite)
        );
        assert_eq!(DatabaseScheme::of_url("no-scheme"), None);
    }

    #[test]
    fn every_known_key_is_accepted() {
        let values = [
            "production",
            "0.0.0.0:8080",
            "5",
            "5",
            "10",
            "10",
            "debug",
            "json",
            "postgres://runtime:secret@localhost/app",
            "postgres://migrator:secret@localhost/app",
            "correct horse battery staple",
        ];
        let pairs: Vec<(&str, &str)> = KNOWN_KEYS.iter().copied().zip(values).collect();
        assert_eq!(pairs.len(), KNOWN_KEYS.len());

        AppConfig::from_vars(vars(&pairs)).expect("every known key must be accepted");
    }

    #[cfg(unix)]
    #[test]
    fn non_unicode_values_are_rejected() {
        use std::os::unix::ffi::OsStringExt;

        let error = AppConfig::from_vars(vec![(
            OsString::from("FORGE_BIND"),
            OsString::from_vec(vec![0x30, 0xff, 0xfe]),
        )])
        .expect_err("non-UTF-8 value must be rejected");

        assert_eq!(error, ConfigError::NotUnicode { key: "FORGE_BIND" });
    }

    #[cfg(unix)]
    #[test]
    fn non_unicode_forge_keys_are_rejected_as_unknown() {
        use std::os::unix::ffi::OsStringExt;

        let error = AppConfig::from_vars(vec![(
            OsString::from_vec(b"FORGE_\xffBIND".to_vec()),
            OsString::from("0.0.0.0:8080"),
        )])
        .expect_err("non-UTF-8 key must be rejected");

        assert!(matches!(error, ConfigError::UnknownKey { .. }));
    }
}
