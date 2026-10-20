//! `uuid_v7`, `i18n_lookup`, `attribute_encryption` and `rate_limit_decision`.

use std::hint::black_box;
use std::time::Duration;

use forge::core::Id;
use forge::encryption::{Aad, Cipher, Key, KeyMaterial, KeyRing};
use forge::i18n::{Args, Options as I18nOptions, Translator};
use forge::ratelimit::{Limit, Limiter as _, MAX_N, MemoryLimiter, MemoryOptions};
use forge_bench::Case;
use forge_bench::exec::block_on;

use crate::support::must;

pub fn cases(cases: &mut Vec<Case>) {
    uuid_v7(cases);
    i18n_lookup(cases);
    attribute_encryption(cases);
    rate_limit_decision(cases);
}

// uuid_v7 --------------------------------------------------------------------

enum Marker {}

/// Generating an identifier (`forge::core::Id`, a UUIDv7 from the clock and
/// the OS random source) and formatting it in its canonical text form.
fn uuid_v7(cases: &mut Vec<Case>) {
    cases.push(Case::new("uuid_v7", "new", 2_000_000, || {
        Box::new(|iterations| {
            for _ in 0..iterations {
                black_box(Id::<Marker>::new());
            }
        })
    }));
    cases.push(Case::new("uuid_v7", "string", 2_000_000, || {
        let id = Id::<Marker>::new();
        assert_eq!(id.to_string().len(), 36);
        Box::new(move |iterations| {
            for _ in 0..iterations {
                black_box(black_box(&id).to_string());
            }
        })
    }));
}

// i18n_lookup ----------------------------------------------------------------

const EN: &str = r#"{"en": {
  "users": {"welcome": "Welcome back"},
  "greeting": "Hello, %{name}!",
  "inbox": {"zero": "No messages", "one": "%{count} message", "other": "%{count} messages"}
}}"#;
const PT: &str = r#"{"pt": {"only_pt": "Só em português", "greeting": "Olá, %{name}!"}}"#;
const PT_BR: &str = r#"{"pt-BR": {"greeting": "Oi, %{name}!"}}"#;

