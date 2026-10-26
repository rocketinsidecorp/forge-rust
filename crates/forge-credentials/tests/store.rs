//! The credentials store through its public API: a file embedded in the
//! binary, the options, the typed accessors and their errors, and the
//! redaction of everything the store prints.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use forge_credentials::{
    CredentialsError, MAX_FILE_BYTES, Options, Store, load, open, parse, seal,
};
use forge_encryption::{KeyMaterial, encode_key, generate_key};
use serde::Deserialize;

const SECRET: &str = "sk_live_DISTINCTIVE_4242";

fn document() -> Vec<u8> {
    format!(
        r#"{{
  "stripe": {{"api_key": "{SECRET}", "retries": 3, "live": true, "note": null}},
  "empty_text": "",
  "empty_object": {{}},
  "empty_list": [],
  "list": [1, 2],
  "encryption": {{"active": "k1", "keys": {{"k1": "{key}"}}}}
}}"#,
        key = *encode_key(&generate_key().unwrap())
    )
    .into_bytes()
}

/// A key and its text as FORGE_MASTER_KEY carries it.
fn key() -> (KeyMaterial, String) {
    let material = generate_key().unwrap();
    let text = encode_key(&material).to_string();
    (material, text)
}

/// Leaks a sealed file so it can be embedded like `include_bytes!` data.
fn embedded(environment: &str, material: &str) -> &'static [u8] {
    let keys = forge_credentials::parse_keys(material).unwrap();
    let file = seal(environment, &document(), &keys[0]).unwrap();
    Box::leak(file.into_boxed_slice())
}

fn lookup_of(value: Option<&str>) -> impl Fn(&str) -> Option<String> + Send + Sync + 'static {
    let value = value.map(str::to_owned);
    move |name| {
        assert_eq!(name, "FORGE_MASTER_KEY", "only the master key is looked up");
        value.clone()
    }
}

#[test]
fn an_embedded_file_opens_with_the_key_from_the_lookup() {
    let (_, text) = key();
    let file = embedded("production", &text);
    let store = open(
        "production",
        Options::default()
            .with_embedded(file)
            .with_lookup(lookup_of(Some(&text)))
            .with_required(["stripe.api_key", "stripe.retries"]),
    )
    .unwrap();
    assert!(store.exists());
    assert_eq!(store.environment(), "production");
    assert_eq!(store.string(&["stripe", "api_key"]).unwrap(), SECRET);

    // The disk is not consulted: a directory that does not exist is fine.
    let store = open(
        "production",
        Options::default()
            .with_dir("/nonexistent/forge-credentials")
            .with_embedded(file)
            .with_lookup(lookup_of(Some(&text))),
    )
    .unwrap();
    assert!(store.has(&["stripe", "api_key"]));
}

#[test]
fn an_embedded_file_needs_a_valid_key_from_the_environment_only() {
    let (_, text) = key();
    let file = embedded("production", &text);
    let with = |value: Option<&str>| {
        open(
            "production",
            Options::default()
                .with_embedded(file)
                .with_lookup(lookup_of(value)),
        )
    };
    // No key, or a blank one: key files are never embedded.
    assert!(matches!(with(None), Err(CredentialsError::KeyMissing)));
    assert!(matches!(
        with(Some(" \n ")),
        Err(CredentialsError::KeyMissing)
    ));
    // A malformed key names the variable, never the value.
    let error = with(Some("not-a-key-SECRET-MATERIAL")).unwrap_err();
    let printed = error.to_string();
    assert!(
        matches!(&error, CredentialsError::InvalidKey(message) if message.starts_with("FORGE_MASTER_KEY: ")),
        "{printed}"
    );
    assert!(!printed.contains("SECRET-MATERIAL"), "{printed}");
    // A key that does not belong to the file.
    let (_, other) = key();
    assert!(matches!(
        with(Some(&other)),
        Err(CredentialsError::NoMatchingKey(_))
    ));
    // A key of the wrong environment's file.
    let staging = embedded("staging", &text);
    assert!(matches!(
        open(
            "production",
            Options::default()
                .with_embedded(staging)
                .with_lookup(lookup_of(Some(&text)))
        ),
        Err(CredentialsError::Decrypt)
    ));
}

#[test]
fn an_embedded_file_over_the_limit_is_refused_before_it_is_read() {
    let big: &'static [u8] = Box::leak(vec![b'x'; MAX_FILE_BYTES + 1].into_boxed_slice());
    let error = open(
        "production",
        Options::default()
            .with_embedded(big)
            .with_lookup(lookup_of(None)),
    )
    .unwrap_err();
    assert!(
        matches!(&error, CredentialsError::Format(message) if message == "file is too large"),
        "{error}"
    );
}

#[test]
fn the_process_environment_is_the_default_key_source() {
    // The default lookup reads FORGE_MASTER_KEY from the process. Whatever the
    // environment holds, a file sealed under an unrelated key does not open.
    let (_, text) = key();
    let file = embedded("production", &text);
    let result = open("production", Options::default().with_embedded(file));
    assert!(
        matches!(
            result,
            Err(CredentialsError::KeyMissing
                | CredentialsError::NoMatchingKey(_)
                | CredentialsError::InvalidKey(_))
        ),
        "{result:?}"
    );
    // `load` is `open` with the default options: no file below the default
    // directory in development and test is an empty store, in production an error.
    let store = load("test").unwrap();
    assert!(!store.exists());
    assert!(matches!(
        load("production"),
        Err(CredentialsError::NotFound(_))
    ));
}

