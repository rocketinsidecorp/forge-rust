//! Attribute encryption with a key ring: `fg1.<key id>.<base64url>` values
//! bound to their table, column and record id, and keyed blind indexes.

use std::fmt;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::hmac;
use zeroize::Zeroizing;

use crate::aead::{NONCE_SIZE, TAG_SIZE};
use crate::{Aead, EncryptionError, KeyRing, associated_data, derive_key, valid_key_id};

/// The largest attribute plaintext.
pub const MAX_PLAINTEXT_BYTES: usize = 1 << 20;

const TEXT_PREFIX: &str = "fg1";
const PURPOSE_ATTRIBUTES: &str = "attributes/aead";
const PURPOSE_BLIND_INDEX: &str = "attributes/blind-index";
const AD_DOMAIN: &str = "forge/attribute/v1";
const INDEX_DOMAIN: &str = "forge/blind-index/v1";

/// Where a value lives. It is authenticated with the value, so a ciphertext
/// copied to another table, column or row fails to decrypt. Every field is
/// required; `record_id` should be the row's immutable primary key (a UUIDv7
/// in its canonical hyphenated form in Forge applications).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Aad<'a> {
    /// The table name.
    pub table: &'a str,
    /// The column name.
    pub column: &'a str,
    /// The row's primary key as text.
    pub record_id: &'a str,
}

impl<'a> Aad<'a> {
    /// The location `table.column` of row `record_id`.
    #[must_use]
    pub const fn new(table: &'a str, column: &'a str, record_id: &'a str) -> Self {
        Self {
            table,
            column,
            record_id,
        }
    }

    fn validate(&self) -> Result<(), EncryptionError> {
        if self.table.is_empty() || self.column.is_empty() || self.record_id.is_empty() {
            return Err(EncryptionError::MissingAad);
        }
        Ok(())
    }

    fn encode(&self, key_id: &str) -> Vec<u8> {
        associated_data(&[AD_DOMAIN, key_id, self.table, self.column, self.record_id])
    }
}

struct KeySet {
    id: String,
    aead: Aead,
    index: hmac::Key,
}

/// Encrypts and decrypts attribute values with a key ring: it writes with the
/// active key and reads with any. Immutable and safe for concurrent use.
pub struct Cipher {
    /// The active key first, in ring order.
    sets: Vec<KeySet>,
}

impl Cipher {
    /// Derives the encryption and blind-index subkeys of every ring key with
    /// HKDF-SHA256.
    pub fn new(ring: &KeyRing) -> Result<Self, EncryptionError> {
        let sets = ring
            .keys()
            .map(|key| {
                let aead_key = derive_key(key.material(), PURPOSE_ATTRIBUTES)?;
                let index_key = derive_key(key.material(), PURPOSE_BLIND_INDEX)?;
                Ok(KeySet {
                    id: key.id().to_owned(),
                    aead: Aead::new(&aead_key)?,
                    index: hmac::Key::new(hmac::HMAC_SHA256, index_key.expose()),
                })
            })
            .collect::<Result<Vec<_>, EncryptionError>>()?;
        if sets.is_empty() {
            return Err(EncryptionError::InvalidKey(
                "the key ring is empty".to_owned(),
            ));
        }
        Ok(Self { sets })
    }

    fn active(&self) -> &KeySet {
        // `new` refuses an empty ring.
        &self.sets[0]
    }

    /// The identifier used for new ciphertexts.
    #[must_use]
    pub fn active_key_id(&self) -> &str {
        &self.active().id
    }

    /// Seals `plaintext` with the active key and a fresh random nonce. The
    /// result, `fg1.<key id>.<base64url(nonce || ciphertext || tag)>`, fits
    /// any TEXT column. Two encryptions of the same value differ.
    pub fn encrypt(&self, plaintext: &[u8], aad: &Aad<'_>) -> Result<String, EncryptionError> {
        aad.validate()?;
        if plaintext.len() > MAX_PLAINTEXT_BYTES {
            return Err(EncryptionError::TooLarge);
        }
        let active = self.active();
        let sealed = active
            .aead
            .seal_prefixed(plaintext, &aad.encode(&active.id))?;
        Ok(format!(
            "{TEXT_PREFIX}.{}.{}",
            active.id,
            URL_SAFE_NO_PAD.encode(sealed)
        ))
    }

    fn parse(&self, text: &str) -> Result<(&KeySet, Vec<u8>), EncryptionError> {
        let Some((prefix, rest)) = text.split_once('.') else {
            return Err(EncryptionError::Malformed);
        };
        if prefix != TEXT_PREFIX {
            return Err(EncryptionError::Malformed);
        }
        let Some((key_id, payload)) = rest.split_once('.') else {
            return Err(EncryptionError::Malformed);
        };
        // Unpadded base64 length of the largest sealed value.
        let max_payload = ((MAX_PLAINTEXT_BYTES + 64) * 4).div_ceil(3);
        if !valid_key_id(key_id) || payload.len() > max_payload {
            return Err(EncryptionError::Malformed);
        }
        let sealed = URL_SAFE_NO_PAD
            .decode(payload)
            .map_err(|_| EncryptionError::Malformed)?;
        let set = self
            .sets
            .iter()
            .find(|set| set.id == key_id)
            .ok_or_else(|| EncryptionError::UnknownKey(key_id.to_owned()))?;
        Ok((set, sealed))
    }

