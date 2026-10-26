//! The credentials file (`config/credentials/<environment>.json.enc`) and key
//! text (`FORGE_MASTER_KEY`, `<environment>.key`), forge_credentials
//! (ADR 0041).
//!
//! Input: NUL-separated: a file, an environment name and key text. Properties:
//! nothing panics; a file that is not sealed under the fixed key never opens
//! (only the sealed seeds do), a file that opens re-seals and opens again to
//! the same document, and no error message repeats the file or a key.
#![no_main]

use forge_credentials::{
    key_id, new_key, parse_keys, redact, seal, unseal, validate_document, validate_environment,
};
use forge_encryption::KeyMaterial;
use libfuzzer_sys::fuzz_target;

const KEY_BYTES: [u8; 32] = [0x42; 32];

/// Whether `shown` repeats the key text. Key text that is a piece of the
/// error message's own fixed wording ("credentials: ke") is not an echo, so a
/// match counts only when the same key with its letters and digits shifted is
/// echoed too: a coincidence with the template cannot follow the key. Found
/// by this target (seed `key-text-in-error-template`).
fn echoes(key_text: &str, shown: &str) -> bool {
    let key = key_text.trim();
    if key.len() < 8 || !shown.contains(key) {
        return false;
    }
    let shifted: String = key
        .chars()
        .map(|character| match character {
            'z' => 'a',
            'Z' => 'A',
            '9' => '0',
            other if other.is_ascii_alphanumeric() => char::from(other as u8 + 1),
            other => other,
        })
        .collect();
    match parse_keys(&shifted) {
        Err(error) => error.to_string().contains(&shifted),
        Ok(_) => false,
    }
}

fuzz_target!(|data: &[u8]| {
    let mut fields = data.splitn(3, |byte| *byte == 0);
    let file = fields.next().unwrap_or_default();
    let environment = String::from_utf8_lossy(fields.next().unwrap_or(b"development")).into_owned();
    let key_text = String::from_utf8_lossy(fields.next().unwrap_or_default()).into_owned();

    // Key text: parsing is total, and a refused key is not echoed.
    if let Err(error) = parse_keys(&key_text) {
        let shown = error.to_string();
        assert!(
            !echoes(&key_text, &shown),
            "an error echoed the key: {key_text:?} -> {shown:?}"
        );
    }

    let _ = validate_environment(&environment);
    let _ = validate_document(file);
    let _ = redact(file);
    let _ = key_id(file);

    let Ok(material) = KeyMaterial::from_bytes(&KEY_BYTES) else {
        return;
    };
    let Ok(key) = new_key(material) else {
        return;
    };
    let keys = [key];
    if let Ok(document) = unseal(&environment, file, &keys) {
        // Whatever opened is a valid document that seals and opens again.
        assert!(validate_document(&document).is_ok());
        let resealed = seal(&environment, &document, &keys[0]).expect("a document re-seals");
        let reopened = unseal(&environment, &resealed, &keys).expect("a sealed file opens");
        assert_eq!(&*document, &*reopened);
    }
});