#[test]
fn options_print_what_they_hold_without_the_lookup_or_the_file() {
    let (_, text) = key();
    let file = embedded("production", &text);
    let options = Options::default()
        .with_dir("config/elsewhere")
        .with_embedded(file)
        .with_lookup(lookup_of(Some(&text)))
        .with_required(["stripe.api_key"]);
    let printed = format!("{options:?}");
    assert!(printed.contains("config/elsewhere"), "{printed}");
    assert!(
        printed.contains(&format!("Some({})", file.len())),
        "{printed}"
    );
    assert!(printed.contains("custom"), "{printed}");
    assert!(printed.contains("stripe.api_key"), "{printed}");
    assert!(!printed.contains(&text), "{printed}");
    let plain = format!("{:?}", Options::default());
    assert!(plain.contains("None"), "{plain}");
}

#[test]
fn accessors_name_the_path_and_never_the_value() {
    let store = parse("test", &document()).unwrap();
    assert_eq!(store.environment(), "test");
    assert!(store.exists());
    assert!(store.has(&["stripe", "live"]));
    assert!(!store.has(&[]));
    assert!(!store.has(&["stripe", "absent"]));
    // Walking through a scalar is a missing path, not a type error.
    assert!(matches!(
        store.string(&["stripe", "api_key", "deeper"]),
        Err(CredentialsError::Missing(path)) if path == "stripe.api_key.deeper"
    ));
    assert!(matches!(
        store.string(&[]),
        Err(CredentialsError::Missing(path)) if path.is_empty()
    ));
    for (path, found) in [
        (&["stripe", "note"][..], "null"),
        (&["stripe", "retries"], "a number"),
        (&["stripe", "live"], "a boolean"),
        (&["list"], "an array"),
        (&["empty_object"], "an object"),
    ] {
        let error = store.string(path).unwrap_err();
        assert!(
            matches!(&error, CredentialsError::Type { found: actual, wanted: "a string", .. } if *actual == found),
            "{error}"
        );
        assert!(!error.to_string().contains(SECRET));
    }
    assert_eq!(
        store.secret(&["stripe", "api_key"]).unwrap().expose(),
        SECRET
    );
    assert!(!format!("{:?}", store.secret(&["stripe", "api_key"]).unwrap()).contains(SECRET));
}

#[test]
fn decode_reads_the_document_or_a_path_and_reports_only_the_path() {
    #[derive(Debug, Deserialize)]
    struct Stripe {
        api_key: String,
        retries: u32,
    }
    #[derive(Debug, Deserialize)]
    struct Whole {
        stripe: Stripe,
        list: Vec<u8>,
    }
    let store = parse("test", &document()).unwrap();
    let stripe: Stripe = store.decode(&["stripe"]).unwrap();
    assert_eq!((stripe.api_key.as_str(), stripe.retries), (SECRET, 3));
    let whole: Whole = store.decode(&[]).unwrap();
    assert_eq!((whole.stripe.retries, whole.list), (3, vec![1, 2]));

    // The wrong shape names the path; serde's message, which can quote values, is dropped.
    let error = store.decode::<Stripe>(&["list"]).unwrap_err();
    assert!(matches!(&error, CredentialsError::Decode(path) if path == "list"));
    let error = store.decode::<Stripe>(&[]).unwrap_err();
    assert!(matches!(&error, CredentialsError::Decode(path) if path.is_empty()));
    assert!(matches!(
        store.decode::<Stripe>(&["absent"]),
        Err(CredentialsError::Missing(_))
    ));
}

#[test]
fn require_reports_every_empty_or_missing_path_at_once_sorted() {
    let store = parse("production", &document()).unwrap();
    // Numbers, booleans and non-empty strings, lists and objects count as set.
    store
        .require(&[
            "stripe.api_key",
            "stripe.retries",
            "stripe.live",
            "list",
            "stripe",
        ])
        .unwrap();
    let error = store
        .require(&[
            "stripe.note",
            "empty_text",
            "empty_object",
            "empty_list",
            "absent.path",
            "stripe.api_key",
        ])
        .unwrap_err();
    match &error {
        CredentialsError::MissingRequired { environment, paths } => {
            assert_eq!(environment, "production");
            assert_eq!(
                paths,
                &[
                    "absent.path",
                    "empty_list",
                    "empty_object",
                    "empty_text",
                    "stripe.note"
                ]
            );
        }
        other => panic!("{other:?}"),
    }
    assert!(!error.to_string().contains(SECRET));
}

#[test]
fn a_store_prints_and_serializes_only_its_environment() {
    let store: Store = parse("production", &document()).unwrap();
    let expected = "credentials.Store(production, [REDACTED])";
    assert_eq!(format!("{store:?}"), expected);
    assert_eq!(store.to_string(), expected);
    assert_eq!(
        serde_json::to_string(&store).unwrap(),
        format!("\"{expected}\"")
    );
    let clone = store.clone();
    assert_eq!(clone.to_string(), expected);
}

#[test]
fn key_rings_come_from_the_document_and_reject_a_bad_ring() {
    let store = parse("test", &document()).unwrap();
    assert_eq!(store.key_ring(&["encryption"]).unwrap().active_id(), "k1");
    // A path that is not a ring at all.
    assert!(store.key_ring(&["stripe"]).is_err());
    assert!(store.key_ring(&["absent"]).is_err());
}
