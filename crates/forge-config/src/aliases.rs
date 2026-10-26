//! Deprecated spellings of configuration keys (naming decision D2).
//!
//! Forge-Rust and Forge-Go read one environment variable per setting, and
//! the structured `FORGE_HTTP_*`, `FORGE_DATABASE_*`, `FORGE_CACHE_*` and
//! `FORGE_STORAGE_*` names of Forge-Go are canonical. The older Forge-Rust
//! spellings in [`DEPRECATED_KEYS`] are still read for one minor version:
//! [`AppConfig::from_env`](crate::AppConfig::from_env) prints a one-line
//! warning to standard error the first time the process sees each of them,
//! and a variable set under both spellings to different values is refused
//! with [`ConfigError::ConflictingAlias`] instead of letting one silently
//! win. Values are compared after parsing, so `FORGE_SHUTDOWN_GRACE_SECS=15`
//! and `FORGE_HTTP_SHUTDOWN_TIMEOUT=15s` agree.

use std::io;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::ConfigError;

/// A configuration key renamed to Forge-Go's spelling.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeprecatedKey {
    /// The old spelling, still read with a warning.
    pub deprecated: &'static str,
    /// The spelling to use instead.
    pub canonical: &'static str,
    /// How the value changes with the name, empty when it does not.
    pub value_change: &'static str,
}

/// The canonical name of the listen address (`FORGE_BIND` before).
pub const HTTP_ADDR_KEY: &str = "FORGE_HTTP_ADDR";
/// The canonical name of the request body bound (`FORGE_MAX_BODY_BYTES`
/// before).
pub const HTTP_MAX_BODY_BYTES_KEY: &str = "FORGE_HTTP_MAX_BODY_BYTES";
/// The canonical name of the per-request deadline, a duration
/// (`FORGE_REQUEST_TIMEOUT_SECS` before, in seconds).
pub const HTTP_WRITE_TIMEOUT_KEY: &str = "FORGE_HTTP_WRITE_TIMEOUT";
/// The canonical name of the shutdown drain deadline, a duration
/// (`FORGE_SHUTDOWN_GRACE_SECS` before, in seconds).
pub const HTTP_SHUTDOWN_TIMEOUT_KEY: &str = "FORGE_HTTP_SHUTDOWN_TIMEOUT";
/// The canonical name of the cache's default time to live, a duration
/// (`FORGE_CACHE_DEFAULT_TTL_SECS` before, in seconds).
pub const CACHE_DEFAULT_TTL_KEY: &str = "FORGE_CACHE_DEFAULT_TTL";
/// The canonical name of the signed-link lifetime, a duration
/// (`FORGE_STORAGE_LINK_TTL_SECS` before, in seconds).
pub const STORAGE_LINK_TTL_KEY: &str = "FORGE_STORAGE_LINK_TTL";

const SECONDS_TO_DURATION: &str = "a duration such as 15s instead of a number of seconds";

/// Every deprecated spelling and its canonical key, read for one minor
/// version after 0.1 and then removed.
pub const DEPRECATED_KEYS: &[DeprecatedKey] = &[
    DeprecatedKey {
        deprecated: "FORGE_BIND",
        canonical: HTTP_ADDR_KEY,
        value_change: "",
    },
    DeprecatedKey {
        deprecated: "FORGE_MAX_BODY_BYTES",
        canonical: HTTP_MAX_BODY_BYTES_KEY,
        value_change: "",
    },
    DeprecatedKey {
        deprecated: "FORGE_REQUEST_TIMEOUT_SECS",
        canonical: HTTP_WRITE_TIMEOUT_KEY,
        value_change: SECONDS_TO_DURATION,
    },
    DeprecatedKey {
        deprecated: "FORGE_SHUTDOWN_GRACE_SECS",
        canonical: HTTP_SHUTDOWN_TIMEOUT_KEY,
        value_change: SECONDS_TO_DURATION,
    },
    DeprecatedKey {
        deprecated: "FORGE_CACHE_DEFAULT_TTL_SECS",
        canonical: CACHE_DEFAULT_TTL_KEY,
        value_change: SECONDS_TO_DURATION,
    },
    DeprecatedKey {
        deprecated: "FORGE_STORAGE_LINK_TTL_SECS",
        canonical: STORAGE_LINK_TTL_KEY,
        value_change: SECONDS_TO_DURATION,
    },
];

/// The deprecated spelling of `canonical`. Every caller names a key of
/// [`DEPRECATED_KEYS`]; a test checks it.
pub(crate) fn deprecated_spelling(canonical: &str) -> &'static str {
    DEPRECATED_KEYS
        .iter()
        .find(|key| key.canonical == canonical)
        .map_or("", |key| key.deprecated)
}