/// `Translator::translate` on a loaded catalog: a plain key; one `%{name}`
/// argument; the `one` and `other` plural forms; a key that `pt-BR` lacks,
/// answered by its parent locale `pt`.
fn i18n_lookup(cases: &mut Vec<Case>) {
    type Fixture = (
        &'static str,
        &'static str,
        &'static str,
        Option<(&'static str, i64)>,
        &'static str,
    );
    // (variant, locale, key, plural count or name argument, expected text)
    let fixtures: [Fixture; 5] = [
        ("simple", "en", "users.welcome", None, "Welcome back"),
        (
            "interpolation",
            "en",
            "greeting",
            Some(("name", 0)),
            "Hello, Ada!",
        ),
        ("plural_one", "en", "inbox", Some(("count", 1)), "1 message"),
        (
            "plural_other",
            "en",
            "inbox",
            Some(("count", 42)),
            "42 messages",
        ),
        ("fallback", "pt-BR", "only_pt", None, "Só em português"),
    ];
    for (variant, locale, key, argument, expected) in fixtures {
        cases.push(Case::new("i18n_lookup", variant, 500_000, move || {
            let translator = must(
                Translator::from_files(
                    &[("en.json", EN), ("pt.json", PT), ("pt-BR.json", PT_BR)],
                    I18nOptions::new(),
                ),
                "catalog",
            );
            let args = move || match argument {
                Some(("name", _)) => Args::new().with("name", "Ada"),
                Some((_, count)) => Args::new().count(count),
                None => Args::new(),
            };
            assert_eq!(translator.translate(locale, key, &args()), expected);
            Box::new(move |iterations| {
                for _ in 0..iterations {
                    black_box(translator.translate(black_box(locale), black_box(key), &args()));
                }
            })
        }));
    }
}

// attribute_encryption -------------------------------------------------------

/// Sealing and opening one 64-byte attribute (AES-256-GCM bound to its table,
/// column and record) with `Cipher`, and through the `Attribute` helper a
/// model's column uses (`seal::<String>` / `open::<String>`).
fn attribute_encryption(cases: &mut Vec<Case>) {
    const PLAIN: &str = "123-45-6789 is a sixty-four byte attribute, padded to that size.";
    const RECORD: &str = "0198c4a2-7c00-7000-8000-000000000001";

    fn cipher() -> Cipher {
        let key = must(KeyMaterial::from_bytes(&[7_u8; 32]), "key");
        let ring = must(
            KeyRing::new(must(Key::new("k1", key), "key id"), []),
            "ring",
        );
        must(Cipher::new(&ring), "cipher")
    }

    cases.push(Case::new("attribute_encryption", "seal", 500_000, || {
        assert_eq!(PLAIN.len(), 64);
        let cipher = cipher();
        let aad = Aad::new("users", "ssn", RECORD);
        Box::new(move |iterations| {
            for _ in 0..iterations {
                black_box(cipher.encrypt(black_box(PLAIN.as_bytes()), &aad).is_ok());
            }
        })
    }));
    cases.push(Case::new("attribute_encryption", "open", 500_000, || {
        let cipher = cipher();
        let aad = Aad::new("users", "ssn", RECORD);
        let sealed = must(cipher.encrypt(PLAIN.as_bytes(), &aad), "seal");
        assert_eq!(
            must(cipher.decrypt(&sealed, &aad), "open").as_slice(),
            PLAIN.as_bytes()
        );
        Box::new(move |iterations| {
            for _ in 0..iterations {
                black_box(cipher.decrypt(black_box(&sealed), &aad).is_ok());
            }
        })
    }));
    cases.push(Case::new(
        "attribute_encryption",
        "value_seal",
        500_000,
        || {
            let cipher = cipher();
            let value = PLAIN.to_owned();
            Box::new(move |iterations| {
                let column = cipher.attribute("users", "ssn");
                for _ in 0..iterations {
                    black_box(column.seal(black_box(RECORD), black_box(&value)).is_ok());
                }
            })
        },
    ));
    cases.push(Case::new(
        "attribute_encryption",
        "value_open",
        500_000,
        || {
            let cipher = cipher();
            let sealed = must(
                cipher
                    .attribute("users", "ssn")
                    .seal(RECORD, &PLAIN.to_owned()),
                "seal",
            );
            assert_eq!(
                must(
                    cipher
                        .attribute("users", "ssn")
                        .open::<String>(RECORD, &sealed),
                    "open"
                ),
                PLAIN
            );
            Box::new(move |iterations| {
                let column = cipher.attribute("users", "ssn");
                for _ in 0..iterations {
                    black_box(
                        column
                            .open::<String>(black_box(RECORD), black_box(&sealed))
                            .is_ok(),
                    );
                }
            })
        },
    ));
}

// rate_limit_decision --------------------------------------------------------

/// One `Limiter::allow` of the in-process `MemoryLimiter` (the database
/// limiters cost one round trip, which this would only measure as the
/// driver's). The call goes through the `async_trait` contract, so each call
/// boxes its future: one allocation Forge-Go's `Memory.Allow` does not make.
///
/// - `allowed`: one key far under its limit;
/// - `denied`: one key over its limit;
/// - `distinct_keys`: 4096 keys in turn against a table of 1000, so every
///   call inserts a bucket and evicts another (the worst case).
fn rate_limit_decision(cases: &mut Vec<Case>) {
    let key = forge::ratelimit::key("bench", &["203.0.113.7"]);
    let allowed_key = key.clone();
    cases.push(Case::new(
        "rate_limit_decision",
        "allowed",
        2_000_000,
        move || {
            let limiter = must(MemoryLimiter::new(MemoryOptions::default()), "limiter");
            let limit = must(Limit::new(MAX_N, Duration::from_secs(3_600)), "limit");
            assert!(must(block_on(limiter.allow(&allowed_key, limit)), "allow").allowed);
            Box::new(move |iterations| {
                for _ in 0..iterations {
                    black_box(block_on(limiter.allow(black_box(&allowed_key), limit)).is_ok());
                }
            })
        },
    ));
    cases.push(Case::new(
        "rate_limit_decision",
        "denied",
        2_000_000,
        move || {
            let limiter = must(MemoryLimiter::new(MemoryOptions::default()), "limiter");
            let limit = must(Limit::new(1, Duration::from_secs(3_600)), "limit");
            let _ = block_on(limiter.allow(&key, limit));
            assert!(!must(block_on(limiter.allow(&key, limit)), "allow").allowed);
            Box::new(move |iterations| {
                for _ in 0..iterations {
                    black_box(block_on(limiter.allow(black_box(&key), limit)).is_ok());
                }
            })
        },
    ));
    cases.push(Case::new(
        "rate_limit_decision",
        "distinct_keys",
        500_000,
        || {
            let limiter = must(
                MemoryLimiter::new(MemoryOptions::default().with_max_keys(1_000)),
                "limiter",
            );
            let limit = must(Limit::new(10, Duration::from_secs(3_600)), "limit");
            let keys: Vec<String> = (0..4_096)
                .map(|index| forge::ratelimit::key("bench", &[&index.to_string()]))
                .collect();
            let mut next = 0_usize;
            Box::new(move |iterations| {
                for _ in 0..iterations {
                    black_box(
                        block_on(limiter.allow(black_box(&keys[next % keys.len()]), limit)).is_ok(),
                    );
                    next += 1;
                }
            })
        },
    ));
}
