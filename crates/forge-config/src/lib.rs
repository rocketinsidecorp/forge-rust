//! Typed runtime configuration with explicit defaults and validation.
//!
//! Environment access is isolated at the process boundary. Application code
//! receives a validated [`AppConfig`] and never needs to parse ad-hoc strings.

use std::{
    collections::HashMap,
    ffi::OsString,
    fmt,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::PathBuf,
    str::FromStr,
    time::Duration,
};

use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD},
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
const DEFAULT_RATE_LIMIT_MAX_KEYS: usize = 100_000;
const MAX_RATE_LIMIT_MAX_KEYS: usize = 10_000_000;
const DEFAULT_CACHE_MAX_ENTRIES: usize = 10_000;
const MAX_CACHE_MAX_ENTRIES: usize = 100_000_000;
const DEFAULT_CACHE_MAX_BYTES: u64 = 64 << 20;
const DEFAULT_CACHE_MAX_VALUE_BYTES: usize = 1 << 20;
/// Largest cache value: well inside the `MEDIUMBLOB` of MySQL/MariaDB and the
/// packet limits of every backend. Big objects belong in storage.
pub const MAX_CACHE_VALUE_BYTES: usize = 8 << 20;
/// Headroom the in-memory cache needs above one maximum-size value (key and
/// per-entry bookkeeping).
const CACHE_BYTES_HEADROOM: u64 = 4096;
const DEFAULT_CACHE_TTL_SECS: u64 = 3600;
const MAX_CACHE_TTL_SECS: u64 = 365 * 24 * 3600;
const DEFAULT_STORAGE_MAX_UPLOAD_BYTES: u64 = 512 << 10;
const MAX_STORAGE_MAX_UPLOAD_BYTES: u64 = 1 << 30;
const DEFAULT_STORAGE_LINK_TTL_SECS: u64 = 900;
const MAX_STORAGE_LINK_TTL_SECS: u64 = 7 * 24 * 3600;
/// Shortest decoded stream-signing key (`FORGE_STREAMS_KEY`), in bytes: an
/// HMAC-SHA256 key as long as the digest.
pub const STREAMS_KEY_MIN_BYTES: usize = 32;
const STREAMS_KEY_CONTRACT: &str = "base64 (standard or URL-safe, padded or not) of at least 32 \
     random bytes, such as `openssl rand -base64 32` prints";
/// Bounds of the decoded storage signing key, in bytes.
pub const STORAGE_SIGNING_KEY_BYTES: std::ops::RangeInclusive<usize> = 32..=1024;
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
    TRUSTED_PROXIES_KEY,
    "FORGE_LOG",
    "FORGE_LOG_FORMAT",
    "FORGE_DATABASE_URL",
    "FORGE_MIGRATION_DATABASE_URL",
    SETUP_PASSWORD_KEY,
    "FORGE_RATE_LIMIT_MAX_KEYS",
    "FORGE_CACHE_MAX_ENTRIES",
    "FORGE_CACHE_MAX_BYTES",
    "FORGE_CACHE_MAX_VALUE_BYTES",
    "FORGE_CACHE_DEFAULT_TTL_SECS",
    "FORGE_STORAGE_ROOT",
    "FORGE_STORAGE_SIGNING_KEY",
    "FORGE_STORAGE_MAX_UPLOAD_BYTES",
    "FORGE_STORAGE_LINK_TTL_SECS",
    "FORGE_STREAMS_KEY",
];

/// The reverse proxies whose `X-Forwarded-For` header is believed (ADR 0032):
/// a comma-separated list of CIDR prefixes or bare addresses. Empty, the
/// default, trusts nobody.
pub const TRUSTED_PROXIES_KEY: &str = "FORGE_HTTP_TRUSTED_PROXIES";

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
    /// Networks of the reverse proxies whose `X-Forwarded-For` header is
    /// believed when resolving the client address
    /// (`FORGE_HTTP_TRUSTED_PROXIES`, ADR 0032). Empty, the default, trusts
    /// nobody: the client is the TCP peer. Every entry was validated by
    /// [`TrustedProxy::new`].
    #[serde(default)]
    pub trusted_proxies: Vec<TrustedProxy>,
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
            trusted_proxies: Vec::new(),
        }
    }
}

/// Shortest IPv4 prefix a trusted proxy network may have. Shorter prefixes
/// trust a large part of the address space (two `/1` prefixes cover the
/// whole IPv4 internet), letting almost any client choose its address.
pub const MIN_TRUSTED_PREFIX_BITS_V4: u8 = 8;
/// Shortest IPv6 prefix a trusted proxy network may have.
pub const MIN_TRUSTED_PREFIX_BITS_V6: u8 = 16;

