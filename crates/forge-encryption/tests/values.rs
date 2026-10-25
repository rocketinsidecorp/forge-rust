//! Encrypted column values through the public API: JSON of any size, the
//! optional forms, the names an attribute reports and what rotation records
//! print.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use forge_encryption::{
    Cipher, EncryptionError, Json, Key, KeyRing, Plaintext, Record, Update, generate_key,
};
use serde::{Deserialize, Serialize};

fn cipher() -> Cipher {
    let key = Key::new("k1", generate_key().unwrap()).unwrap();
    Cipher::new(&KeyRing::new(key, []).unwrap()).unwrap()
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct Profile {
    name: String,
    tags: Vec<String>,
}

#[test]
fn json_values_of_any_size_round_trip_and_wrong_shapes_report_the_kind_only() {
    let cipher = cipher();
    let attribute = cipher.attribute("users", "profile");
    assert_eq!(
        (attribute.table(), attribute.column()),
        ("users", "profile")
    );

    // Larger than the first buffer the encoder grows from.
    let profile = Profile {
        name: "Ada Lovelace".to_owned(),
        tags: (0..400).map(|i| format!("tag-{i}")).collect(),
    };
    let sealed = attribute.seal("1", &Json(profile.clone())).unwrap();
    assert!(!sealed.contains("Ada"));
    let opened: Json<Profile> = attribute.open("1", &sealed).unwrap();
    assert_eq!(opened.0, profile);

    // The same text under another row does not open.
    assert!(matches!(
        attribute.open::<Json<Profile>>("2", &sealed),
        Err(EncryptionError::Decrypt)
    ));
    // A value of another shape reports the kind, never the content.
    let sealed = attribute
        .seal("1", &Json(vec!["secret-content".to_owned()]))
        .unwrap();
    let error = attribute.open::<Json<Profile>>("1", &sealed).unwrap_err();
    let text = format!("{error} {error:?}");
    assert!(matches!(error, EncryptionError::Encoding(_)), "{text}");
    assert!(!text.contains("secret-content"), "{text}");
}

#[test]
fn plaintexts_have_a_byte_form_and_nullable_columns_stay_null() {
    let text = Json("héllo".to_owned());
    let bytes = text.to_plaintext().unwrap();
    assert_eq!(bytes.as_slice(), "\"héllo\"".as_bytes());
    assert_eq!(Json::<String>::from_plaintext(&bytes).unwrap(), text);
    assert!(Json::<String>::from_plaintext(b"not json").is_err());

    let cipher = cipher();
    let attribute = cipher.attribute("users", "nickname");
    assert_eq!(
        attribute.seal_optional::<Json<String>>("1", None).unwrap(),
        None
    );
    assert_eq!(
        attribute.open_optional::<Json<String>>("1", None).unwrap(),
        None
    );
    let sealed = attribute
        .seal_optional("1", Some(&Json("ada".to_owned())))
        .unwrap();
    let opened = attribute
        .open_optional::<Json<String>>("1", sealed.as_deref())
        .unwrap();
    assert_eq!(opened, Some(Json("ada".to_owned())));
}

#[test]
fn rotation_records_print_their_ids_and_never_their_ciphertext() {
    let record = Record {
        id: "42".to_owned(),
        sealed: "fg1.SECRET-CIPHERTEXT".to_owned(),
    };
    let update = Update {
        id: "42".to_owned(),
        old_sealed: "fg1.OLD-CIPHERTEXT".to_owned(),
        new_sealed: "fg1.NEW-CIPHERTEXT".to_owned(),
        index: Some("BLIND-INDEX".to_owned()),
    };
    for printed in [format!("{record:?}"), format!("{update:?}")] {
        assert!(printed.contains("42"), "{printed}");
        for secret in ["CIPHERTEXT", "BLIND-INDEX"] {
            assert!(!printed.contains(secret), "{printed}");
        }
    }
    assert_eq!(update.clone(), update);
}
