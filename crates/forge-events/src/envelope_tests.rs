//! Envelopes, error messages and the failure conversions that need no
//! database.

use std::error::Error as _;

use serde::Deserialize;

use super::*;

#[derive(Debug, Serialize, Deserialize, PartialEq)]
struct Paid {
    cents: i64,
}

impl IntegrationEvent for Paid {
    const TYPE: &'static str = "billing.paid";
}

#[test]
fn an_envelope_exposes_what_the_publisher_forwards() {
    let id = EventId::new();
    let tenant = TenantId::new();
    let occurred_at = Timestamp::now();
    let envelope = EventEnvelope {
        id,
        event_type: "billing.paid".to_owned(),
        version: 1,
        tenant_id: Some(tenant),
        occurred_at,
        attempt: 3,
        payload: r#"{"cents":5}"#.to_owned(),
    };

    assert_eq!(envelope.id(), id);
    assert_eq!(envelope.event_type(), "billing.paid");
    assert_eq!(envelope.version(), 1);
    assert_eq!(envelope.tenant_id(), Some(tenant));
    assert_eq!(envelope.occurred_at(), occurred_at);
    assert_eq!(envelope.attempt(), 3);
    assert_eq!(envelope.payload_json(), r#"{"cents":5}"#);
    assert!(envelope.is::<Paid>());
    assert_eq!(
        envelope.decode::<Paid>().expect("decodes"),
        Paid { cents: 5 }
    );
    assert_eq!(envelope.clone(), envelope);
    let debug = format!("{envelope:?}");
    assert!(
        debug.contains("attempt: 3") && debug.contains(&tenant.to_string()),
        "{debug}"
    );
    assert!(
        !debug.contains("cents"),
        "the payload is never printed: {debug}"
    );
}

#[test]
fn every_error_says_what_failed_without_the_payload() {
    let long = "x".repeat(300);
    let invalid = validate_name("event type", &long).expect_err("too long and not a name");
    let EventsError::InvalidName { kind, name } = &invalid else {
        panic!("{invalid:?}");
    };
    assert_eq!(*kind, "event type");
    assert_eq!(name.len(), MAX_NAME, "the rejected name is truncated");
    assert!(
        invalid
            .to_string()
            .starts_with("invalid event type name \"xxx")
    );

    for (error, message) in [
        (EventsError::InvalidVersion, "event versions start at 1"),
        (
            EventsError::InvalidSetting("batch"),
            "invalid events setting: batch",
        ),
        (
            EventsError::PayloadTooLarge { limit: 7 },
            "the event payload exceeds 7 bytes",
        ),
        (
            EventsError::TypeMismatch,
            "the event has another type or version",
        ),
        (
            EventsError::Decode,
            "the event payload could not be decoded",
        ),
        (EventsError::Corrupt, "persisted event data is invalid"),
    ] {
        assert_eq!(error.to_string(), message);
    }
    let serialize = serde_json::from_str::<Paid>("not json").expect_err("not JSON");
    let error = EventsError::Serialize(serialize);
    assert_eq!(
        error.to_string(),
        "the event payload could not be serialized"
    );
    assert!(error.source().is_some());
}

#[test]
fn a_database_error_keeps_its_cause_but_not_its_message() {
    let error = EventsError::from(forge_db::sqlx::Error::RowNotFound);
    assert!(matches!(error, EventsError::Database(_)), "{error:?}");
    assert_eq!(error.to_string(), "event store operation failed");
    assert!(error.source().is_some());
}

#[derive(Debug, Error)]
#[error("broker rejected the event")]
struct Outer(#[source] Middle);

#[derive(Debug, Error)]
#[error("connection reset")]
struct Middle(#[source] std::io::Error);

#[test]
fn a_converted_error_carries_its_whole_cause_chain_and_is_retryable() {
    let error = PublishError::from(Outer(Middle(std::io::Error::other("peer closed"))));

    assert!(!error.is_permanent());
    assert_eq!(
        error.message(),
        "broker rejected the event: connection reset: peer closed"
    );
    assert_eq!(error.to_string(), error.message());
    assert_eq!(
        format!("{:?}", PublishError::permanent("no")),
        "PublishError { message: \"no\", permanent: true }"
    );
    assert!(
        PublishError::retry(format_args!("attempt {}", 2))
            .message()
            .ends_with('2')
    );
}