    /// Opens a value produced by [`encrypt`](Self::encrypt) for the same
    /// location. The key is picked by the identifier in the text, so values
    /// written before a rotation stay readable while the previous key is in
    /// the ring. A wrong key, modified text or another table, column or record
    /// id all fail with [`EncryptionError::Decrypt`].
    pub fn decrypt(
        &self,
        text: &str,
        aad: &Aad<'_>,
    ) -> Result<Zeroizing<Vec<u8>>, EncryptionError> {
        aad.validate()?;
        let (set, sealed) = self.parse(text)?;
        if sealed.len() < NONCE_SIZE + TAG_SIZE {
            return Err(EncryptionError::Decrypt);
        }
        set.aead.open_prefixed(&sealed, &aad.encode(&set.id))
    }

    /// Whether `text` was sealed with a key other than the active one.
    pub fn needs_rotation(&self, text: &str) -> Result<bool, EncryptionError> {
        let (set, _) = self.parse(text)?;
        Ok(set.id != self.active().id)
    }

    /// A keyed digest of `plaintext` for equality lookups on an encrypted
    /// column: HMAC-SHA256 under a key derived for blind indexing, bound to
    /// the table and column. Equal plaintexts in one column give equal
    /// digests; nothing else is revealed without the key. An explicit opt-in:
    /// normalize the input (case, whitespace) first and store the digest in a
    /// separate indexed column.
    pub fn blind_index(
        &self,
        table: &str,
        column: &str,
        plaintext: &[u8],
    ) -> Result<String, EncryptionError> {
        blind_index(self.active(), table, column, plaintext)
    }

    /// The digests under every ring key, the active key first. Query with
    /// `index_column = ANY(...)` (or `IN (...)`) during a rotation, so rows
    /// indexed with a previous key are still found.
    pub fn blind_indexes(
        &self,
        table: &str,
        column: &str,
        plaintext: &[u8],
    ) -> Result<Vec<String>, EncryptionError> {
        self.sets
            .iter()
            .map(|set| blind_index(set, table, column, plaintext))
            .collect()
    }
}

fn blind_index(
    set: &KeySet,
    table: &str,
    column: &str,
    plaintext: &[u8],
) -> Result<String, EncryptionError> {
    if table.is_empty() || column.is_empty() {
        return Err(EncryptionError::Configuration(
            "table and column are required".to_owned(),
        ));
    }
    if plaintext.len() > MAX_PLAINTEXT_BYTES {
        return Err(EncryptionError::TooLarge);
    }
    let mut context = hmac::Context::with_key(&set.index);
    context.update(&associated_data(&[INDEX_DOMAIN, table, column]));
    context.update(plaintext);
    Ok(URL_SAFE_NO_PAD.encode(context.sign().as_ref()))
}

