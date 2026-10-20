//! Cross-implementation compatibility with Forge-Go.
//!
//! `fixtures/production.json.enc` and `fixtures/go-interop.json` were written
//! by Forge-Go's own `credentials` and `encryption` packages under fixed,
//! test-only keys (never use them for anything else). These tests prove that
//! Rust reads what Go writes, derives the same key ids and blind indexes, and
//! writes the same envelope layout.
//!
//! The reverse direction: `FORGE_INTEROP_DIR=<dir> cargo test -p
//! forge-credentials --test go_interop -- --ignored` writes Rust-produced
//! values to `<dir>` for a Go program that opens them with Forge-Go
//! (see docs/credentials.md, "Compatibility with Forge-Go").

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::PathBuf;

use forge_credentials::{CredentialsError, Options, parse_keys, seal, unseal};
use forge_encryption::{Aad, Cipher, EncryptionError, Json, KeyRing};
use serde::Deserialize;

#[derive(Deserialize)]
struct Fixture {
    environment: String,
    document: String,
    master_key_text: String,
    key_ids: Vec<String>,
    ring_active: String,
    ring_keys: BTreeMap<String, String>,
    table: String,
    column: String,
    record_id: String,
    plaintext: String,
    sealed_active: String,
    sealed_previous: String,
    address_column: String,
    sealed_address_json: String,
    blind_index: String,
    blind_indexes: Vec<String>,
}

#[derive(Debug, Deserialize, PartialEq, serde::Serialize)]
struct Address {
    street: String,
    number: u32,
}

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn fixture() -> Fixture {
    serde_json::from_slice(&std::fs::read(fixtures().join("go-interop.json")).unwrap()).unwrap()
}

fn go_file() -> Vec<u8> {
    std::fs::read(fixtures().join("production.json.enc")).unwrap()
}

#[test]
fn rust_derives_the_key_ids_go_records() {
    let fixture = fixture();
    let keys = parse_keys(&fixture.master_key_text).unwrap();
    let ids: Vec<&str> = keys.iter().map(|key| key.id()).collect();
    assert_eq!(ids, fixture.key_ids);
    assert_eq!(
        forge_credentials::key_id(&go_file()).unwrap(),
        fixture.key_ids[1]
    );
}

#[test]
fn rust_opens_a_credentials_file_written_by_go() {
    let fixture = fixture();
    let keys = parse_keys(&fixture.master_key_text).unwrap();
    let plaintext = unseal(&fixture.environment, &go_file(), &keys).unwrap();
    assert_eq!(&*plaintext, fixture.document.as_bytes());

    // The environment is authenticated: the same file is refused as staging's.
    assert!(matches!(
        unseal("staging", &go_file(), &keys),
        Err(CredentialsError::Decrypt)
    ));
    // Only the old key: no key matches the file's key id.
    assert!(matches!(
        unseal(&fixture.environment, &go_file(), &keys[..1]),
        Err(CredentialsError::NoMatchingKey(id)) if id == fixture.key_ids[1]
    ));

    let master = fixture.master_key_text.clone();
    let store = forge_credentials::open(
        "production",
        Options::default()
            .with_dir(fixtures())
            .with_lookup(move |name| (name == "FORGE_MASTER_KEY").then(|| master.clone()))
            .with_required(["stripe.api_key", "encryption"]),
    )
    .unwrap();
    assert_eq!(
        store.secret(&["stripe", "api_key"]).unwrap().expose(),
        "sk_test_interop"
    );
    assert_eq!(store.string(&["unicode"]).unwrap(), "ação ✓");
    let ring = store.key_ring(&["encryption"]).unwrap();
    assert_eq!(ring.ids(), ["2026-10", "2025-03"]);
}