/// The deprecated keys among `keys`, in [`DEPRECATED_KEYS`] order.
pub fn deprecated_keys_in<'k>(keys: impl IntoIterator<Item = &'k str>) -> Vec<DeprecatedKey> {
    let keys: Vec<&str> = keys.into_iter().collect();
    DEPRECATED_KEYS
        .iter()
        .filter(|key| keys.contains(&key.deprecated))
        .copied()
        .collect()
}

/// The one-line warning for a deprecated key.
#[must_use]
pub fn deprecation_warning(key: &DeprecatedKey) -> String {
    let mut line = format!(
        "warning: {} is deprecated and will be removed in the next minor version; use {}",
        key.deprecated, key.canonical
    );
    if !key.value_change.is_empty() {
        line.push_str(" (");
        line.push_str(key.value_change);
        line.push(')');
    }
    line
}

/// Which deprecation warnings were already printed: each one at most once
/// per [`DeprecationNotices`], and the process shares one through
/// [`AppConfig::from_env`](crate::AppConfig::from_env).
#[derive(Debug, Default)]
pub struct DeprecationNotices {
    printed: AtomicU64,
}

impl DeprecationNotices {
    /// No warning printed yet.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            printed: AtomicU64::new(0),
        }
    }

    /// Writes the warning of every deprecated key among `keys` that this
    /// value has not reported before. A failed write is ignored: a warning
    /// must never stop the process.
    pub fn report<'k>(&self, keys: impl IntoIterator<Item = &'k str>, output: &mut dyn io::Write) {
        for key in deprecated_keys_in(keys) {
            let Some(index) = DEPRECATED_KEYS.iter().position(|known| *known == key) else {
                continue;
            };
            let bit = 1_u64 << index;
            if self.printed.fetch_or(bit, Ordering::Relaxed) & bit == 0 {
                let _ = writeln!(output, "{}", deprecation_warning(&key));
            }
        }
    }
}

/// The notices of this process.
pub(crate) static PROCESS_NOTICES: DeprecationNotices = DeprecationNotices::new();

/// Reads a setting spelled `canonical` or, deprecated, the
/// [`deprecated_spelling`] of it. Each spelling is parsed with its own
/// contract; set under both, the parsed values must agree, or loading fails
/// closed naming both keys (never the values, which may be secrets).
pub(crate) fn read_aliased<T: PartialEq>(
    lookup: &impl Fn(&str) -> Option<String>,
    canonical: &'static str,
    parse_canonical: impl FnOnce(String) -> Result<T, ConfigError>,
    parse_deprecated: impl FnOnce(String) -> Result<T, ConfigError>,
) -> Result<Option<T>, ConfigError> {
    let deprecated = deprecated_spelling(canonical);
    let current = lookup(canonical).map(parse_canonical).transpose()?;
    let old = lookup(deprecated).map(parse_deprecated).transpose()?;
    match (current, old) {
        (Some(current), Some(old)) if current != old => Err(ConfigError::ConflictingAlias {
            canonical,
            deprecated,
        }),
        (Some(value), _) | (None, Some(value)) => Ok(Some(value)),
        (None, None) => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_deprecated_key_has_one_distinct_canonical_spelling() {
        assert!(DEPRECATED_KEYS.len() <= 64, "one bit per key");
        for key in DEPRECATED_KEYS {
            assert_ne!(key.deprecated, key.canonical);
            assert!(key.canonical.starts_with("FORGE_"), "{key:?}");
            assert_eq!(deprecated_spelling(key.canonical), key.deprecated);
            assert!(
                crate::KNOWN_KEYS.contains(&key.canonical),
                "{} must be a known key",
                key.canonical
            );
            assert!(
                !crate::KNOWN_KEYS.contains(&key.deprecated),
                "{} must not be listed as current",
                key.deprecated
            );
            assert_eq!(
                DEPRECATED_KEYS
                    .iter()
                    .filter(|other| other.canonical == key.canonical
                        || other.deprecated == key.deprecated)
                    .count(),
                1
            );
        }
    }

    #[test]
    fn each_warning_is_printed_once_per_notices() {
        let notices = DeprecationNotices::new();
        let mut output = Vec::new();
        notices.report(["FORGE_BIND", "FORGE_ENV", "FORGE_BIND"], &mut output);
        notices.report(["FORGE_BIND"], &mut output);
        notices.report(["FORGE_SHUTDOWN_GRACE_SECS", "FORGE_BIND"], &mut output);
        let text = String::from_utf8(output).expect("UTF-8");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines,
            [
                "warning: FORGE_BIND is deprecated and will be removed in the next minor \
                 version; use FORGE_HTTP_ADDR",
                "warning: FORGE_SHUTDOWN_GRACE_SECS is deprecated and will be removed in the \
                 next minor version; use FORGE_HTTP_SHUTDOWN_TIMEOUT (a duration such as 15s \
                 instead of a number of seconds)",
            ]
        );
        let mut silent = Vec::new();
        notices.report(["FORGE_HTTP_ADDR", "FORGE_ENV"], &mut silent);
        assert!(silent.is_empty());
    }
}
