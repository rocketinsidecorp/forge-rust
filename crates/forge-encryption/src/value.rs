//! Typed attribute values: the Forge-owned way to encrypt a model column.

use std::io;

use serde::Serialize;
use serde::de::DeserializeOwned;
use zeroize::Zeroizing;

use crate::{Aad, Cipher, EncryptionError};

/// A value that can be stored in an encrypted column. Strings and byte
/// vectors are stored as they are; wrap any other serde type in [`Json`].
/// This is the same encoding as Forge-Go's `encryption.Value[T]`, so either
/// implementation reads the other's columns.
pub trait Plaintext: Sized {
    /// The bytes to encrypt.
    fn to_plaintext(&self) -> Result<Zeroizing<Vec<u8>>, EncryptionError>;
    /// The value of decrypted bytes.
    fn from_plaintext(bytes: &[u8]) -> Result<Self, EncryptionError>;
}

impl Plaintext for String {
    fn to_plaintext(&self) -> Result<Zeroizing<Vec<u8>>, EncryptionError> {
        Ok(Zeroizing::new(self.as_bytes().to_vec()))
    }

    fn from_plaintext(bytes: &[u8]) -> Result<Self, EncryptionError> {
        // Validate in place: only the returned value copies the plaintext.
        // `String::from_utf8(bytes.to_vec())` would drop an unwiped copy in
        // its error.
        std::str::from_utf8(bytes)
            .map(str::to_owned)
            .map_err(|_| EncryptionError::Encoding("the decrypted value is not UTF-8".to_owned()))
    }
}

impl Plaintext for Vec<u8> {
    fn to_plaintext(&self) -> Result<Zeroizing<Vec<u8>>, EncryptionError> {
        Ok(Zeroizing::new(self.clone()))
    }

    fn from_plaintext(bytes: &[u8]) -> Result<Self, EncryptionError> {
        Ok(bytes.to_vec())
    }
}

/// Stores any serde type as JSON inside the encrypted column.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Json<T>(pub T);

impl<T: Serialize + DeserializeOwned> Plaintext for Json<T> {
    fn to_plaintext(&self) -> Result<Zeroizing<Vec<u8>>, EncryptionError> {
        let mut out = ZeroizingWriter(Zeroizing::new(Vec::new()));
        serde_json::to_writer(&mut out, &self.0).map_err(|_| {
            EncryptionError::Encoding("the value cannot be encoded as JSON".to_owned())
        })?;
        Ok(out.0)
    }

    fn from_plaintext(bytes: &[u8]) -> Result<Self, EncryptionError> {
        // serde_json's messages can quote the input; report the kind only.
        serde_json::from_slice(bytes).map(Json).map_err(|_| {
            EncryptionError::Encoding(
                "the decrypted JSON does not match the expected type".to_owned(),
            )
        })
    }
}

/// A JSON sink that wipes every buffer it outgrows. `serde_json::to_vec`
/// grows a plain `Vec`, and each reallocation frees a partial copy of the
/// plaintext without wiping it.
struct ZeroizingWriter(Zeroizing<Vec<u8>>);

impl io::Write for ZeroizingWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let needed = self
            .0
            .len()
            .checked_add(data.len())
            .ok_or_else(|| io::Error::other("value too large"))?;
        if needed > self.0.capacity() {
            let capacity = needed.max(self.0.capacity().saturating_mul(2)).max(64);
            let mut grown = Zeroizing::new(Vec::with_capacity(capacity));
            grown.extend_from_slice(&self.0);
            // The old buffer is wiped (its whole capacity) when dropped here.
            self.0 = grown;
        }
        self.0.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// One encrypted column, `table.column`: seals values for a row and opens
/// them again. Created by [`Cipher::attribute`].
///
/// The stored text goes into an ordinary TEXT column; bind it like any
/// string. The record id must be known before the insert (a UUIDv7 generated
/// in Rust) and be the row's immutable primary key.
#[derive(Clone, Copy, Debug)]
pub struct Attribute<'c> {
    cipher: &'c Cipher,
    table: &'c str,
    column: &'c str,
}

impl Cipher {
    /// The encrypted column `table.column`.
    #[must_use]
    pub fn attribute<'c>(&'c self, table: &'c str, column: &'c str) -> Attribute<'c> {
        Attribute {
            cipher: self,
            table,
            column,
        }
    }
}

