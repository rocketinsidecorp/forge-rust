//! Exit codes and errors of `forge deploy`, `forge logs` and `forge app`.
//! The codes are part of the command contract (docs/cli.md).

use std::collections::BTreeMap;
use std::fmt;

/// Invalid arguments or configuration, SSH failures and failed remote
/// commands.
pub(crate) const EXIT_FAILED: u8 = 1;
/// Another deploy holds the lock; nothing changed.
pub(crate) const EXIT_LOCK_HELD: u8 = 3;
/// A new container failed its health check; every host serves the previous
/// version again.
pub(crate) const EXIT_UNHEALTHY: u8 = 4;
/// The migration failed; no container was started or switched.
pub(crate) const EXIT_MIGRATION_FAILED: u8 = 5;
/// A local hook exited non-zero; the step after it did not run.
pub(crate) const EXIT_HOOK_FAILED: u8 = 6;
/// A failure could not be fully reverted: hosts may run different
/// versions. Run `forge deploy details`.
pub(crate) const EXIT_INCONSISTENT: u8 = 7;

/// The remote exit status `acquireLock` uses when the lock is held.
pub(crate) const LOCK_HELD_EXIT_STATUS: i32 = 75;

/// A remote command that exited non-zero.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RemoteError {
    pub(crate) host: String,
    pub(crate) exit_code: i32,
    pub(crate) stderr: String,
    /// What the command printed before failing (not part of the message).
    pub(crate) stdout: String,
}

impl fmt::Display for RemoteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut message = self.stderr.trim().to_owned();
        if message.len() > 2000 {
            let mut cut = 2000;
            while !message.is_char_boundary(cut) {
                cut -= 1;
            }
            message.truncate(cut);
            message.push_str("...");
        }
        if message.is_empty() {
            write!(
                formatter,
                "{}: remote command exited with status {}",
                self.host, self.exit_code
            )
        } else {
            write!(
                formatter,
                "{}: remote command exited with status {}: {message}",
                self.host, self.exit_code
            )
        }
    }
}

/// The lock record stored in the lock directory on each host.
#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub(crate) struct LockInfo {
    pub(crate) holder: String,
    pub(crate) message: String,
    #[serde(with = "rfc3339")]
    pub(crate) acquired_at: forge_proxy::clock::Timestamp,
}

/// Go's `time.Time` JSON form.
pub(crate) mod rfc3339 {
    use forge_proxy::clock::Timestamp;

    pub(crate) fn serialize<S: serde::Serializer>(
        value: &Timestamp,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&value.rfc3339_nano())
    }

    pub(crate) fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Timestamp, D::Error> {
        let text: String = serde::Deserialize::deserialize(deserializer)?;
        Timestamp::parse_rfc3339(&text).ok_or_else(|| serde::de::Error::custom("invalid time"))
    }
}

/// An error with what callers need to classify it, the way Forge-Go's
/// wrapped errors are inspected with `errors.As`.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct DeployError {
    pub(crate) message: String,
    /// The exit code set with [`DeployError::with_code`]; `None` exits 1.
    pub(crate) code: Option<u8>,
    /// The status of a failed remote command somewhere in the chain.
    pub(crate) remote_exit: Option<i32>,
    /// A new container failed its health or readiness check.
    pub(crate) health: bool,
}

impl fmt::Display for DeployError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for DeployError {}

impl From<String> for DeployError {
    fn from(message: String) -> Self {
        Self::new(message)
    }
}

impl From<RemoteError> for DeployError {
    fn from(error: RemoteError) -> Self {
        Self {
            message: error.to_string(),
            remote_exit: Some(error.exit_code),
            ..Self::default()
        }
    }
}

impl DeployError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            ..Self::default()
        }
    }

    /// The process exit code.
    pub(crate) fn exit_code(&self) -> u8 {
        self.code.unwrap_or(EXIT_FAILED)
    }

    /// Go's `withCode`: the outermost code wins.
    #[must_use]
    pub(crate) fn with_code(mut self, code: u8) -> Self {
        self.code = Some(code);
        self
    }

    /// Go's `fmt.Errorf("PREFIX: %w", err)`.
    #[must_use]
    pub(crate) fn context(mut self, prefix: impl fmt::Display) -> Self {
        self.message = format!("{prefix}: {}", self.message);
        self
    }

    /// Go's `fmt.Errorf("%w; SUFFIX", err)`.
    #[must_use]
    pub(crate) fn suffix(mut self, suffix: impl fmt::Display) -> Self {
        self.message = format!("{}; {suffix}", self.message);
        self
    }

    /// Marks a failed health or readiness check.
    #[must_use]
    pub(crate) fn health(mut self) -> Self {
        self.health = true;
        self
    }

    /// Go's `errors.Join`: messages on separate lines; the first code and
    /// remote status found win, as `errors.As` would find them.
    pub(crate) fn join(errors: Vec<Self>) -> Option<Self> {
        if errors.is_empty() {
            return None;
        }
        Some(Self {
            message: errors
                .iter()
                .map(|e| e.message.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
            code: errors.iter().find_map(|e| e.code),
            remote_exit: errors.iter().find_map(|e| e.remote_exit),
            health: errors.iter().any(|e| e.health),
        })
    }

    /// The deploy lock is held by others.
    pub(crate) fn lock_held(holders: &BTreeMap<String, LockInfo>) -> Self {
        let parts: Vec<String> = holders
            .iter()
            .map(|(host, info)| {
                format!(
                    "{host}: held by {} since {} ({})",
                    info.holder,
                    info.acquired_at.human(),
                    info.message
                )
            })
            .collect();
        Self::new(format!(
            "deploy lock is held: {}; wait, or release it with forge deploy lock release",
            parts.join("; ")
        ))
        .with_code(EXIT_LOCK_HELD)
    }
}
