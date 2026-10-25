//! The credential and session value types through their public API: what
//! they accept, what they expose and that nothing secret prints.

use async_trait::async_trait;
use forge_auth::{
    CredentialError, CsrfToken, CsrfTokenDigest, PasswordCredential, PasswordCredentialStore,
    PasswordCredentialStoreError, PasswordCredentialVersion, PasswordHash, PasswordHashError,
    PasswordHasher, Session, SessionBearerToken, SessionCredentialDigest, SessionError,
    UnixTimestamp,
};
use forge_security::PrincipalId;

const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$c2FsdHNhbHQ$aGFzaGhhc2hoYXNoaGFzaA";

fn time(seconds: u64) -> UnixTimestamp {
    UnixTimestamp::from_secs(seconds)
}

#[test]
fn tokens_are_canonical_lowercase_hex_and_print_redacted() {
    let bearer = SessionBearerToken::new(TOKEN).expect("canonical");
    let csrf = CsrfToken::new(TOKEN).expect("canonical");
    assert_eq!(bearer.expose(), TOKEN);
    assert_eq!(csrf.expose(), TOKEN);
    assert_eq!(format!("{bearer:?}"), "SessionBearerToken([REDACTED])");
    assert_eq!(format!("{csrf:?}"), "CsrfToken([REDACTED])");
    for bad in [
        "",
        "abc",
        &TOKEN.to_uppercase(),
        &TOKEN[..63],
        &format!("{TOKEN}0"),
    ] {
        assert_eq!(
            SessionBearerToken::new(bad),
            Err(CredentialError::InvalidToken),
            "{bad:?}"
        );
        assert_eq!(CsrfToken::new(bad), Err(CredentialError::InvalidToken));
    }
}

#[test]
fn digests_keep_their_bytes_and_print_redacted() {
    let bytes = [7_u8; 32];
    let session = SessionCredentialDigest::from_bytes(bytes);
    let csrf = CsrfTokenDigest::from_bytes(bytes);
    assert_eq!(session.as_bytes(), &bytes);
    assert_eq!(csrf.as_bytes(), &bytes);
    assert_eq!(
        format!("{session:?}"),
        "SessionCredentialDigest([REDACTED])"
    );
    assert_eq!(format!("{csrf:?}"), "CsrfTokenDigest([REDACTED])");
}

#[test]
fn password_hashes_are_argon2id_phc_strings() {
    let hash = PasswordHash::new(HASH).expect("argon2id");
    assert_eq!(hash.expose(), HASH);
    assert_eq!(format!("{hash:?}"), "PasswordHash([REDACTED])");
    for bad in [
        "$2b$12$bcrypt",
        "plain",
        &format!("{HASH}\n"),
        &format!("$argon2id${}", "x".repeat(1100)),
    ] {
        assert_eq!(
            PasswordHash::new(bad),
            Err(CredentialError::InvalidPasswordHash)
        );
    }
}

#[test]
fn sessions_expose_their_lifetime_and_the_principal_they_authenticate() {
    let principal = PrincipalId::new();
    let mut session = Session::new(principal, time(100), time(200)).expect("valid");
    assert_eq!(session.principal_id(), principal);
    assert_eq!(session.issued_at(), time(100));
    assert_eq!(session.expires_at(), time(200));
    assert_eq!(session.expires_at().as_secs(), 200);
    assert_eq!(session.revoked_at(), None);

    let authenticated = session.authenticate(time(150)).expect("valid now");
    assert_eq!(authenticated.principal_id(), principal);
    assert_eq!(authenticated.session_id(), session.id());
    assert_eq!(authenticated.authenticated_at(), time(150));

    // Revocation keeps its earliest time and cannot predate the session.
    assert_eq!(
        session.revoke(time(99)),
        Err(SessionError::InvalidRevocationTime)
    );
    session.revoke(time(120)).expect("revoked");
    session.revoke(time(180)).expect("idempotent");
    assert_eq!(session.revoked_at(), Some(time(120)));
    assert_eq!(session.authenticate(time(150)), Err(SessionError::Revoked));

    let restored = Session::restore(
        session.id(),
        principal,
        time(100),
        time(200),
        Some(time(120)),
    )
    .expect("restores");
    assert_eq!(restored, session);
    assert_eq!(
        Session::restore(session.id(), principal, time(100), time(100), None),
        Err(SessionError::InvalidLifetime)
    );
    assert_eq!(
        Session::restore(
            session.id(),
            principal,
            time(100),
            time(200),
            Some(time(50))
        ),
        Err(SessionError::InvalidRevocationTime)
    );
}

#[test]
fn stored_credentials_carry_their_login_only_when_the_store_provided_it() {
    let principal = PrincipalId::new();
    let version = PasswordCredentialVersion::new(3).expect("positive");
    let hash = PasswordHash::new(HASH).expect("argon2id");
    let credential = PasswordCredential::new(principal, hash.clone(), true, version);
    assert_eq!(credential.login(), None);
    assert!(credential.disabled());
    assert_eq!(credential.principal_id(), principal);
    assert_eq!(credential.password_hash(), &hash);
    assert_eq!(credential.version().get(), 3);
    let named = credential.with_login("ada@example.com");
    assert_eq!(named.login(), Some("ada@example.com"));
    assert!(PasswordCredentialVersion::new(3) < PasswordCredentialVersion::new(4));
}

struct MinimalStore;

#[async_trait]
impl PasswordCredentialStore for MinimalStore {
    async fn find_by_login(
        &self,
        _: &str,
    ) -> Result<Option<PasswordCredential>, PasswordCredentialStoreError> {
        Ok(None)
    }

    async fn replace_password_hash(
        &self,
        _: PrincipalId,
        _: PasswordCredentialVersion,
        _: &PasswordHash,
    ) -> Result<(), PasswordCredentialStoreError> {
        Ok(())
    }
}

struct MinimalHasher;

#[async_trait]
impl PasswordHasher for MinimalHasher {
    async fn hash(&self, _: &[u8]) -> Result<PasswordHash, PasswordHashError> {
        Err(PasswordHashError::Unavailable)
    }

    async fn verify(&self, _: &[u8], _: &PasswordHash) -> Result<bool, PasswordHashError> {
        Ok(false)
    }
}

#[tokio::test]
async fn defaults_fail_closed_for_stores_and_stay_conservative_for_hashers() {
    // A store that does not implement step-up makes every step-up unavailable.
    assert_eq!(
        MinimalStore
            .find_by_principal(PrincipalId::new())
            .await
            .err(),
        Some(PasswordCredentialStoreError::Unavailable)
    );
    // A hasher without policy upgrades never asks for a rehash.
    let hash = PasswordHash::new(HASH).expect("argon2id");
    assert_eq!(MinimalHasher.needs_rehash(&hash), Ok(false));
    assert_eq!(
        MinimalHasher.hash(b"x").await,
        Err(PasswordHashError::Unavailable)
    );
    assert_eq!(MinimalHasher.verify(b"x", &hash).await, Ok(false));
}
