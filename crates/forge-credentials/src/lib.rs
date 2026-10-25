//! Encrypted per-environment credentials, Forge's equivalent of Rails'
//! `credentials:edit` (ADR 0041).
//!
//! Secrets live in `config/credentials/<environment>.json.enc`, safe to
//! commit, encrypted with AES-256-GCM under a key that is never committed:
//! `FORGE_MASTER_KEY` or the git-ignored `config/credentials/<environment>.key`
//! (mode 0600). The file's format version, key id and environment name are
//! authenticated, so a file of another environment cannot be substituted.
//! The format is Forge-Go's, byte for byte: a file written by either
//! implementation opens in the other.
//!
//! ```no_run
//! use forge_credentials::Options;
//!
//! # fn main() -> Result<(), forge_credentials::CredentialsError> {
//! // Production fails fast when the file, the key or a required path is absent.
//! let credentials = forge_credentials::open(
//!     "production",
//!     Options::default().with_required(["stripe.api_key"]),
//! )?;
//! let stripe_key = credentials.secret(&["stripe", "api_key"])?; // prints [REDACTED]
//! let ring = credentials.key_ring(&["encryption"])?; // for forge_encryption::Cipher
//! # let _ = (stripe_key, ring);
//! # Ok(())
//! # }
//! ```

#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

mod document;
mod envelope;
mod error;
mod files;
mod init;
mod store;

pub use document::{indent, redact, validate_document};
pub use envelope::{
    FORMAT_VERSION, MAX_FILE_BYTES, MAX_KEYS, MAX_PLAINTEXT_BYTES, key_id, new_key, parse_keys,
    seal, unseal, validate_environment,
};
pub use error::CredentialsError;
pub use files::{
    DEFAULT_DIR, ENV_MASTER_KEY, KeySource, file_name, key_file_name, load_keys, prepare_key_dir,
    read_file, write_file, write_key_file,
};
pub use init::{InitError, init};
pub use store::{Lookup, Options, Store, load, open, parse};