/// A network of reverse proxies whose `X-Forwarded-For` header is believed
/// (ADR 0032): a CIDR prefix, or a bare address as a `/32` or `/128`.
///
/// A value exists only after [`TrustedProxy::new`] (or parsing, which calls
/// it) accepted it, so every `TrustedProxy` is safe to trust: never `/0`, no
/// IPv4 prefix shorter than `/8`, no IPv6 prefix shorter than `/16` and no
/// IPv4-mapped IPv6 prefix (peers are compared in their IPv4 form, so it
/// would silently never match; write the IPv4 form). The network address is
/// stored masked.
///
/// ```
/// use forge_config::TrustedProxy;
///
/// let proxy: TrustedProxy = "172.18.5.9/16".parse()?;
/// assert_eq!(proxy.to_string(), "172.18.0.0/16");
/// assert!(proxy.contains("172.18.0.2".parse()?));
/// assert!(proxy.contains("::ffff:172.18.0.2".parse()?));
/// assert!("0.0.0.0/0".parse::<TrustedProxy>().is_err());
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct TrustedProxy {
    network: IpAddr,
    prefix_len: u8,
}

impl TrustedProxy {
    /// The network of `address` and the first `prefix_len` bits, or why it
    /// is not a safe trusted proxy network.
    ///
    /// This is the single definition of the rules: parsing
    /// `FORGE_HTTP_TRUSTED_PROXIES`, deserializing a [`ServerConfig`] and
    /// the HTTP server's resolver all go through it. The error never contains
    /// the address.
    pub fn new(address: IpAddr, prefix_len: u8) -> Result<Self, TrustedProxyError> {
        let network = match address {
            IpAddr::V4(address) => {
                if prefix_len > 32 {
                    return Err(TrustedProxyError::InvalidPrefix);
                }
                if prefix_len < MIN_TRUSTED_PREFIX_BITS_V4 {
                    return Err(TrustedProxyError::TooBroadV4);
                }
                IpAddr::V4(Ipv4Addr::from(u32::from(address) & v4_mask(prefix_len)))
            }
            IpAddr::V6(address) => {
                if prefix_len > 128 {
                    return Err(TrustedProxyError::InvalidPrefix);
                }
                if address.to_ipv4_mapped().is_some() {
                    return Err(TrustedProxyError::Ipv4Mapped);
                }
                if prefix_len < MIN_TRUSTED_PREFIX_BITS_V6 {
                    return Err(TrustedProxyError::TooBroadV6);
                }
                IpAddr::V6(Ipv6Addr::from(u128::from(address) & v6_mask(prefix_len)))
            }
        };
        Ok(Self {
            network,
            prefix_len,
        })
    }

    /// The masked network address.
    #[must_use]
    pub const fn network(&self) -> IpAddr {
        self.network
    }

    /// The prefix length in bits.
    #[must_use]
    pub const fn prefix_len(&self) -> u8 {
        self.prefix_len
    }

    /// Whether `address` is inside this network. IPv4-mapped IPv6 addresses
    /// are compared in their IPv4 form.
    #[must_use]
    pub fn contains(&self, address: IpAddr) -> bool {
        match (self.network, address.to_canonical()) {
            (IpAddr::V4(network), IpAddr::V4(address)) => {
                u32::from(address) & v4_mask(self.prefix_len) == u32::from(network)
            }
            (IpAddr::V6(network), IpAddr::V6(address)) => {
                u128::from(address) & v6_mask(self.prefix_len) == u128::from(network)
            }
            _ => false,
        }
    }
}

fn v4_mask(prefix_len: u8) -> u32 {
    u32::MAX
        .checked_shl(32 - u32::from(prefix_len))
        .unwrap_or(0)
}

fn v6_mask(prefix_len: u8) -> u128 {
    u128::MAX
        .checked_shl(128 - u32::from(prefix_len))
        .unwrap_or(0)
}

impl fmt::Display for TrustedProxy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}/{}", self.network, self.prefix_len)
    }
}

impl FromStr for TrustedProxy {
    type Err = TrustedProxyError;

    /// Parses one entry: a CIDR prefix such as `172.18.0.0/16`, or a bare
    /// address (a `/32` or `/128`). Surrounding whitespace is ignored; zoned
    /// addresses, ports, brackets and prefix lengths with a sign or a leading
    /// zero are malformed.
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let value = value.trim();
        let (address, prefix_len) = match value.split_once('/') {
            Some((address, bits)) => (address, Some(parse_prefix_len(bits)?)),
            None => (value, None),
        };
        let address: IpAddr = address.parse().map_err(|_| TrustedProxyError::Malformed)?;
        let prefix_len = prefix_len.unwrap_or(match address {
            IpAddr::V4(_) => 32,
            IpAddr::V6(_) => 128,
        });
        match Self::new(address, prefix_len) {
            // A length beyond the address is a parse failure, as in Forge-Go.
            Err(TrustedProxyError::InvalidPrefix) => Err(TrustedProxyError::Malformed),
            result => result,
        }
    }
}

