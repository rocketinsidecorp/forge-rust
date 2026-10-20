use thiserror::Error;

/// Every failure of this crate. Messages never contain key material,
/// plaintexts or ciphertexts.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum EncryptionError {
    /// Key material or a key identifier that cannot be used.
    #[error("encryption: invalid key: {0}")]
    InvalidKey(String),
    /// Every authentication failure: wrong key, modified ciphertext or
    /// mismatched associated data. It deliberately says no more.
    #[error("encryption: decryption failed")]
    Decrypt,
    /// A value that is not a Forge ciphertext.
    #[error("encryption: malformed ciphertext")]
    Malformed,
    /// A ciphertext sealed with a key the ring does not hold, such as a
    /// retired key removed before the rotation finished.
    #[error("encryption: unknown key identifier {0:?}")]
    UnknownKey(String),
    /// A plaintext or ciphertext over [`MAX_PLAINTEXT_BYTES`](crate::MAX_PLAINTEXT_BYTES).
    #[error("encryption: value is too large")]
    TooLarge,
    /// The table, column or record id of the associated data is empty.
    #[error("encryption: table, column and record id are required")]
    MissingAad,
    /// The operating system's random source failed.
    #[error("encryption: the random source is unavailable")]
    Randomness,
    /// A typed value could not be converted to or from its stored bytes. The
    /// message names the problem, never the value.
    #[error("encryption: {0}")]
    Encoding(String),
    /// A re-encryption table description is invalid.
    #[error("encryption: {0}")]
    Configuration(String),
}