#[test]
fn rust_writes_the_envelope_layout_go_writes() {
    let fixture = fixture();
    let keys = parse_keys(&fixture.master_key_text).unwrap();
    let ours = seal(&fixture.environment, fixture.document.as_bytes(), &keys[1]).unwrap();
    let mask = |file: &[u8]| {
        let mut value: serde_json::Value = serde_json::from_slice(file).unwrap();
        let nonce = value["nonce"].as_str().unwrap().len();
        let ciphertext = value["ciphertext"].as_str().unwrap().len();
        value["nonce"] = "N".repeat(nonce).into();
        value["ciphertext"] = "C".repeat(ciphertext).into();
        let mut text = serde_json::to_vec_pretty(&value).unwrap();
        text.push(b'\n');
        (text, String::from_utf8(file.to_vec()).unwrap())
    };
    let (go_masked, go_text) = mask(&go_file());
    let (rust_masked, rust_text) = mask(&ours);
    assert_eq!(rust_masked, go_masked);
    // Same field order, indentation and final newline as Go's MarshalIndent.
    let shape = |text: &str| -> Vec<String> {
        text.lines()
            .map(|line| line.split(':').next().unwrap_or_default().to_owned())
            .collect()
    };
    assert_eq!(shape(&rust_text), shape(&go_text));
    assert!(rust_text.ends_with("}\n"));
    assert_eq!(
        &*unseal(&fixture.environment, &ours, &keys).unwrap(),
        fixture.document.as_bytes()
    );
}

#[test]
fn rust_decrypts_attributes_and_matches_go_blind_indexes() {
    let fixture = fixture();
    let ring = KeyRing::parse(&fixture.ring_active, &fixture.ring_keys).unwrap();
    let cipher = Cipher::new(&ring).unwrap();
    let aad = Aad::new(&fixture.table, &fixture.column, &fixture.record_id);
    for sealed in [&fixture.sealed_active, &fixture.sealed_previous] {
        assert_eq!(
            &*cipher.decrypt(sealed, &aad).unwrap(),
            fixture.plaintext.as_bytes()
        );
    }
    assert!(!cipher.needs_rotation(&fixture.sealed_active).unwrap());
    assert!(cipher.needs_rotation(&fixture.sealed_previous).unwrap());
    // Bound to the row: Go's ciphertext does not open for another one.
    assert_eq!(
        cipher.decrypt(
            &fixture.sealed_active,
            &Aad::new(
                &fixture.table,
                &fixture.column,
                "0199f5c4-3a52-7d1e-9b7e-3f1c2a4d5e70"
            )
        ),
        Err(EncryptionError::Decrypt)
    );
    let address: Json<Address> = cipher
        .attribute(&fixture.table, &fixture.address_column)
        .open(&fixture.record_id, &fixture.sealed_address_json)
        .unwrap();
    assert_eq!(
        address.0,
        Address {
            street: "Rua A".to_owned(),
            number: 7
        }
    );
    assert_eq!(
        cipher
            .blind_index(
                &fixture.table,
                &fixture.column,
                fixture.plaintext.as_bytes()
            )
            .unwrap(),
        fixture.blind_index
    );
    assert_eq!(
        cipher
            .blind_indexes(
                &fixture.table,
                &fixture.column,
                fixture.plaintext.as_bytes()
            )
            .unwrap(),
        fixture.blind_indexes
    );
}

/// Writes Rust-produced values for Forge-Go to open (the reverse direction).
#[test]
#[ignore = "manual interop check: set FORGE_INTEROP_DIR and run the Go verifier"]
fn write_rust_values_for_go() {
    let Some(dir) = std::env::var_os("FORGE_INTEROP_DIR").map(PathBuf::from) else {
        return;
    };
    let fixture = fixture();
    let keys = parse_keys(&fixture.master_key_text).unwrap();
    let file = seal(&fixture.environment, fixture.document.as_bytes(), &keys[0]).unwrap();
    std::fs::write(dir.join("rust-production.json.enc"), file).unwrap();
    let ring = KeyRing::parse(&fixture.ring_active, &fixture.ring_keys).unwrap();
    let cipher = Cipher::new(&ring).unwrap();
    let ssn = cipher
        .attribute(&fixture.table, &fixture.column)
        .seal(&fixture.record_id, &fixture.plaintext)
        .unwrap();
    let address = cipher
        .attribute(&fixture.table, &fixture.address_column)
        .seal(
            &fixture.record_id,
            &Json(Address {
                street: "Rua A".to_owned(),
                number: 7,
            }),
        )
        .unwrap();
    let values = serde_json::json!({ "sealed_ssn": ssn, "sealed_address_json": address });
    std::fs::write(
        dir.join("rust-values.json"),
        serde_json::to_vec_pretty(&values).unwrap(),
    )
    .unwrap();
}