/// Decimal prefix length without sign or leading zero, at most 255.
fn parse_prefix_len(bits: &str) -> Result<u8, TrustedProxyError> {
    let canonical = !bits.is_empty()
        && bits.len() <= 3
        && bits.bytes().all(|byte| byte.is_ascii_digit())
        && (bits == "0" || !bits.starts_with('0'));
    if !canonical {
        return Err(TrustedProxyError::Malformed);
    }
    bits.parse().map_err(|_| TrustedProxyError::Malformed)
}

impl Serialize for TrustedProxy {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for TrustedProxy {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let text = String::deserialize(deserializer)?;
        text.parse()
            .map_err(|error| serde::de::Error::custom(format!("trusted proxy {error}")))
    }
}

/// Why a trusted proxy entry was refused. The message completes the sentence
/// "entry N ..." and never contains the entry.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[non_exhaustive]
pub enum TrustedProxyError {
    /// Not an IP address or CIDR prefix.
    #[error("is not an IP address or CIDR prefix such as 172.18.0.0/16")]
    Malformed,
    /// The prefix length is longer than the address.
    #[error("is not a valid CIDR prefix")]
    InvalidPrefix,
    /// An IPv4-mapped IPv6 prefix or address.
    #[error("is an IPv4-mapped IPv6 prefix; use the IPv4 form")]
    Ipv4Mapped,
    /// An IPv4 prefix shorter than [`MIN_TRUSTED_PREFIX_BITS_V4`].
    #[error(
        "is too broad: IPv4 prefixes shorter than /8 trust too many addresses; list the proxy networks instead"
    )]
    TooBroadV4,
    /// An IPv6 prefix shorter than [`MIN_TRUSTED_PREFIX_BITS_V6`].
    #[error(
        "is too broad: IPv6 prefixes shorter than /16 trust too many addresses; list the proxy networks instead"
    )]
    TooBroadV6,
}

/// Parses `FORGE_HTTP_TRUSTED_PROXIES`: a comma-separated list of CIDR
/// prefixes or bare addresses, such as `172.18.0.0/16, fd00:1::/64,
/// 10.0.0.7`. A blank list trusts nobody. An empty, malformed or unsafe
/// entry (see [`TrustedProxy::new`]) is an error naming the entry's
/// position, never its text.
pub fn parse_trusted_proxies(list: &str) -> Result<Vec<TrustedProxy>, ConfigError> {
    if list.trim().is_empty() {
        return Ok(Vec::new());
    }
    list.split(',')
        .enumerate()
        .map(|(index, entry)| {
            entry
                .parse()
                .map_err(|reason| ConfigError::InvalidTrustedProxy {
                    entry: index + 1,
                    reason,
                })
        })
        .collect()
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

/// Rate limiter settings (`forge-ratelimit`).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RateLimitConfig {
    /// Live buckets the in-memory limiter keeps (`FORGE_RATE_LIMIT_MAX_KEYS`).
    pub max_keys: usize,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            max_keys: DEFAULT_RATE_LIMIT_MAX_KEYS,
        }
    }
}

/// Application cache settings (`forge-cache`).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CacheConfig {
    /// Entry bound of every cache (`FORGE_CACHE_MAX_ENTRIES`).
    pub max_entries: usize,
    /// Memory bound of the in-memory cache: keys, values and per-entry
    /// overhead (`FORGE_CACHE_MAX_BYTES`).
    pub max_bytes: u64,
    /// Largest cached value (`FORGE_CACHE_MAX_VALUE_BYTES`, at most
    /// [`MAX_CACHE_VALUE_BYTES`]).
    pub max_value_bytes: usize,
    /// Time to live used when a write passes none
    /// (`FORGE_CACHE_DEFAULT_TTL_SECS`).
    #[serde(with = "duration_seconds")]
    pub default_ttl: Duration,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            max_entries: DEFAULT_CACHE_MAX_ENTRIES,
            max_bytes: DEFAULT_CACHE_MAX_BYTES,
            max_value_bytes: DEFAULT_CACHE_MAX_VALUE_BYTES,
            default_ttl: Duration::from_secs(DEFAULT_CACHE_TTL_SECS),
        }
    }
}

/// File storage settings (`forge-storage`).
///
/// Storage is *configured* when [`StorageConfig::root`] is set. Production
/// then requires an absolute root and a signing key; there is no default key.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct StorageConfig {
    /// Directory of the disk adapter (`FORGE_STORAGE_ROOT`); `None` means
    /// storage is not used.
    pub root: Option<PathBuf>,
    /// HMAC key of signed links as unpadded base64url of
    /// [`STORAGE_SIGNING_KEY_BYTES`] random bytes
    /// (`FORGE_STORAGE_SIGNING_KEY`). Redacted in diagnostics.
    pub signing_key: Option<SecretString>,
    /// Largest uploaded file (`FORGE_STORAGE_MAX_UPLOAD_BYTES`).
    pub max_upload_bytes: u64,
    /// Default lifetime of signed links (`FORGE_STORAGE_LINK_TTL_SECS`).
    #[serde(with = "duration_seconds")]
    pub link_ttl: Duration,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            root: None,
            signing_key: None,
            max_upload_bytes: DEFAULT_STORAGE_MAX_UPLOAD_BYTES,
            link_ttl: Duration::from_secs(DEFAULT_STORAGE_LINK_TTL_SECS),
        }
    }
}