impl fmt::Debug for Cipher {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Cipher")
            .field(
                "key_ids",
                &self
                    .sets
                    .iter()
                    .map(|set| set.id.as_str())
                    .collect::<Vec<_>>(),
            )
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Key, generate_key};

    fn ring(ids: &[&str]) -> KeyRing {
        let mut keys = ids
            .iter()
            .map(|id| Key::new(*id, generate_key().expect("random")).expect("key"));
        let active = keys.next().expect("one key");
        KeyRing::new(active, keys).expect("ring")
    }

    const PEOPLE: Aad<'static> = Aad::new("people", "ssn", "0199f5c4-3a52-7d1e-9b7e-3f1c2a4d5e6f");

    #[test]
    fn values_round_trip_and_are_randomized() {
        let cipher = Cipher::new(&ring(&["k1"])).expect("cipher");
        let first = cipher.encrypt(b"123-45-6789", &PEOPLE).expect("encrypt");
        let second = cipher.encrypt(b"123-45-6789", &PEOPLE).expect("encrypt");
        assert_ne!(first, second);
        assert!(first.starts_with("fg1.k1."));
        assert!(!first.contains("123-45"));
        assert_eq!(
            &*cipher.decrypt(&first, &PEOPLE).expect("decrypt"),
            b"123-45-6789"
        );
        assert_eq!(
            &*cipher
                .decrypt(&cipher.encrypt(b"", &PEOPLE).expect("e"), &PEOPLE)
                .expect("d"),
            b""
        );
    }

    #[test]
    fn a_value_moved_to_another_row_column_or_table_fails() {
        let cipher = Cipher::new(&ring(&["k1"])).expect("cipher");
        let sealed = cipher.encrypt(b"secret", &PEOPLE).expect("encrypt");
        for other in [
            Aad::new("people", "ssn", "0199f5c4-3a52-7d1e-9b7e-3f1c2a4d5e70"),
            Aad::new("people", "notes", PEOPLE.record_id),
            Aad::new("accounts", "ssn", PEOPLE.record_id),
        ] {
            assert_eq!(
                cipher.decrypt(&sealed, &other),
                Err(EncryptionError::Decrypt)
            );
        }
        assert_eq!(
            cipher.decrypt(&sealed, &Aad::new("people", "", "x")),
            Err(EncryptionError::MissingAad)
        );
        assert_eq!(
            cipher.encrypt(b"x", &Aad::new("people", "ssn", "")),
            Err(EncryptionError::MissingAad)
        );
    }

    #[test]
    fn tampering_truncation_and_wrong_keys_fail_closed() {
        let cipher = Cipher::new(&ring(&["k1"])).expect("cipher");
        let sealed = cipher.encrypt(b"secret", &PEOPLE).expect("encrypt");
        // Flip each payload character.
        let (head, payload) = sealed.split_at("fg1.k1.".len());
        for index in 0..payload.len() {
            let mut bytes = payload.as_bytes().to_vec();
            bytes[index] = if bytes[index] == b'A' { b'B' } else { b'A' };
            let tampered = format!("{head}{}", String::from_utf8(bytes).expect("ascii"));
            let result = cipher.decrypt(&tampered, &PEOPLE);
            assert!(
                matches!(
                    result,
                    Err(EncryptionError::Decrypt | EncryptionError::Malformed)
                ),
                "{result:?}"
            );
        }
        for cut in 1..sealed.len() {
            assert!(matches!(
                cipher.decrypt(&sealed[..cut], &PEOPLE),
                Err(EncryptionError::Decrypt | EncryptionError::Malformed)
            ));
        }
        assert_eq!(
            cipher.decrypt("fg1.k1.AAAA", &PEOPLE),
            Err(EncryptionError::Decrypt)
        );
        for malformed in [
            "",
            "fg1",
            "fg2.k1.AAAA",
            "fg1.k1",
            "fg1.bad id.AAAA",
            "fg1.k1.***",
        ] {
            assert_eq!(
                cipher.decrypt(malformed, &PEOPLE),
                Err(EncryptionError::Malformed),
                "{malformed}"
            );
        }
        // Relabeling the key id is caught: it is authenticated.
        let relabeled = Cipher::new(&ring(&["k1", "k2"])).expect("cipher");
        let other = relabeled.encrypt(b"secret", &PEOPLE).expect("encrypt");
        assert_eq!(
            relabeled.decrypt(&other.replacen("fg1.k1.", "fg1.k2.", 1), &PEOPLE),
            Err(EncryptionError::Decrypt)
        );
        // Same identifier, different material.
        let stranger = Cipher::new(&ring(&["k1"])).expect("cipher");
        assert_eq!(
            stranger.decrypt(&sealed, &PEOPLE),
            Err(EncryptionError::Decrypt)
        );
        assert_eq!(
            stranger.decrypt(&sealed.replacen("k1", "k9", 1), &PEOPLE),
            Err(EncryptionError::UnknownKey("k9".to_owned()))
        );
        assert_eq!(
            cipher.encrypt(&vec![0; MAX_PLAINTEXT_BYTES + 1], &PEOPLE),
            Err(EncryptionError::TooLarge)
        );
    }

    #[test]
    fn rotation_keeps_old_values_readable() {
        let old_ring = ring(&["2025-03"]);
        let old = Cipher::new(&old_ring).expect("cipher");
        let sealed = old.encrypt(b"secret", &PEOPLE).expect("encrypt");
        let new_ring = KeyRing::new(
            Key::new("2026-10", generate_key().expect("random")).expect("key"),
            [old_ring.active().clone()],
        )
        .expect("ring");
        let rotated = Cipher::new(&new_ring).expect("cipher");
        assert_eq!(rotated.active_key_id(), "2026-10");
        assert!(rotated.needs_rotation(&sealed).expect("parse"));
        assert_eq!(
            &*rotated.decrypt(&sealed, &PEOPLE).expect("old value"),
            b"secret"
        );
        let fresh = rotated.encrypt(b"secret", &PEOPLE).expect("encrypt");
        assert!(!rotated.needs_rotation(&fresh).expect("parse"));
        assert!(old.decrypt(&fresh, &PEOPLE).is_err());
    }

    #[test]
    fn blind_indexes_are_keyed_and_bound_to_the_column() {
        let ring = ring(&["k1", "k0"]);
        let cipher = Cipher::new(&ring).expect("cipher");
        let index = cipher.blind_index("people", "ssn", b"123").expect("index");
        assert_eq!(
            index,
            cipher.blind_index("people", "ssn", b"123").expect("index")
        );
        assert_ne!(
            index,
            cipher
                .blind_index("people", "tax_id", b"123")
                .expect("index")
        );
        assert_ne!(
            index,
            cipher.blind_index("people", "ssn", b"124").expect("index")
        );
        let all = cipher
            .blind_indexes("people", "ssn", b"123")
            .expect("indexes");
        assert_eq!(all.len(), 2);
        assert_eq!(all[0], index);
        assert_ne!(all[1], index);
        assert!(cipher.blind_index("", "ssn", b"1").is_err());
        let debug = format!("{cipher:?}");
        assert!(debug.contains("k1") && debug.contains("k0"));
    }
}
