//! AES-256-GCM through `ring`, random nonces and unambiguous associated data.

use std::fmt;

use ring::aead::{AES_256_GCM, Aad as RingAad, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use zeroize::{Zeroize as _, Zeroizing};

use crate::{EncryptionError, KeyMaterial, REDACTED};

/// The AES-GCM nonce size in bytes.
pub const NONCE_SIZE: usize = NONCE_LEN;

/// The AES-GCM authentication tag size in bytes.
pub const TAG_SIZE: usize = 16;

/// AES-256-GCM under one key. Debug output is redacted.
pub struct Aead {
    key: LessSafeKey,
}

impl Aead {
    /// AES-256-GCM for a 32-byte key.
    pub fn new(key: &KeyMaterial) -> Result<Self, EncryptionError> {
        let unbound = UnboundKey::new(&AES_256_GCM, key.expose())
            .map_err(|_| EncryptionError::InvalidKey("unusable AES-256 key".to_owned()))?;
        Ok(Self {
            key: LessSafeKey::new(unbound),
        })
    }

    /// Encrypts `plaintext` under `nonce` and returns ciphertext || tag.
    /// The nonce must never repeat under this key; use [`random_nonce`].
    pub fn seal(
        &self,
        nonce: &[u8; NONCE_SIZE],
        plaintext: &[u8],
        associated_data: &[u8],
    ) -> Result<Vec<u8>, EncryptionError> {
        // Sized for the tag so the plaintext copy is never reallocated; it
        // is encrypted in place, and wiped if sealing fails.
        let mut in_out = Vec::with_capacity(plaintext.len() + TAG_SIZE);
        in_out.extend_from_slice(plaintext);
        let sealed = self.key.seal_in_place_append_tag(
            Nonce::assume_unique_for_key(*nonce),
            RingAad::from(associated_data),
            &mut in_out,
        );
        if sealed.is_err() {
            in_out.zeroize();
            return Err(EncryptionError::TooLarge);
        }
        Ok(in_out)
    }

    /// Decrypts ciphertext || tag sealed under `nonce`. Every failure is
    /// [`EncryptionError::Decrypt`].
    pub fn open(
        &self,
        nonce: &[u8],
        ciphertext: &[u8],
        associated_data: &[u8],
    ) -> Result<Zeroizing<Vec<u8>>, EncryptionError> {
        let nonce =
            Nonce::try_assume_unique_for_key(nonce).map_err(|_| EncryptionError::Decrypt)?;
        let mut buffer = Zeroizing::new(ciphertext.to_vec());
        let length = self
            .key
            .open_in_place(nonce, RingAad::from(associated_data), &mut buffer)
            .map_err(|_| EncryptionError::Decrypt)?
            .len();
        buffer.truncate(length);
        Ok(buffer)
    }

    /// Encrypts with a fresh random nonce and returns nonce || ciphertext || tag.
    pub fn seal_prefixed(
        &self,
        plaintext: &[u8],
        associated_data: &[u8],
    ) -> Result<Vec<u8>, EncryptionError> {
        let nonce = random_nonce()?;
        let sealed = self.seal(&nonce, plaintext, associated_data)?;
        let mut out = Vec::with_capacity(NONCE_SIZE + sealed.len());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&sealed);
        Ok(out)
    }

    /// Reverses [`seal_prefixed`](Self::seal_prefixed). Every failure is
    /// [`EncryptionError::Decrypt`].
    pub fn open_prefixed(
        &self,
        sealed: &[u8],
        associated_data: &[u8],
    ) -> Result<Zeroizing<Vec<u8>>, EncryptionError> {
        if sealed.len() < NONCE_SIZE + TAG_SIZE {
            return Err(EncryptionError::Decrypt);
        }
        let (nonce, ciphertext) = sealed.split_at(NONCE_SIZE);
        self.open(nonce, ciphertext, associated_data)
    }
}

impl fmt::Debug for Aead {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "Aead(AES-256-GCM, {REDACTED})")
    }
}

/// A random 96-bit nonce. Random nonces stay safe for up to 2^32 messages per
/// key; rotate keys well before that.
pub fn random_nonce() -> Result<[u8; NONCE_SIZE], EncryptionError> {
    let mut nonce = [0_u8; NONCE_SIZE];
    getrandom::fill(&mut nonce).map_err(|_| EncryptionError::Randomness)?;
    Ok(nonce)
}

/// Encodes `parts` unambiguously as AEAD associated data: each part is
/// prefixed with its 4-byte big-endian length, so `("ab", "c")` and
/// `("a", "bc")` differ. Parts are short identifiers; one longer than
/// `u32::MAX` bytes cannot be encoded and yields an empty result, which never
/// matches a real encoding.
#[must_use]
pub fn associated_data(parts: &[&str]) -> Vec<u8> {
    let mut out = Vec::with_capacity(parts.iter().map(|part| 4 + part.len()).sum());
    for part in parts {
        let Ok(length) = u32::try_from(part.len()) else {
            return Vec::new();
        };
        out.extend_from_slice(&length.to_be_bytes());
        out.extend_from_slice(part.as_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generate_key;

    #[test]
    fn associated_data_is_length_prefixed() {
        assert_ne!(associated_data(&["ab", "c"]), associated_data(&["a", "bc"]));
        assert_eq!(associated_data(&["a"]), [0, 0, 0, 1, b'a']);
        assert_eq!(associated_data(&[""]), [0, 0, 0, 0]);
    }

    #[test]
    fn prefixed_sealing_authenticates_everything() {
        let aead = Aead::new(&generate_key().expect("random")).expect("aead");
        let sealed = aead.seal_prefixed(b"secret", b"ad").expect("seal");
        assert_eq!(sealed.len(), NONCE_SIZE + 6 + TAG_SIZE);
        assert_eq!(
            &*aead.open_prefixed(&sealed, b"ad").expect("open"),
            b"secret"
        );
        assert_eq!(
            aead.open_prefixed(&sealed, b"other"),
            Err(EncryptionError::Decrypt)
        );
        for index in 0..sealed.len() {
            let mut tampered = sealed.clone();
            tampered[index] ^= 1;
            assert_eq!(
                aead.open_prefixed(&tampered, b"ad"),
                Err(EncryptionError::Decrypt)
            );
        }
        assert_eq!(
            aead.open_prefixed(&sealed[..NONCE_SIZE + TAG_SIZE - 1], b"ad"),
            Err(EncryptionError::Decrypt)
        );
        let other = Aead::new(&generate_key().expect("random")).expect("aead");
        assert_eq!(
            other.open_prefixed(&sealed, b"ad"),
            Err(EncryptionError::Decrypt)
        );
        assert_ne!(
            aead.seal_prefixed(b"secret", b"ad").expect("seal"),
            sealed,
            "nonces are random"
        );
        assert!(!format!("{aead:?}").contains("secret"));
    }
}