impl Attribute<'_> {
    /// The table name.
    #[must_use]
    pub fn table(&self) -> &str {
        self.table
    }

    /// The column name.
    #[must_use]
    pub fn column(&self) -> &str {
        self.column
    }

    /// Encrypts `value` for row `record_id` with the active key.
    pub fn seal<T: Plaintext>(
        &self,
        record_id: &str,
        value: &T,
    ) -> Result<String, EncryptionError> {
        let plaintext = value.to_plaintext()?;
        self.cipher
            .encrypt(&plaintext, &Aad::new(self.table, self.column, record_id))
    }

    /// Decrypts `text` stored in row `record_id`. Text copied from another
    /// row, column or table fails with [`EncryptionError::Decrypt`].
    pub fn open<T: Plaintext>(&self, record_id: &str, text: &str) -> Result<T, EncryptionError> {
        let plaintext = self
            .cipher
            .decrypt(text, &Aad::new(self.table, self.column, record_id))?;
        T::from_plaintext(&plaintext)
    }

    /// [`seal`](Self::seal) for a nullable column: `None` stays NULL.
    pub fn seal_optional<T: Plaintext>(
        &self,
        record_id: &str,
        value: Option<&T>,
    ) -> Result<Option<String>, EncryptionError> {
        value.map(|value| self.seal(record_id, value)).transpose()
    }

    /// [`open`](Self::open) for a nullable column: NULL reads as `None`.
    pub fn open_optional<T: Plaintext>(
        &self,
        record_id: &str,
        text: Option<&str>,
    ) -> Result<Option<T>, EncryptionError> {
        text.map(|text| self.open(record_id, text)).transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Key, KeyRing, generate_key};

    #[derive(Debug, PartialEq, serde::Deserialize, serde::Serialize)]
    struct Address {
        street: String,
        number: u32,
    }

    #[test]
    fn a_value_json_cannot_hold_is_refused_without_echoing_it() {
        // JSON object keys must be strings: a map keyed by bytes cannot be encoded.
        let value = Json(std::collections::BTreeMap::from([(
            vec![1_u8, 2],
            "sk_live_secret".to_owned(),
        )]));
        let error = value.to_plaintext().expect_err("not JSON");
        assert!(matches!(error, EncryptionError::Encoding(_)), "{error}");
        assert!(!error.to_string().contains("sk_live"), "{error}");
    }

    #[test]
    fn json_plaintext_matches_serde_json_across_buffer_growth() {
        let long = Json(vec!["é\"x".repeat(300); 40]);
        let plaintext = long.to_plaintext().expect("encode");
        assert_eq!(
            plaintext.as_slice(),
            serde_json::to_vec(&long.0).expect("json")
        );
        assert!(plaintext.capacity() >= plaintext.len());
        assert_eq!(
            Json::<Vec<String>>::from_plaintext(&plaintext).expect("decode"),
            long
        );
        assert_eq!(
            String::from_plaintext(&[0xff, 0xfe]),
            Err(EncryptionError::Encoding(
                "the decrypted value is not UTF-8".to_owned()
            ))
        );
    }

    #[test]
    fn typed_values_round_trip_per_row() {
        let ring = KeyRing::new(
            Key::new("k1", generate_key().expect("random")).expect("key"),
            [],
        )
        .expect("ring");
        let cipher = Cipher::new(&ring).expect("cipher");
        let column = cipher.attribute("people", "address");
        let address = Json(Address {
            street: "Rua A".to_owned(),
            number: 7,
        });
        let sealed = column.seal("row-1", &address).expect("seal");
        assert_eq!(
            column
                .open::<Json<Address>>("row-1", &sealed)
                .expect("open"),
            address
        );
        assert_eq!(
            column.open::<Json<Address>>("row-2", &sealed),
            Err(EncryptionError::Decrypt)
        );
        let wrong_type = column
            .open::<Json<u32>>("row-1", &sealed)
            .expect_err("type");
        assert!(!wrong_type.to_string().contains("Rua A"));
        let bytes = cipher.attribute("people", "photo");
        let sealed = bytes.seal("row-1", &vec![0_u8, 159, 255]).expect("seal");
        assert_eq!(
            bytes.open::<Vec<u8>>("row-1", &sealed).expect("open"),
            [0, 159, 255]
        );
        assert!(bytes.open::<String>("row-1", &sealed).is_err());
        assert_eq!(
            column.seal_optional::<String>("row-1", None).expect("null"),
            None
        );
        assert_eq!(
            column.open_optional::<String>("row-1", None).expect("null"),
            None
        );
        let text = column
            .seal_optional("row-1", Some(&"x".to_owned()))
            .expect("seal");
        assert_eq!(
            column
                .open_optional::<String>("row-1", text.as_deref())
                .expect("open"),
            Some("x".to_owned())
        );
    }
}
