//! Authenticated attribute encryption with key rotation, Forge's equivalent of
//! Active Record Encryption (ADR 0041), and the primitives encrypted
//! credentials (`forge-credentials`) are built on.
//!
//! Every primitive comes from `ring`: AES-256-GCM, HKDF-SHA256 key derivation
//! and HMAC-SHA256. Forge only composes them, with the exact formats of
//! Forge-Go's `encryption` package, so a value written by one implementation
//! decrypts in the other.
//!
//! - Every ciphertext is bound to its location through associated data
//!   (table, column and record id): a value copied to another row, column or
//!   table fails to decrypt instead of being accepted.
//! - Encryption is always randomized. Equality lookups use an opt-in keyed
//!   blind index ([`Cipher::blind_index`]); there is no deterministic mode.
//! - A [`KeyRing`] has one active key, used for new values, and previous keys
//!   that only decrypt; [`Cipher::reencrypt`] moves a column to the active key.
//!
//! ```
//! use forge_encryption::{Aad, Cipher, Key, KeyRing, generate_key};
//!
//! # fn main() -> Result<(), forge_encryption::EncryptionError> {
//! let ring = KeyRing::new(Key::new("2026-10", generate_key()?)?, [])?;
//! let cipher = Cipher::new(&ring)?;
//! let people_ssn = cipher.attribute("people", "ssn");
//! let id = "0199f5c4-3a52-7d1e-9b7e-3f1c2a4d5e6f";
//! let stored: String = people_ssn.seal(id, &"123-45-6789".to_owned())?;
//! assert!(stored.starts_with("fg1.2026-10."));
//! let plain: String = people_ssn.open(id, &stored)?;
//! assert_eq!(plain, "123-45-6789");
//! // The same text does not decrypt for another row.
//! assert!(cipher.decrypt(&stored, &Aad::new("people", "ssn", "another-row")).is_err());
//! # Ok(())
//! # }
//! ```

#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

mod aead;
mod cipher;
mod error;
mod key;
mod rotate;
mod sql;
mod value;

pub use aead::{Aead, NONCE_SIZE, TAG_SIZE, associated_data, random_nonce};
pub use cipher::{Aad, Cipher, MAX_PLAINTEXT_BYTES};
pub use error::EncryptionError;
pub use key::{
    KEY_SIZE, Key, KeyMaterial, KeyRing, derive_key, encode_key, generate_key, parse_key,
    valid_key_id,
};
pub use rotate::{
    BoxError, Record, ReencryptError, ReencryptFailure, Report, Rewriter, RotateOptions, Update,
};
pub use sql::{IdType, Placeholders, SqlColumn};
pub use value::{Attribute, Json, Plaintext};

#[cfg(feature = "mysql")]
pub use sql::mysql;
#[cfg(feature = "postgres")]
pub use sql::postgres;
#[cfg(feature = "sqlite")]
pub use sql::sqlite;

/// What every redacted value prints as.
pub(crate) const REDACTED: &str = "[REDACTED]";
