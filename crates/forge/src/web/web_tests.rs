//! What `web` logs and answers when a handler or a response fails.

use std::collections::HashMap;

use super::*;
use crate::test_support::warnings;

#[test]
fn a_value_that_cannot_be_serialized_is_an_empty_500() {
    // JSON object keys must be strings.
    let value: HashMap<(u8, u8), u8> = HashMap::from([((1, 2), 3)]);
    let log = warnings(|| {
        let response = json(StatusCode::OK, &value);
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(response.body().is_empty());
    });
    assert!(log.contains("response serialization failed"), "{log}");
}

#[test]
fn server_side_failures_are_logged_with_their_causes_and_hidden_from_the_client() {
    let dependency = ForgeError::new(
        ErrorCategory::Infrastructure,
        "payments_unavailable",
        "the payment provider is unavailable",
    )
    .with_source(std::io::Error::other("connection reset by peer"));
    let bug = ForgeError::new(ErrorCategory::Internal, "invariant", "invariant broken")
        .with_source(std::io::Error::other("index out of range"));

    let log = warnings(|| {
        let response = error(&dependency);
        assert!(response.status().is_server_error());
        assert!(!String::from_utf8_lossy(response.body()).contains("connection reset"));
        let response = error(&bug);
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(!String::from_utf8_lossy(response.body()).contains("index out of range"));
        // A client error is not an incident: nothing is logged for it.
        let missing = ForgeError::new(ErrorCategory::NotFound, "gone", "not found");
        assert_eq!(error(&missing).status(), StatusCode::NOT_FOUND);
    });

    assert!(log.contains("dependency failure"), "{log}");
    assert!(log.contains("payments_unavailable"), "{log}");
    assert!(log.contains("connection reset by peer"), "{log}");
    assert!(log.contains("request failed"), "{log}");
    assert!(log.contains("index out of range"), "{log}");
    assert!(!log.contains("gone"), "{log}");
}

#[test]
fn a_reported_failure_keeps_the_response_and_logs_by_category() {
    let dependency = ForgeError::new(ErrorCategory::Infrastructure, "mail_down", "mail is down")
        .with_source(std::io::Error::other("smtp timeout"));
    let other = ForgeError::new(ErrorCategory::Conflict, "audit_conflict", "audit conflict");

    let log = warnings(|| {
        report(&dependency);
        report(&other);
    });

    assert!(
        log.contains("WARN") && log.contains("dependency failure"),
        "{log}"
    );
    assert!(
        log.contains("mail_down") && log.contains("smtp timeout"),
        "{log}"
    );
    assert!(
        log.contains("ERROR") && log.contains("operation failed"),
        "{log}"
    );
    assert!(log.contains("audit_conflict"), "{log}");
}
