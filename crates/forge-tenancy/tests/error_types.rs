//! Value and error types of the tenancy crate through its public API.

use forge_tenancy::{
    AccountStoreError, MembershipStoreError, MembershipStoreErrorKind, MfaError, TenantSlug,
};

#[test]
fn membership_store_errors_keep_their_kind_message_and_retry_hint() {
    for (kind, retryable) in [
        (MembershipStoreErrorKind::Unavailable, true),
        (MembershipStoreErrorKind::Conflict, false),
        (MembershipStoreErrorKind::Corrupt, false),
    ] {
        let error = MembershipStoreError::new(kind, "store said no", retryable);
        assert_eq!(error.kind(), kind);
        assert_eq!(error.retryable(), retryable);
        assert_eq!(error.to_string(), "store said no");
    }
}

#[test]
fn slugs_expose_their_text_and_account_store_failures_map_to_mfa_errors() {
    let slug = TenantSlug::parse_input("  Acme-Corp ").expect("valid");
    assert_eq!(slug.as_str(), "acme-corp");
    assert_eq!(slug.to_string(), "acme-corp");
    assert!(TenantSlug::parse_input("not a slug").is_err());

    assert_eq!(
        MfaError::from(AccountStoreError::Unavailable),
        MfaError::Unavailable
    );
    assert_eq!(
        MfaError::from(AccountStoreError::Conflict),
        MfaError::Unavailable
    );
    assert_eq!(
        MfaError::from(AccountStoreError::Corrupt),
        MfaError::Mechanism
    );
}