impl StorageConfig {
    /// Decodes the configured signing key, or `None` when none is set.
    ///
    /// [`AppConfig`] validation already guarantees a canonical key of an
    /// acceptable length; the error never contains the key.
    pub fn signing_key_bytes(&self) -> Result<Option<Vec<u8>>, ConfigError> {
        self.signing_key
            .as_ref()
            .map(|key| decode_signing_key(key.expose()))
            .transpose()
    }
}

fn decode_signing_key(value: &str) -> Result<Vec<u8>, ConfigError> {
    let invalid = || ConfigError::InvalidValue {
        key: "FORGE_STORAGE_SIGNING_KEY",
        value: REDACTED.to_owned(),
        expected: "unpadded base64url of 32 to 1024 random bytes",
    };
    let bytes = URL_SAFE_NO_PAD.decode(value).map_err(|_| invalid())?;
    if !STORAGE_SIGNING_KEY_BYTES.contains(&bytes.len()) {
        return Err(invalid());
    }
    Ok(bytes)
}

/// Live-update stream settings (`forge-broadcast`).
///
/// The key signs the channel names pages subscribe to. It is optional here:
/// only an application that serves live updates needs it, and the
/// broadcast signer refuses to start without it in production (development
/// and test then use a random per-process key).
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct StreamsConfig {
    /// HMAC key of signed stream names as base64 (standard or URL-safe,
    /// padded or not) of at least [`STREAMS_KEY_MIN_BYTES`] random bytes
    /// (`FORGE_STREAMS_KEY`), shared by every replica. Redacted in
    /// diagnostics.
    pub key: Option<SecretString>,
}

impl StreamsConfig {
    /// Decodes the configured key, or `None` when none is set. The error
    /// never contains the key.
    pub fn key_bytes(&self) -> Result<Option<Vec<u8>>, ConfigError> {
        self.key
            .as_ref()
            .map(|key| decode_streams_key(key.expose()))
            .transpose()
    }
}

