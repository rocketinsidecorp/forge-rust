//! The encrypted file: a small JSON envelope around AES-256-GCM, byte for byte
//! the format of Forge-Go's `credentials` package.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use forge_encryption::{
    Aead, EncryptionError, Key, KeyMaterial, NONCE_SIZE, associated_data, derive_key, parse_key,
    random_nonce, valid_key_id,
};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::{CredentialsError, document};

/// The version of the encrypted file format. It is authenticated, so a file
/// cannot be downgraded or reinterpreted.
pub const FORMAT_VERSION: i64 = 1;

/// The largest credentials document.
pub const MAX_PLAINTEXT_BYTES: usize = 1 << 20;

/// The largest encrypted file.
pub const MAX_FILE_BYTES: usize = 2 << 20;

/// The most keys a key list may hold.
pub const MAX_KEYS: usize = 8;

const ALGORITHM: &str = "AES-256-GCM";
const PURPOSE_AEAD: &str = "credentials/aead";
const PURPOSE_KEY_ID: &str = "credentials/key-id";
const AD_DOMAIN: &str = "forge/credentials";

/// Accepts 1 to 32 lowercase ASCII letters, digits, hyphens or underscores,
/// starting with a letter. The name becomes part of a file name and of the
/// authenticated data, so it must be unambiguous.
pub fn validate_environment(environment: &str) -> Result<(), CredentialsError> {
    let bytes = environment.as_bytes();
    let valid = !bytes.is_empty()
        && bytes.len() <= 32
        && bytes[0].is_ascii_lowercase()
        && bytes.iter().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'_' || *byte == b'-'
        });
    if valid {
        Ok(())
    } else {
        Err(CredentialsError::InvalidEnvironment)
    }
}

/// The stored form. Only the nonce and ciphertext change between edits, and
/// nothing in it is secret. Field order is part of the format.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
struct Envelope {
    version: i64,
    algorithm: String,
    key_id: String,
    nonce: String,
    ciphertext: String,
}

/// Wraps 32 bytes of key material. Its identifier is a keyed fingerprint,
/// the hex of the first 8 bytes of an HKDF output, which names the key in the
/// file header without revealing it.
pub fn new_key(material: KeyMaterial) -> Result<Key, CredentialsError> {
    let id = derive_key(&material, PURPOSE_KEY_ID)?;
    let hex: String = id.expose()[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    Ok(Key::new(hex, material)?)
}

/// Parses key text: canonical unpadded base64url keys separated by commas or
/// newlines, the active key first. Blank lines and lines starting with `#`
/// are ignored. Errors never echo the input.
pub fn parse_keys(text: &str) -> Result<Vec<Key>, CredentialsError> {
    let mut keys = Vec::new();
    for field in text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .flat_map(|line| line.split(','))
        .map(str::trim)
        .filter(|field| !field.is_empty())
    {
        let material = parse_key(field).map_err(|error| {
            CredentialsError::InvalidKey(format!("credentials: key {}: {error}", keys.len() + 1))
        })?;
        keys.push(new_key(material)?);
    }
    if keys.is_empty() {
        return Err(CredentialsError::InvalidKey(
            "credentials: no key found".to_owned(),
        ));
    }
    if keys.len() > MAX_KEYS {
        return Err(CredentialsError::InvalidKey(format!(
            "credentials: at most {MAX_KEYS} keys are accepted"
        )));
    }
    Ok(keys)
}

fn additional_data(environment: &str, version: i64, key_id: &str) -> Vec<u8> {
    associated_data(&[AD_DOMAIN, &version.to_string(), environment, key_id])
}

fn aead(key: &Key) -> Result<Aead, CredentialsError> {
    let derived = derive_key(key.material(), PURPOSE_AEAD)?;
    Ok(Aead::new(&derived)?)
}

/// Encrypts a credentials document with `key` for `environment` and returns
/// the file contents. The environment name, format version and key id are
/// authenticated: the result decrypts only for the same environment, so a
/// staging file cannot be installed as production's.
pub fn seal(environment: &str, plaintext: &[u8], key: &Key) -> Result<Vec<u8>, CredentialsError> {
    validate_environment(environment)?;
    document::validate_document(plaintext)?;
    let nonce = random_nonce()?;
    let ciphertext = aead(key)?.seal(
        &nonce,
        plaintext,
        &additional_data(environment, FORMAT_VERSION, key.id()),
    )?;
    let envelope = Envelope {
        version: FORMAT_VERSION,
        algorithm: ALGORITHM.to_owned(),
        key_id: key.id().to_owned(),
        nonce: URL_SAFE_NO_PAD.encode(nonce),
        ciphertext: URL_SAFE_NO_PAD.encode(ciphertext),
    };
    let mut out = serde_json::to_vec_pretty(&envelope)
        .map_err(|_| CredentialsError::Format("cannot encode the envelope".to_owned()))?;
    out.push(b'\n');
    Ok(out)
}

/// The key id recorded in a credentials file, read without decrypting it.
pub fn key_id(file: &[u8]) -> Result<String, CredentialsError> {
    Ok(parse_envelope(file)?.key_id)
}

fn parse_envelope(file: &[u8]) -> Result<Envelope, CredentialsError> {
    if file.len() > MAX_FILE_BYTES {
        return Err(CredentialsError::Format("file is too large".to_owned()));
    }
    let envelope: Envelope = serde_json::from_slice(file)
        .map_err(|_| CredentialsError::Format("not a credentials file".to_owned()))?;
    if envelope.version != FORMAT_VERSION {
        return Err(CredentialsError::Format(format!(
            "unsupported version {} (this Forge reads version {FORMAT_VERSION})",
            envelope.version
        )));
    }
    if envelope.algorithm != ALGORITHM
        || !valid_key_id(&envelope.key_id)
        || envelope.nonce.is_empty()
        || envelope.ciphertext.is_empty()
    {
        return Err(CredentialsError::Format(
            "unsupported algorithm or missing fields".to_owned(),
        ));
    }
    Ok(envelope)
}

/// Decrypts `file` for `environment` with the key whose id the file records.
/// Wrong key, modified file, wrong environment and wrong format version are
/// all [`CredentialsError::Decrypt`].
pub fn unseal(
    environment: &str,
    file: &[u8],
    keys: &[Key],
) -> Result<Zeroizing<Vec<u8>>, CredentialsError> {
    validate_environment(environment)?;
    let header = parse_envelope(file)?;
    let Some(key) = keys.iter().find(|key| key.id() == header.key_id) else {
        return Err(CredentialsError::NoMatchingKey(header.key_id));
    };
    let nonce = URL_SAFE_NO_PAD.decode(&header.nonce);
    let ciphertext = URL_SAFE_NO_PAD.decode(&header.ciphertext);
    let (Ok(nonce), Ok(ciphertext)) = (nonce, ciphertext) else {
        return Err(CredentialsError::Format("invalid encoding".to_owned()));
    };
    if ciphertext.len() > MAX_PLAINTEXT_BYTES + 64 {
        return Err(CredentialsError::Format("invalid encoding".to_owned()));
    }
    if nonce.len() != NONCE_SIZE {
        return Err(CredentialsError::Format("invalid nonce".to_owned()));
    }
    aead(key)?
        .open(
            &nonce,
            &ciphertext,
            &additional_data(environment, header.version, &header.key_id),
        )
        .map_err(|error| match error {
            EncryptionError::Decrypt => CredentialsError::Decrypt,
            other => CredentialsError::Encryption(other),
        })
}
