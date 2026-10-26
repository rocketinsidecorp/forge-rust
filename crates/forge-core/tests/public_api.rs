//! The public surface of `forge-core` that its in-crate tests reach only
//! indirectly: defaults, accessors, conversions and what malformed serde
//! input reports.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use forge_core::{Cancellation, ErrorCategory, ForgeError, Id, LifecycleContext, Timestamp, Uuid};
use serde::{Deserialize as _, de::IntoDeserializer as _};

enum User {}

#[tokio::test]
async fn cancellation_scopes_and_contexts_nest() {
    let root = Cancellation::default();
    let context = LifecycleContext::new(root.clone());
    let child = context.child();
    assert!(!child.cancellation().is_cancelled());
    assert!(!LifecycleContext::default().cancellation().is_cancelled());
    root.cancel();
    // The child scope observes the parent's cancellation, without waiting.
    child.cancellation().cancelled().await;
    assert!(child.cancellation().is_cancelled());
    assert!(context.cancellation().is_cancelled());
}

#[test]
fn identifiers_convert_default_and_refuse_other_versions() {
    let id = Id::<User>::default();
    let uuid: Uuid = id.into();
    assert_eq!(uuid.get_version_num(), 7);
    assert_eq!(Id::<User>::try_from(uuid).expect("v7"), id);
    assert_eq!(id.as_uuid(), &uuid);
    assert_eq!(id.into_uuid(), uuid);
    assert!(Id::<User>::try_from(Uuid::nil()).is_err());
    assert!("not a uuid".parse::<Id<User>>().is_err());
    assert_eq!(id.to_string().parse::<Id<User>>().expect("round trip"), id);
}

#[test]
fn deserializing_an_identifier_or_timestamp_from_the_wrong_type_says_what_is_expected() {
    let id = Id::<User>::deserialize(5_i64.into_deserializer());
    let error: serde::de::value::Error = id.expect_err("a number is not an identifier");
    assert!(
        error.to_string().contains("a hyphenated UUIDv7 string"),
        "{error}"
    );
    let valid = Id::<User>::new().to_string();
    let parsed: Result<Id<User>, serde::de::value::Error> =
        Id::<User>::deserialize(valid.as_str().into_deserializer());
    assert!(parsed.is_ok());

    let timestamp = Timestamp::deserialize(true.into_deserializer());
    let error: serde::de::value::Error = timestamp.expect_err("a boolean is not a timestamp");
    assert!(
        error.to_string().contains("an RFC 3339 timestamp"),
        "{error}"
    );
    let parsed = Timestamp::deserialize("2026-10-25T12:00:00Z".into_deserializer());
    let parsed: Result<Timestamp, serde::de::value::Error> = parsed;
    assert_eq!(parsed.expect("valid").to_string(), "2026-10-25T12:00:00Z");
}

#[test]
fn timestamps_report_the_clock_and_reject_malformed_text() {
    let now = Timestamp::now();
    assert!(now.unix_micros() > 1_700_000_000_000_000);
    assert!(Timestamp::now() >= now);
    // Each punctuation position is checked.
    for text in [
        "2026-10-25 12:00:00Z",
        "2026-10-25T12.00:00Z",
        "2026/10/25T12:00:00Z",
        "",
    ] {
        assert!(Timestamp::parse(text).is_err(), "{text:?}");
    }
}

#[test]
fn errors_expose_their_retry_hint_and_metadata() {
    let error = ForgeError::new(ErrorCategory::Internal, "db.down", "the database is down")
        .with_retryable(true)
        .with_metadata("attempt", 3);
    assert!(error.is_retryable());
    assert_eq!(error.metadata()["attempt"], 3);
    assert!(!ForgeError::new(ErrorCategory::Internal, "x", "y").is_retryable());
}