/// Decodes `FORGE_STREAMS_KEY` like Forge-Go does: any of the four base64
/// alphabets/paddings, at least [`STREAMS_KEY_MIN_BYTES`] bytes.
fn decode_streams_key(value: &str) -> Result<Vec<u8>, ConfigError> {
    let value = value.trim();
    let bytes = [STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD]
        .iter()
        .find_map(|engine| engine.decode(value).ok());
    match bytes {
        Some(bytes) if bytes.len() >= STREAMS_KEY_MIN_BYTES => Ok(bytes),
        _ => Err(ConfigError::InvalidValue {
            key: "FORGE_STREAMS_KEY",
            value: REDACTED.to_owned(),
            expected: STREAMS_KEY_CONTRACT,
        }),
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
    /// Rate limiter settings.
    pub rate_limit: RateLimitConfig,
    /// Application cache settings.
    pub cache: CacheConfig,
    /// File storage settings.
    pub storage: StorageConfig,
    /// Live-update stream settings.
    pub streams: StreamsConfig,
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
        if let Some(value) = lookup(TRUSTED_PROXIES_KEY) {
            config.server.trusted_proxies = parse_trusted_proxies(&value)?;
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
        apply_services(&lookup, &mut config)?;
        // The format default depends on the environment, so derive it only
        // after FORGE_ENV has been applied.
        config.log.format = match lookup("FORGE_LOG_FORMAT") {
            Some(value) => value.parse()?,
            None => LogFormat::default_for(config.environment),
        };

        validate_services(&config)?;
        Ok(config)
    }
}

/// Reads the rate limit, cache, storage and streams keys.
fn apply_services(
    lookup: &impl Fn(&str) -> Option<String>,
    config: &mut AppConfig,
) -> Result<(), ConfigError> {
    if let Some(value) = lookup("FORGE_RATE_LIMIT_MAX_KEYS") {
        config.rate_limit.max_keys = parse_bounded(
            "FORGE_RATE_LIMIT_MAX_KEYS",
            &value,
            1..=MAX_RATE_LIMIT_MAX_KEYS,
            "an integer from 1 to 10000000",
        )?;
    }
    if let Some(value) = lookup("FORGE_CACHE_MAX_ENTRIES") {
        config.cache.max_entries = parse_bounded(
            "FORGE_CACHE_MAX_ENTRIES",
            &value,
            1..=MAX_CACHE_MAX_ENTRIES,
            "an integer from 1 to 100000000",
        )?;
    }
    if let Some(value) = lookup("FORGE_CACHE_MAX_BYTES") {
        config.cache.max_bytes = parse_positive("FORGE_CACHE_MAX_BYTES", &value)?;
    }
    if let Some(value) = lookup("FORGE_CACHE_MAX_VALUE_BYTES") {
        config.cache.max_value_bytes = parse_bounded(
            "FORGE_CACHE_MAX_VALUE_BYTES",
            &value,
            1..=MAX_CACHE_VALUE_BYTES,
            "an integer from 1 to 8388608",
        )?;
    }
    if let Some(value) = lookup("FORGE_CACHE_DEFAULT_TTL_SECS") {
        config.cache.default_ttl = Duration::from_secs(parse_bounded(
            "FORGE_CACHE_DEFAULT_TTL_SECS",
            &value,
            1..=MAX_CACHE_TTL_SECS,
            "a number of seconds from 1 to 31536000",
        )?);
    }
    if let Some(value) = lookup("FORGE_STORAGE_ROOT") {
        let trimmed = value.trim();
        if trimmed.is_empty() || trimmed.contains('\0') {
            return Err(ConfigError::InvalidValue {
                key: "FORGE_STORAGE_ROOT",
                value,
                expected: "a non-empty directory path",
            });
        }
        config.storage.root = Some(PathBuf::from(trimmed));
    }
    if let Some(value) = lookup("FORGE_STORAGE_SIGNING_KEY") {
        // Validates the key; the rejected value is a secret and never echoed.
        decode_signing_key(value.trim())?;
        config.storage.signing_key = Some(SecretString::new(value.trim()));
    }
    if let Some(value) = lookup("FORGE_STORAGE_MAX_UPLOAD_BYTES") {
        config.storage.max_upload_bytes = parse_bounded(
            "FORGE_STORAGE_MAX_UPLOAD_BYTES",
            &value,
            1..=MAX_STORAGE_MAX_UPLOAD_BYTES,
            "an integer from 1 to 1073741824",
        )?;
    }
    if let Some(value) = lookup("FORGE_STREAMS_KEY") {
        // Validates the key; the rejected value is a secret and never echoed.
        decode_streams_key(&value)?;
        config.streams.key = Some(SecretString::new(value.trim()));
    }
    if let Some(value) = lookup("FORGE_STORAGE_LINK_TTL_SECS") {
        config.storage.link_ttl = Duration::from_secs(parse_bounded(
            "FORGE_STORAGE_LINK_TTL_SECS",
            &value,
            1..=MAX_STORAGE_LINK_TTL_SECS,
            "a number of seconds from 1 to 604800",
        )?);
    }
    Ok(())
}

/// Rules that involve several keys or the environment.
fn validate_services(config: &AppConfig) -> Result<(), ConfigError> {
    let cache = &config.cache;
    if cache.max_bytes < cache.max_value_bytes as u64 + CACHE_BYTES_HEADROOM {
        return Err(ConfigError::InvalidValue {
            key: "FORGE_CACHE_MAX_BYTES",
            value: cache.max_bytes.to_string(),
            expected: "at least FORGE_CACHE_MAX_VALUE_BYTES plus 4096",
        });
    }
    let storage = &config.storage;
    let Some(root) = &storage.root else {
        return Ok(());
    };
    if config.environment == Environment::Production {
        if !root.is_absolute() {
            return Err(ConfigError::InvalidValue {
                key: "FORGE_STORAGE_ROOT",
                value: root.display().to_string(),
                expected: "an absolute path on a persistent volume in production",
            });
        }
        if storage.signing_key.is_none() {
            return Err(ConfigError::InvalidValue {
                key: "FORGE_STORAGE_SIGNING_KEY",
                value: "(not set)".to_owned(),
                expected: "a signing key whenever FORGE_STORAGE_ROOT is set in production",
            });
        }
    }
    // The server buffers a request body up to FORGE_MAX_BODY_BYTES; an upload
    // limit above it could never be reached and would suggest otherwise.
    if storage.max_upload_bytes > config.server.max_body_bytes as u64 {
        return Err(ConfigError::InvalidValue {
            key: "FORGE_STORAGE_MAX_UPLOAD_BYTES",
            value: storage.max_upload_bytes.to_string(),
            expected: "at most FORGE_MAX_BODY_BYTES when FORGE_STORAGE_ROOT is set",
        });
    }
    Ok(())
}

fn parse_bounded<T>(
    key: &'static str,
    value: &str,
    range: std::ops::RangeInclusive<T>,
    expected: &'static str,
) -> Result<T, ConfigError>
where
    T: FromStr + PartialOrd,
{
    let parsed: T = parse_value(key, value, expected)?;
    if !range.contains(&parsed) {
        return Err(ConfigError::InvalidValue {
            key,
            value: value.to_owned(),
            expected,
        });
    }
    Ok(parsed)
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
    /// An entry of `FORGE_HTTP_TRUSTED_PROXIES` was refused. Only its
    /// position is reported: the value is never echoed.
    #[error("invalid value for FORGE_HTTP_TRUSTED_PROXIES: entry {entry} {reason}")]
    InvalidTrustedProxy {
        /// One-based position of the refused entry.
        entry: usize,
        /// Why it was refused.
        reason: TrustedProxyError,
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
            "172.18.0.0/16, fd00:1::/64",
            "debug",
            "json",
            "postgres://runtime:secret@localhost/app",
            "postgres://migrator:secret@localhost/app",
            "correct horse battery staple",
            "1000",
            "100",
            "100000",
            "1024",
            "60",
            "/var/lib/app/storage",
            "dGhpcy1pcy1hLTMyLWJ5dGUtc2lnbmluZy1rZXktISE",
            "10",
            "60",
            "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY=",
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

    const KEY_32: &str = "dGhpcy1pcy1hLTMyLWJ5dGUtc2lnbmluZy1rZXktISE";

    fn load(pairs: &[(&str, &str)]) -> Result<AppConfig, ConfigError> {
        let values: HashMap<String, String> = pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect();
        AppConfig::from_lookup(|key| values.get(key).cloned())
    }

    #[test]
    fn service_defaults_match_the_documented_values() {
        let config = load(&[]).expect("defaults are valid");

        assert_eq!(config.rate_limit.max_keys, 100_000);
        assert_eq!(config.cache.max_entries, 10_000);
        assert_eq!(config.cache.max_bytes, 64 << 20);
        assert_eq!(config.cache.max_value_bytes, 1 << 20);
        assert_eq!(config.cache.default_ttl, Duration::from_secs(3600));
        assert_eq!(config.storage.root, None);
        assert_eq!(config.storage.signing_key, None);
        assert_eq!(config.storage.max_upload_bytes, 512 << 10);
        assert_eq!(config.storage.link_ttl, Duration::from_secs(900));
    }

    #[test]
    fn service_limits_are_bounded() {
        for (key, value) in [
            ("FORGE_RATE_LIMIT_MAX_KEYS", "0"),
            ("FORGE_RATE_LIMIT_MAX_KEYS", "10000001"),
            ("FORGE_CACHE_MAX_ENTRIES", "0"),
            ("FORGE_CACHE_MAX_VALUE_BYTES", "8388609"),
            ("FORGE_CACHE_DEFAULT_TTL_SECS", "0"),
            ("FORGE_CACHE_DEFAULT_TTL_SECS", "31536001"),
            ("FORGE_STORAGE_MAX_UPLOAD_BYTES", "1073741825"),
            ("FORGE_STORAGE_LINK_TTL_SECS", "604801"),
            ("FORGE_STORAGE_LINK_TTL_SECS", "abc"),
        ] {
            let error = load(&[(key, value)]).expect_err("out of range must fail");
            assert!(
                matches!(error, ConfigError::InvalidValue { key: found, .. } if found == key),
                "{key}={value}: {error}"
            );
        }
    }

    #[test]
    fn cache_byte_bound_must_hold_a_maximum_value() {
        let error = load(&[
            ("FORGE_CACHE_MAX_VALUE_BYTES", "1048576"),
            ("FORGE_CACHE_MAX_BYTES", "1048576"),
        ])
        .expect_err("a byte bound below one value must fail");
        assert!(matches!(
            error,
            ConfigError::InvalidValue {
                key: "FORGE_CACHE_MAX_BYTES",
                ..
            }
        ));
    }

    #[test]
    fn signing_key_must_be_canonical_base64url_of_enough_bytes_and_is_never_echoed() {
        let short = "c2hvcnQ";
        let padded = format!("{KEY_32}=");
        let standard_alphabet = "+/+/+/+/+/+/+/+/+/+/+/+/+/+/+/+/+/+/+/+/+/";
        // The last character carries non-zero trailing bits.
        let non_canonical = "dGhpcy1pcy1hLTMyLWJ5dGUtc2lnbmluZy1rZXktISF";
        for bad in [short, padded.as_str(), standard_alphabet, non_canonical, ""] {
            let error = load(&[("FORGE_STORAGE_SIGNING_KEY", bad)]).expect_err("bad key");
            let text = error.to_string();
            assert!(text.contains("[REDACTED]"), "{text}");
            assert!(bad.is_empty() || !text.contains(bad), "{text}");
        }
        let config = load(&[("FORGE_STORAGE_SIGNING_KEY", KEY_32)]).expect("valid key");
        assert_eq!(
            config
                .storage
                .signing_key_bytes()
                .expect("decodes")
                .map(|bytes| bytes.len()),
            Some(32)
        );
        assert!(!format!("{config:?}").contains(KEY_32));
        assert!(
            !serde_json::to_string(&config)
                .expect("serializes")
                .contains(KEY_32)
        );
    }

    #[test]
    fn streams_key_accepts_every_base64_form_of_32_bytes_and_is_never_echoed() {
        // `openssl rand -base64 32` output, and the same bytes in the other
        // three alphabet/padding forms Forge-Go accepts.
        let standard = "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY=";
        for key in [
            standard,
            "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY",
            "-_-_-_-_-_-_-_-_-_-_-_-_-_-_-_-_-_-_-_-_-_8=",
            "-_-_-_-_-_-_-_-_-_-_-_-_-_-_-_-_-_-_-_-_-_8",
            "+/+/+/+/+/+/+/+/+/+/+/+/+/+/+/+/+/+/+/+/+/8=",
        ] {
            let config = load(&[("FORGE_STREAMS_KEY", key)]).expect(key);
            let bytes = config.streams.key_bytes().expect("decodes").expect("set");
            assert_eq!(bytes.len(), 32, "{key}");
            assert!(!format!("{config:?}").contains(key));
            assert!(
                !serde_json::to_string(&config)
                    .expect("serializes")
                    .contains(key)
            );
        }
        // 31 bytes, not base64, empty.
        for bad in [
            "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZQ==",
            "not base64!",
            "",
        ] {
            let error = load(&[("FORGE_STREAMS_KEY", bad)]).expect_err("bad key");
            let text = error.to_string();
            assert!(text.contains("FORGE_STREAMS_KEY"), "{text}");
            assert!(text.contains("[REDACTED]"), "{text}");
            assert!(bad.is_empty() || !text.contains(bad), "{text}");
        }
        // Optional everywhere: the broadcast signer decides what a missing
        // key means.
        let production = load(&[("FORGE_ENV", "production")]).expect("no key needed here");
        assert_eq!(production.streams.key, None);
        assert_eq!(production.streams.key_bytes(), Ok(None));
    }

    #[test]
    fn production_storage_needs_an_absolute_root_and_a_signing_key() {
        let production = ("FORGE_ENV", "production");
        let key = ("FORGE_STORAGE_SIGNING_KEY", KEY_32);
        for (pairs, failing) in [
            (
                vec![production, ("FORGE_STORAGE_ROOT", "relative/storage"), key],
                "FORGE_STORAGE_ROOT",
            ),
            (
                vec![production, ("FORGE_STORAGE_ROOT", "/srv/storage")],
                "FORGE_STORAGE_SIGNING_KEY",
            ),
        ] {
            let error = load(&pairs).expect_err("production rule must fail");
            assert!(
                matches!(error, ConfigError::InvalidValue { key, .. } if key == failing),
                "{error}"
            );
        }
        load(&[production, ("FORGE_STORAGE_ROOT", "/srv/storage"), key]).expect("valid");
        // Storage that is not configured imposes nothing; development is lenient.
        load(&[production]).expect("no storage configured");
        load(&[("FORGE_STORAGE_ROOT", "storage")]).expect("development accepts a relative root");
    }

    #[test]
    fn upload_limit_cannot_exceed_the_request_body_limit_when_storage_is_configured() {
        let error = load(&[
            ("FORGE_STORAGE_ROOT", "storage"),
            ("FORGE_STORAGE_MAX_UPLOAD_BYTES", "2097152"),
        ])
        .expect_err("2 MiB upload with a 1 MiB body limit must fail");
        assert!(matches!(
            error,
            ConfigError::InvalidValue {
                key: "FORGE_STORAGE_MAX_UPLOAD_BYTES",
                ..
            }
        ));
        load(&[
            ("FORGE_STORAGE_ROOT", "storage"),
            ("FORGE_STORAGE_MAX_UPLOAD_BYTES", "2097152"),
            ("FORGE_MAX_BODY_BYTES", "3145728"),
        ])
        .expect("raised body limit allows it");
        load(&[("FORGE_STORAGE_MAX_UPLOAD_BYTES", "2097152")])
            .expect("not checked while storage is unused");
    }

    fn trusted_proxies(value: &str) -> Result<AppConfig, ConfigError> {
        AppConfig::from_lookup(|key| (key == TRUSTED_PROXIES_KEY).then(|| value.to_owned()))
    }

    #[test]
    fn trusted_proxies_default_to_nobody() {
        assert!(
            AppConfig::from_lookup(|_| None)
                .expect("defaults")
                .server
                .trusted_proxies
                .is_empty()
        );
        for blank in ["", "  "] {
            let config = trusted_proxies(blank).expect("blank trusts nobody");
            assert!(config.server.trusted_proxies.is_empty(), "{blank:?}");
        }
    }

    #[test]
    fn trusted_proxies_parse_masked_prefixes_and_bare_addresses() {
        let config = trusted_proxies(" 172.18.5.9/16, fd00:1::/64,10.0.0.7 ,2001:db8::1")
            .expect("valid list");
        let parsed: Vec<String> = config
            .server
            .trusted_proxies
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(
            parsed,
            [
                "172.18.0.0/16",
                "fd00:1::/64",
                "10.0.0.7/32",
                "2001:db8::1/128"
            ]
        );
    }

    #[test]
    fn trusted_proxies_accept_the_minimum_prefix_lengths() {
        let config = trusted_proxies("10.0.0.0/8, fc00::/16").expect("minimum lengths");
        assert_eq!(config.server.trusted_proxies.len(), 2);
    }

    #[test]
    fn trusted_proxies_reject_invalid_entries_without_echoing_them() {
        for (value, entry) in [
            ("172.18.0.0/33", 1),
            ("not-a-network", 1),
            ("172.18.0.0/16,", 2),
            ("172.18.0.0/16,,10.0.0.0/8", 2),
            ("0.0.0.0/0", 1),
            ("::/0", 1),
            ("0.0.0.0/1,128.0.0.0/1", 1),
            ("10.0.0.0/7", 1),
            ("::/1", 1),
            ("fc00::/15", 1),
            ("10.0.0.0/8, 0.0.0.0/0", 2),
            ("::ffff:10.0.0.0/104", 1),
            ("::ffff:10.0.0.1", 1),
            ("fe80::1%eth0", 1),
            ("fe80::%eth0/64", 1),
            ("172.18.0.0/16 10.0.0.0/8", 1),
            ("*", 1),
            ("10.0.0.0/+8", 1),
            ("10.0.0.0/08", 1),
            ("10.0.0.0/", 1),
            ("10.0.0.1:80", 1),
            ("[2001:db8::1]", 1),
        ] {
            let error = trusted_proxies(value).expect_err(value);
            assert!(
                matches!(error, ConfigError::InvalidTrustedProxy { entry: actual, .. } if actual == entry),
                "{value}: {error:?}"
            );
            let message = error.to_string();
            assert!(message.contains(TRUSTED_PROXIES_KEY), "{message}");
            assert!(message.contains(&format!("entry {entry} ")), "{message}");
            assert!(!message.contains(value.trim()), "{value}: {message}");
            let refused = value.split(',').nth(entry - 1).unwrap_or_default().trim();
            assert!(
                refused.len() < 2 || !message.contains(refused),
                "{value}: {message} echoes {refused}"
            );
        }
    }

    #[test]
    fn trusted_proxy_rules_apply_to_programmatic_values() {
        let v4 = |text: &str| text.parse::<IpAddr>().expect("address");
        for (address, bits, reason) in [
            ("0.0.0.0", 0, TrustedProxyError::TooBroadV4),
            ("0.0.0.0", 1, TrustedProxyError::TooBroadV4),
            ("10.0.0.0", 7, TrustedProxyError::TooBroadV4),
            ("10.0.0.1", 33, TrustedProxyError::InvalidPrefix),
            ("::", 0, TrustedProxyError::TooBroadV6),
            ("::", 1, TrustedProxyError::TooBroadV6),
            ("fc00::", 15, TrustedProxyError::TooBroadV6),
            ("fc00::", 129, TrustedProxyError::InvalidPrefix),
            ("::ffff:10.0.0.0", 104, TrustedProxyError::Ipv4Mapped),
        ] {
            assert_eq!(
                TrustedProxy::new(v4(address), bits),
                Err(reason),
                "{address}/{bits}"
            );
        }
        let proxy = TrustedProxy::new(v4("172.18.5.9"), 16).expect("valid");
        assert_eq!(proxy.network(), v4("172.18.0.0"));
        assert_eq!(proxy.prefix_len(), 16);
        assert!(proxy.contains(v4("172.18.255.1")));
        assert!(proxy.contains(v4("::ffff:172.18.0.2")));
        assert!(!proxy.contains(v4("172.19.0.1")));
        assert!(!proxy.contains(v4("fd00::1")));
        let v6 = TrustedProxy::new(v4("fd00:1::"), 64).expect("valid");
        assert!(v6.contains(v4("fd00:1::abcd")));
        assert!(!v6.contains(v4("fd00:2::1")));
        assert!(!v6.contains(v4("10.0.0.1")));
    }

    #[test]
    fn trusted_proxies_deserialize_through_the_same_rules() {
        let config = AppConfig::from_lookup(|key| {
            (key == TRUSTED_PROXIES_KEY).then(|| "172.18.0.0/16".to_owned())
        })
        .expect("valid");
        let json = serde_json::to_string(&config.server).expect("serializes");
        assert!(
            json.contains(r#""trusted_proxies":["172.18.0.0/16"]"#),
            "{json}"
        );
        let back: ServerConfig = serde_json::from_str(&json).expect("deserializes");
        assert_eq!(back, config.server);
        let unsafe_json = json.replace("172.18.0.0/16", "0.0.0.0/0");
        let error =
            serde_json::from_str::<ServerConfig>(&unsafe_json).expect_err("unsafe prefix refused");
        assert!(!error.to_string().contains("0.0.0.0"), "{error}");
        let without: ServerConfig =
            serde_json::from_str(&json.replace(r#","trusted_proxies":["172.18.0.0/16"]"#, ""))
                .expect("older configuration without the field");
        assert!(without.trusted_proxies.is_empty());
    }
}
