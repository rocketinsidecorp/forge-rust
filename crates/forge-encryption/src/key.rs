//! Master keys, their text form, HKDF derivation and key rings.

use std::collections::BTreeMap;
use std::fmt;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::hkdf;
use zeroize::Zeroizing;

use crate::{EncryptionError, REDACTED};

/// The size of a master key in bytes.
pub const KEY_SIZE: usize = 32;

/// The longest key identifier; identifiers are stored beside every ciphertext.
const MAX_KEY_ID_LENGTH: usize = 32;

/// 32 bytes of secret key material, wiped from memory when dropped. Debug
/// output is redacted.
#[derive(Clone)]
pub struct KeyMaterial(Zeroizing<[u8; KEY_SIZE]>);

impl KeyMaterial {
    /// Copies exactly [`KEY_SIZE`] bytes.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, EncryptionError> {
        let mut material = Zeroizing::new([0_u8; KEY_SIZE]);
        if bytes.len() != KEY_SIZE {
            return Err(EncryptionError::InvalidKey(format!(
                "key must be {KEY_SIZE} bytes"
            )));
        }
        material.copy_from_slice(bytes);
        Ok(Self(material))
    }

    /// The raw bytes, for a primitive that needs them.
    #[must_use]
    pub fn expose(&self) -> &[u8; KEY_SIZE] {
        &self.0
    }
}

impl fmt::Debug for KeyMaterial {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "KeyMaterial({REDACTED})")
    }
}

/// A new random 256-bit master key from the operating system's CSPRNG.
pub fn generate_key() -> Result<KeyMaterial, EncryptionError> {
    let mut material = Zeroizing::new([0_u8; KEY_SIZE]);
    getrandom::fill(material.as_mut_slice()).map_err(|_| EncryptionError::Randomness)?;
    Ok(KeyMaterial(material))
}

/// The canonical text form of a key: unpadded base64url.
#[must_use]
pub fn encode_key(key: &KeyMaterial) -> Zeroizing<String> {
    Zeroizing::new(URL_SAFE_NO_PAD.encode(key.expose()))
}

/// Decodes the canonical text form of a key. It accepts exactly 32 bytes of
/// unpadded base64url and never echoes the input.
pub fn parse_key(encoded: &str) -> Result<KeyMaterial, EncryptionError> {
    let invalid = || {
        EncryptionError::InvalidKey(format!(
            "must be canonical unpadded base64url for exactly {KEY_SIZE} bytes"
        ))
    };
    let raw = Zeroizing::new(URL_SAFE_NO_PAD.decode(encoded).map_err(|_| invalid())?);
    let key = KeyMaterial::from_bytes(&raw).map_err(|_| invalid())?;
    if encode_key(&key).as_str() != encoded {
        return Err(invalid());
    }
    Ok(key)
}

/// Whether `id` is a usable key identifier: 1 to 32 ASCII letters, digits,
/// underscores or hyphens.
#[must_use]
pub fn valid_key_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_KEY_ID_LENGTH
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

/// Derives a 256-bit key for one purpose from a master key with HKDF-SHA256
/// (no salt, info `forge/v1/<purpose>`). Different purposes yield unrelated
/// keys, so one master key can serve encryption and blind indexing.
pub fn derive_key(master: &KeyMaterial, purpose: &str) -> Result<KeyMaterial, EncryptionError> {
    if purpose.is_empty() {
        return Err(EncryptionError::InvalidKey(
            "key purpose is required".to_owned(),
        ));
    }
    let info = format!("forge/v1/{purpose}");
    let info = [info.as_bytes()];
    // An empty salt is HKDF's default: HMAC pads it to the same all-zero key
    // as a HashLen string of zeros (RFC 5869, section 2.2).
    let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, &[]).extract(master.expose());
    let okm = prk
        .expand(&info, hkdf::HKDF_SHA256)
        .map_err(|_| EncryptionError::InvalidKey("key derivation failed".to_owned()))?;
    let mut derived = Zeroizing::new([0_u8; KEY_SIZE]);
    okm.fill(derived.as_mut_slice())
        .map_err(|_| EncryptionError::InvalidKey("key derivation failed".to_owned()))?;
    Ok(KeyMaterial(derived))
}

/// A master key and its identifier. The identifier is stored beside
/// ciphertexts and is not secret; the material never prints.
#[derive(Clone)]
pub struct Key {
    id: String,
    material: KeyMaterial,
}

impl Key {
    /// A key named `id`, which must satisfy [`valid_key_id`].
    pub fn new(id: impl Into<String>, material: KeyMaterial) -> Result<Self, EncryptionError> {
        let id = id.into();
        if !valid_key_id(&id) {
            return Err(EncryptionError::InvalidKey(format!(
                "identifier must be 1-{MAX_KEY_ID_LENGTH} letters, digits, '_' or '-'"
            )));
        }
        Ok(Self { id, material })
    }

    /// The identifier.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The secret material.
    #[must_use]
    pub fn material(&self) -> &KeyMaterial {
        &self.material
    }
}

impl fmt::Debug for Key {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Key")
            .field("id", &self.id)
            .field("material", &REDACTED)
            .finish()
    }
}

/// The active key, used for every new ciphertext, and previous keys, which
/// remain usable for decryption only. Immutable; identifiers are unique.
#[derive(Clone)]
pub struct KeyRing {
    /// The active key first.
    keys: Vec<Key>,
}

