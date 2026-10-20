use std::io;
use std::path::PathBuf;

use forge_encryption::EncryptionError;
use thiserror::Error;

/// Every failure of this crate. Messages name paths, key ids, line and column
/// numbers and JSON paths; never a key, a credential value or document text.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum CredentialsError {
    /// An environment name that cannot name a file.
    #[error("credentials: invalid environment name")]
    InvalidEnvironment,
    /// The file cannot be authenticated: wrong key, a modified file, or a file
    /// of another environment or format version. The cases are deliberately
    /// indistinguishable.
    #[error(
        "credentials: cannot decrypt (wrong key, modified file, or file from another environment)"
    )]
    Decrypt,
    /// None of the available keys has the key id recorded in the file.
    #[error("credentials: no available key matches the file's key id (file key id {0})")]
    NoMatchingKey(String),
    /// A file that is not a Forge credentials file.
    #[error("credentials: invalid file format: {0}")]
    Format(String),
    /// No credentials file exists for the environment.
    #[error("credentials: file not found: {}", .0.display())]
    NotFound(PathBuf),
    /// Neither `FORGE_MASTER_KEY` nor a key file exists.
    #[error(
        "credentials: no key available (set FORGE_MASTER_KEY or create the environment's .key file)"
    )]
    KeyMissing,
    /// A key file readable or writable by group or others.
    #[error(
        "credentials: key file must not be accessible by group or others (chmod 600): {} is {mode:04o}",
        path.display()
    )]
    KeyPermissions {
        /// The key file.
        path: PathBuf,
        /// Its permission bits.
        mode: u32,
    },
    /// Key text that cannot be used (the message never echoes it).
    #[error("{0}")]
    InvalidKey(String),
    /// The document is not one JSON object without duplicate keys, or too large.
    #[error("{0}")]
    Document(String),
    /// A credential that is not present.
    #[error("credentials: missing credential: {0:?}")]
    Missing(String),
    /// Required credentials that are absent or empty.
    #[error("credentials: missing credential in {environment} credentials: {}", .paths.join(", "))]
    MissingRequired {
        /// The environment of the store.
        environment: String,
        /// Every missing dotted path, sorted.
        paths: Vec<String>,
    },
    /// A credential of an unexpected JSON type.
    #[error("credentials: unexpected credential type: {path:?} is {found}, want {wanted}")]
    Type {
        /// The dotted path.
        path: String,
        /// What the document holds there.
        found: &'static str,
        /// What the caller asked for.
        wanted: &'static str,
    },
    /// A credential object that does not match the requested Rust type.
    #[error("credentials: decode {0:?}: the value does not match the expected type")]
    Decode(String),
    /// A file that must not be replaced already exists.
    #[error("credentials: {} already exists", .0.display())]
    Exists(PathBuf),
    /// A file-system operation failed.
    #[error("credentials: {action} {}: {source}", path.display())]
    Io {
        /// What was being done.
        action: &'static str,
        /// The file or directory.
        path: PathBuf,
        /// The cause.
        source: io::Error,
    },
    /// A primitive failed (key derivation, randomness).
    #[error("credentials: {0}")]
    Encryption(#[from] EncryptionError),
}

impl CredentialsError {
    pub(crate) fn io(action: &'static str, path: impl Into<PathBuf>, source: io::Error) -> Self {
        Self::Io {
            action,
            path: path.into(),
            source,
        }
    }
}