impl KeyRing {
    /// A ring of `active` and `previous`.
    pub fn new(
        active: Key,
        previous: impl IntoIterator<Item = Key>,
    ) -> Result<Self, EncryptionError> {
        let mut keys = vec![active];
        for key in previous {
            if keys.iter().any(|existing| existing.id == key.id) {
                return Err(EncryptionError::InvalidKey(format!(
                    "duplicate key identifier {:?}",
                    key.id
                )));
            }
            keys.push(key);
        }
        Ok(Self { keys })
    }

    /// A ring from an active identifier and a map of identifier to canonical
    /// text keys, the shape stored in credentials. Previous keys are ordered
    /// by identifier.
    pub fn parse(active: &str, keys: &BTreeMap<String, String>) -> Result<Self, EncryptionError> {
        let Some(active_text) = keys.get(active) else {
            return Err(EncryptionError::InvalidKey(format!(
                "active key {active:?} is not among the keys"
            )));
        };
        let parse = |id: &str, text: &str| {
            let material = parse_key(text).map_err(|error| match error {
                EncryptionError::InvalidKey(reason) => {
                    EncryptionError::InvalidKey(format!("key {id:?}: {reason}"))
                }
                other => other,
            })?;
            Key::new(id, material)
        };
        let active_key = parse(active, active_text)?;
        let previous = keys
            .iter()
            .filter(|(id, _)| id.as_str() != active)
            .map(|(id, text)| parse(id, text))
            .collect::<Result<Vec<_>, _>>()?;
        Self::new(active_key, previous)
    }

    /// The key used for new ciphertexts.
    #[must_use]
    pub fn active(&self) -> &Key {
        // `new` always stores the active key first.
        &self.keys[0]
    }

    /// The identifier of the active key.
    #[must_use]
    pub fn active_id(&self) -> &str {
        self.active().id()
    }

    /// Every identifier, the active one first.
    #[must_use]
    pub fn ids(&self) -> Vec<&str> {
        self.keys.iter().map(Key::id).collect()
    }

    /// Every key, the active one first.
    pub fn keys(&self) -> impl Iterator<Item = &Key> {
        self.keys.iter()
    }
}

impl fmt::Debug for KeyRing {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("KeyRing")
            .field("ids", &self.ids())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_round_trip_through_their_canonical_text_only() {
        let key = generate_key().expect("random");
        let text = encode_key(&key);
        assert_eq!(text.len(), 43);
        assert_eq!(parse_key(&text).expect("parse").expose(), key.expose());
        for bad in [
            "",
            "short",
            &format!("{}=", text.as_str()),
            &text.replace(['-', '_'], "+"),
        ] {
            if bad != text.as_str() {
                let error = parse_key(bad).expect_err("refused");
                assert!(!error.to_string().contains(bad) || bad.is_empty());
            }
        }
        // A non-canonical final character (extra low bits) is refused.
        let mut tweaked = text.to_string();
        let last = tweaked.pop().expect("char");
        let alternative = if last == 'A' { 'B' } else { 'A' };
        tweaked.push(alternative);
        if let Ok(parsed) = parse_key(&tweaked) {
            assert_eq!(encode_key(&parsed).as_str(), tweaked);
        }
    }

    #[test]
    fn identifiers_and_rings_are_validated() {
        assert!(valid_key_id("2026-10_a"));
        for bad in ["", "a.b", "a b", &"x".repeat(33), "ключ"] {
            assert!(!valid_key_id(bad), "{bad}");
        }
        let key = generate_key().expect("random");
        assert!(Key::new("a.b", key.clone()).is_err());
        let one = Key::new("one", key.clone()).expect("key");
        assert!(KeyRing::new(one.clone(), [one.clone()]).is_err());
        let ring = KeyRing::new(
            one,
            [Key::new("two", generate_key().expect("random")).expect("key")],
        )
        .expect("ring");
        assert_eq!(ring.ids(), ["one", "two"]);
        let debug = format!("{ring:?} {:?}", ring.active());
        assert!(!debug.contains(encode_key(&key).as_str()), "{debug}");
        assert!(debug.contains(REDACTED));
    }

    #[test]
    fn parsed_rings_put_the_active_key_first_and_sort_the_others() {
        let text = |key: &KeyMaterial| encode_key(key).to_string();
        let keys: BTreeMap<String, String> = [
            ("b".to_owned(), text(&generate_key().expect("random"))),
            ("c".to_owned(), text(&generate_key().expect("random"))),
            ("a".to_owned(), text(&generate_key().expect("random"))),
        ]
        .into();
        let ring = KeyRing::parse("c", &keys).expect("ring");
        assert_eq!(ring.ids(), ["c", "a", "b"]);
        assert!(KeyRing::parse("missing", &keys).is_err());
        let mut broken = keys.clone();
        broken.insert("d".to_owned(), "secret-looking-text".to_owned());
        let error = KeyRing::parse("c", &broken).expect_err("bad key");
        assert!(!error.to_string().contains("secret-looking-text"));
    }

    #[test]
    fn derivation_separates_purposes() {
        let master = generate_key().expect("random");
        let first = derive_key(&master, "attributes/aead").expect("derive");
        let second = derive_key(&master, "attributes/blind-index").expect("derive");
        assert_ne!(first.expose(), second.expose());
        assert_eq!(
            derive_key(&master, "attributes/aead")
                .expect("derive")
                .expose(),
            first.expose()
        );
        assert!(derive_key(&master, "").is_err());
    }
}
