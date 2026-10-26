//! Authentication and server-side session contracts.
//!
//! This crate models session lifecycle and authenticated identity. It does not
//! implement password hashing, cookie signing, or session bearer-token
//! cryptography; those are adapter mechanisms that require separate
//! dependency/security review. A session proves identity only. Tenant roles
//! are resolved separately so authorization changes do not remain stale inside
//! credentials.
//!
//! [`PasswordWorkLimiter`] bounds how many password hashes run at once in the
//! process, so a burst of sign-in attempts cannot reserve the memory of
//! hundreds of memory-hard hashes; the hashing adapter runs its work through
//! it.
//!
//! It also owns the framework rules of the sign-in and password-reset flows
//! (ADR 0026): [`LoginThrottle`] with the bounded, per-process
//! [`MemoryLoginThrottle`] and, with the `postgres` feature, the
//! `PgLoginThrottle` every replica shares; the [`NewPassword`] policy, and
//! one-time [`PasswordResetToken`]s (256
//! bits from the OS CSPRNG through `getrandom`, persisted only as SHA-256
//! digests through `sha2`) delivered by a [`PasswordResetNotifier`].
//!
//! A session also ends after [`SessionIdleTimeout`] without use (two hours by
//! default, `FORGE_AUTH_SESSION_IDLE_TIMEOUT`); session stores record each
//! use at most once per [`SESSION_TOUCH_INTERVAL`].
//!
//! API clients present the same opaque tokens as bearer credentials:
//! [`bearer_credential`] reads the `Authorization: Bearer` header,
//! [`SessionBearerToken::generate`] issues a token from the OS CSPRNG and
//! [`SessionBearerToken::digest`] is the SHA-256 digest servers store.
//!
//! The TOTP multi-factor primitives are in [`mfa`]: RFC 6238 codes over
//! HMAC-SHA-1 (`hmac`, `sha1`), AES-256-GCM seed encryption (`ring`),
//! one-time backup codes and the sign-in challenge token.

mod bearer;
mod login;
pub mod mfa;
mod password_policy;
mod password_reset;
mod password_work;
#[cfg(feature = "postgres")]
mod postgres;
mod session_idle;
mod throttle;

use std::num::NonZeroU64;

use async_trait::async_trait;
use forge_core::Id;
use forge_security::PrincipalId;
use thiserror::Error;

pub use bearer::{BEARER_CHALLENGE, BEARER_SCHEME, BearerCredentialError, bearer_credential};
pub use login::{CanonicalLogin, LoginFormatError, MAX_LOGIN_BYTES};
pub use mfa::{
    Aes256GcmSecretCipher, MFA_BACKUP_CODE_COUNT, MFA_KEY_BYTES, MfaBackupCode,
    MfaBackupCodeDigest, MfaChallengeBinding, MfaChallengeDigest, MfaChallengeToken,
    MfaPrimitiveError, SealedTotpSecret, SecretCipher, SecretCipherError, TOTP_DIGITS,
    TOTP_PERIOD_SECS, TOTP_SECRET_BYTES, TotpProvisioningUri, TotpSecret, TotpSecretDigest,
    parse_mfa_key,
};
pub use password_policy::{
    MAX_PASSWORD_BYTES, MIN_PASSWORD_CHARS, NewPassword, PasswordPolicyError,
};
pub use password_reset::{
    PASSWORD_RESET_LIFETIME_SECS, PASSWORD_RESET_PATH, PasswordResetDelivery,
    PasswordResetDeliveryError, PasswordResetNotifier, PasswordResetToken,
    PasswordResetTokenDigest, UndeliverablePasswordResets, password_reset_throttle_config,
};
pub use password_work::{
    DEFAULT_PASSWORD_WORK_WAIT, MAX_DEFAULT_PASSWORD_WORK, PasswordWorkLimiter,
};
#[cfg(feature = "postgres")]
pub use postgres::{INLINE_PRUNE_BATCH, PgLoginThrottle};
pub use session_idle::{SESSION_TOUCH_INTERVAL, SessionIdleTimeout, SessionIdleTimeoutError};
pub use throttle::{
    LoginThrottle, MemoryLoginThrottle, ThrottleConfig, ThrottleConfigError, ThrottleDecision,
    ThrottleError, ThrottleKey,
};

/// Marker type for a session identity.
#[derive(Debug)]
pub enum SessionMarker {}

/// UUIDv7 identifier for a server-side session record.
pub type SessionId = Id<SessionMarker>;

/// Validated PHC-encoded Argon2id password hash.
///
/// Debug output is always redacted. Concrete hashing/verification is delegated
/// to a reviewed infrastructure adapter.
#[derive(Clone, Eq, PartialEq)]
pub struct PasswordHash(String);

impl PasswordHash {
    /// Accepts only bounded Argon2id PHC strings.
    pub fn new(value: impl Into<String>) -> Result<Self, CredentialError> {
        let value = value.into();
        if !value.starts_with("$argon2id$")
            || value.len() > 1024
            || value.bytes().any(|byte| byte.is_ascii_control())
        {
            return Err(CredentialError::InvalidPasswordHash);
        }
        Ok(Self(value))
    }

    /// Exposes the PHC string only to authentication/persistence adapters.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for PasswordHash {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("PasswordHash([REDACTED])")
    }
}

/// Opaque high-entropy bearer credential presented in the session cookie.
#[derive(Clone, Eq, PartialEq)]
pub struct SessionBearerToken(String);

impl SessionBearerToken {
    /// Creates a token from the canonical 64-character lowercase hex encoding
    /// of 32 random bytes.
    pub fn new(value: impl Into<String>) -> Result<Self, CredentialError> {
        validate_secret_token(value.into()).map(Self)
    }

    /// Exposes the token only at HTTP/cryptographic boundaries.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for SessionBearerToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SessionBearerToken([REDACTED])")
    }
}

/// Opaque high-entropy CSRF token bound to a server-side session.
#[derive(Clone, Eq, PartialEq)]
pub struct CsrfToken(String);

impl CsrfToken {
    /// Creates a token from the canonical 64-character lowercase hex encoding
    /// of 32 random bytes.
    pub fn new(value: impl Into<String>) -> Result<Self, CredentialError> {
        validate_secret_token(value.into()).map(Self)
    }

    /// Exposes the token only at HTTP/cryptographic boundaries.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for CsrfToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("CsrfToken([REDACTED])")
    }
}

fn validate_secret_token(value: String) -> Result<String, CredentialError> {
    if value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        Ok(value)
    } else {
        Err(CredentialError::InvalidToken)
    }
}

/// Authentication credential validation failure.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum CredentialError {
    /// Persisted/configured password hash was not a bounded Argon2id PHC string.
    #[error("invalid password hash")]
    InvalidPasswordHash,
    /// Session/CSRF token was not in the canonical high-entropy encoding.
    #[error("invalid authentication token")]
    InvalidToken,
    /// The operating-system CSPRNG could not provide a token.
    #[error("secure randomness unavailable")]
    RandomnessUnavailable,
}

/// Fixed-size digest of an opaque session bearer credential.
///
/// The raw bearer token is intentionally not part of Forge's persistence
/// contract. Infrastructure adapters derive this digest with a reviewed
/// cryptographic hash before calling a SessionStore.
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct SessionCredentialDigest([u8; 32]);

impl SessionCredentialDigest {
    /// Wraps a 256-bit credential digest produced by a cryptographic adapter.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Returns the digest bytes for persistence/query binding.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Compares two digests in constant time (`subtle`), for stores that
    /// match digests in process rather than by an indexed lookup.
    #[must_use]
    pub fn matches(&self, other: &Self) -> bool {
        use subtle::ConstantTimeEq as _;
        self.0.ct_eq(&other.0).into()
    }
}

impl std::fmt::Debug for SessionCredentialDigest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SessionCredentialDigest([REDACTED])")
    }
}

/// Fixed-size digest of the CSRF token associated with a session.
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct CsrfTokenDigest([u8; 32]);

impl CsrfTokenDigest {
    /// Wraps a 256-bit digest produced by a cryptographic adapter.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Returns digest bytes for persistence/query binding.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl std::fmt::Debug for CsrfTokenDigest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("CsrfTokenDigest([REDACTED])")
    }
}

/// UTC Unix timestamp in whole seconds.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct UnixTimestamp(u64);

impl UnixTimestamp {
    /// Creates a timestamp from seconds since the Unix epoch.
    #[must_use]
    pub const fn from_secs(seconds: u64) -> Self {
        Self(seconds)
    }

    /// Returns seconds since the Unix epoch.
    #[must_use]
    pub const fn as_secs(self) -> u64 {
        self.0
    }
}

/// Server-side session record.
///
/// The record deliberately contains no bearer secret. Concrete adapters store
/// only an appropriate verifier/digest for credentials presented by clients.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Session {
    id: SessionId,
    principal_id: PrincipalId,
    issued_at: UnixTimestamp,
    expires_at: UnixTimestamp,
    revoked_at: Option<UnixTimestamp>,
}

impl Session {
    /// Creates an active session with a strictly later expiry.
    pub fn new(
        principal_id: PrincipalId,
        issued_at: UnixTimestamp,
        expires_at: UnixTimestamp,
    ) -> Result<Self, SessionError> {
        if expires_at <= issued_at {
            return Err(SessionError::InvalidLifetime);
        }

        Ok(Self {
            id: SessionId::new(),
            principal_id,
            issued_at,
            expires_at,
            revoked_at: None,
        })
    }

    /// Restores a persisted server-side session while re-validating invariants.
    pub fn restore(
        id: SessionId,
        principal_id: PrincipalId,
        issued_at: UnixTimestamp,
        expires_at: UnixTimestamp,
        revoked_at: Option<UnixTimestamp>,
    ) -> Result<Self, SessionError> {
        if expires_at <= issued_at {
            return Err(SessionError::InvalidLifetime);
        }
        if revoked_at.is_some_and(|timestamp| timestamp < issued_at) {
            return Err(SessionError::InvalidRevocationTime);
        }

        Ok(Self {
            id,
            principal_id,
            issued_at,
            expires_at,
            revoked_at,
        })
    }

    /// Session identifier.
    #[must_use]
    pub const fn id(&self) -> SessionId {
        self.id
    }

    /// Principal identity bound to this session.
    #[must_use]
    pub const fn principal_id(&self) -> PrincipalId {
        self.principal_id
    }

    /// Timestamp at which the session was issued.
    #[must_use]
    pub const fn issued_at(&self) -> UnixTimestamp {
        self.issued_at
    }

    /// Timestamp after which the session must not authenticate.
    #[must_use]
    pub const fn expires_at(&self) -> UnixTimestamp {
        self.expires_at
    }

    /// Revocation timestamp, when revoked.
    #[must_use]
    pub const fn revoked_at(&self) -> Option<UnixTimestamp> {
        self.revoked_at
    }

    /// Authenticates this session at a trusted current time.
    ///
    /// Expiry is exclusive: authentication exactly at expires_at is denied.
    pub fn authenticate(&self, now: UnixTimestamp) -> Result<AuthenticatedPrincipal, SessionError> {
        if self.revoked_at.is_some() {
            return Err(SessionError::Revoked);
        }
        if now < self.issued_at {
            return Err(SessionError::NotYetValid);
        }
        if now >= self.expires_at {
            return Err(SessionError::Expired);
        }

        Ok(AuthenticatedPrincipal {
            principal_id: self.principal_id,
            session_id: self.id,
            authenticated_at: now,
        })
    }

    /// Revokes this session. Revocation is idempotent and keeps the earliest
    /// revocation timestamp.
    pub fn revoke(&mut self, now: UnixTimestamp) -> Result<(), SessionError> {
        if now < self.issued_at {
            return Err(SessionError::InvalidRevocationTime);
        }
        if self.revoked_at.is_none() {
            self.revoked_at = Some(now);
        }
        Ok(())
    }

    /// Rotates to a new session and revokes this one.
    pub fn rotate(
        &mut self,
        now: UnixTimestamp,
        new_expires_at: UnixTimestamp,
    ) -> Result<Self, SessionError> {
        self.authenticate(now)?;
        let replacement = Self::new(self.principal_id, now, new_expires_at)?;
        self.revoke(now)?;
        Ok(replacement)
    }
}

/// Identity proven by a currently valid server-side session.
///
/// Fields are private; callers obtain this proof only through Session::authenticate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AuthenticatedPrincipal {
    principal_id: PrincipalId,
    session_id: SessionId,
    authenticated_at: UnixTimestamp,
}

impl AuthenticatedPrincipal {
    /// Authenticated principal identifier.
    #[must_use]
    pub const fn principal_id(self) -> PrincipalId {
        self.principal_id
    }

    /// Session that established the identity.
    #[must_use]
    pub const fn session_id(self) -> SessionId {
        self.session_id
    }

    /// Trusted time at which the session was evaluated.
    #[must_use]
    pub const fn authenticated_at(self) -> UnixTimestamp {
        self.authenticated_at
    }
}

/// Password hashing/verification boundary.
///
/// Implementations must use a password-specific memory-hard algorithm. The
/// initial generated adapter uses Argon2id and executes work off the async
/// executor's core worker threads, at most [`PasswordWorkLimiter`]'s bound at
/// once.
#[async_trait]
pub trait PasswordHasher: Send + Sync {
    /// Hashes one password into a self-describing PHC string.
    async fn hash(&self, password: &[u8]) -> Result<PasswordHash, PasswordHashError>;

    /// Verifies a password without exposing algorithm-specific errors.
    async fn verify(
        &self,
        password: &[u8],
        expected: &PasswordHash,
    ) -> Result<bool, PasswordHashError>;

    /// Returns whether a successfully verified hash should be upgraded to the
    /// implementation's current password policy.
    ///
    /// Implementations that do not support policy upgrades may keep the
    /// conservative default.
    fn needs_rehash(&self, _expected: &PasswordHash) -> Result<bool, PasswordHashError> {
        Ok(false)
    }
}

/// Safe password hashing failure.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum PasswordHashError {
    /// Hashing/verification mechanism could not complete.
    #[error("password hashing unavailable")]
    Unavailable,
    /// Stored password hash is malformed or unsupported.
    #[error("password hash is invalid")]
    InvalidHash,
    /// Password input exceeds the authentication mechanism's bounded input.
    #[error("password input is invalid")]
    InvalidPassword,
}

/// Optimistic version of a persisted password credential.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct PasswordCredentialVersion(NonZeroU64);

impl PasswordCredentialVersion {
    /// Creates a positive credential version.
    #[must_use]
    pub const fn new(value: u64) -> Option<Self> {
        match NonZeroU64::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    /// Returns the positive version number.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// Persisted password credential resolved by a login identifier.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PasswordCredential {
    principal_id: PrincipalId,
    password_hash: PasswordHash,
    disabled: bool,
    version: PasswordCredentialVersion,
    login: Option<String>,
}

impl PasswordCredential {
    /// Reconstructs a stored credential.
    #[must_use]
    pub const fn new(
        principal_id: PrincipalId,
        password_hash: PasswordHash,
        disabled: bool,
        version: PasswordCredentialVersion,
    ) -> Self {
        Self {
            principal_id,
            password_hash,
            disabled,
            version,
            login: None,
        }
    }

    /// Adds the login exactly as stored, the only address reset links may
    /// be delivered to (never the address a visitor typed).
    #[must_use]
    pub fn with_login(mut self, login: impl Into<String>) -> Self {
        self.login = Some(login.into());
        self
    }

    /// The stored login, when the store provided it.
    #[must_use]
    pub fn login(&self) -> Option<&str> {
        self.login.as_deref()
    }

    /// Principal owning the credential.
    #[must_use]
    pub const fn principal_id(&self) -> PrincipalId {
        self.principal_id
    }

    /// Stored PHC password hash.
    #[must_use]
    pub const fn password_hash(&self) -> &PasswordHash {
        &self.password_hash
    }

    /// Whether interactive authentication is disabled.
    #[must_use]
    pub const fn disabled(&self) -> bool {
        self.disabled
    }

    /// Optimistic version observed with this credential.
    #[must_use]
    pub const fn version(&self) -> PasswordCredentialVersion {
        self.version
    }
}

/// Persistence boundary used by password authentication.
#[async_trait]
pub trait PasswordCredentialStore: Send + Sync {
    /// Resolves one login. Callers pass a [`CanonicalLogin`]; stores should
    /// return the stored login with [`PasswordCredential::with_login`].
    async fn find_by_login(
        &self,
        login: &str,
    ) -> Result<Option<PasswordCredential>, PasswordCredentialStoreError>;

    /// Resolves the credential of a signed-in principal, for step-up
    /// authentication (re-entering the password before changing a second
    /// factor). The default fails closed: a store that does not implement
    /// it makes every step-up unavailable.
    async fn find_by_principal(
        &self,
        _principal_id: PrincipalId,
    ) -> Result<Option<PasswordCredential>, PasswordCredentialStoreError> {
        Err(PasswordCredentialStoreError::Unavailable)
    }

    /// Replaces the password hash only when the observed credential version is
    /// still current. Implementations increment the persisted version on
    /// success and return Conflict on a stale version or concurrently disabled
    /// principal.
    async fn replace_password_hash(
        &self,
        principal_id: PrincipalId,
        expected_version: PasswordCredentialVersion,
        password_hash: &PasswordHash,
    ) -> Result<(), PasswordCredentialStoreError>;
}

/// Safe password credential persistence error.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum PasswordCredentialStoreError {
    /// Durable credential storage is unavailable.
    #[error("credential store unavailable")]
    Unavailable,
    /// Optimistic credential update lost a race or the principal was disabled.
    #[error("credential store conflict")]
    Conflict,
    /// Stored credential data is malformed or violates framework invariants.
    #[error("invalid persisted credential")]
    CorruptRecord,
}

/// Persistence boundary for opaque server-side sessions.
///
/// Implementations store only SessionCredentialDigest, never the bearer token.
/// rotate must revoke the previous record and insert the replacement atomically.
#[async_trait]
pub trait SessionStore: Send + Sync {
    /// Inserts a newly issued session and its credential digest.
    async fn insert(
        &self,
        session: &Session,
        credential: &SessionCredentialDigest,
        csrf: &CsrfTokenDigest,
    ) -> Result<(), SessionStoreError>;

    /// Resolves a persisted session by credential digest.
    ///
    /// A store that enforces a [`SessionIdleTimeout`] returns `None` for a
    /// session unused for longer than the timeout, exactly as for an unknown
    /// credential, and records the use of a live one at most once per
    /// [`SESSION_TOUCH_INTERVAL`]; a failure to record it is an error (fail
    /// closed), not a pass.
    async fn find_by_credential(
        &self,
        credential: &SessionCredentialDigest,
    ) -> Result<Option<Session>, SessionStoreError>;

    /// Verifies the CSRF digest bound to one persisted session.
    async fn verify_csrf(
        &self,
        session_id: SessionId,
        csrf: &CsrfTokenDigest,
    ) -> Result<bool, SessionStoreError>;

    /// Persists session revocation.
    async fn revoke(
        &self,
        session_id: SessionId,
        revoked_at: UnixTimestamp,
    ) -> Result<(), SessionStoreError>;

    /// Atomically revokes one session and inserts its replacement.
    async fn rotate(
        &self,
        previous_session_id: SessionId,
        revoked_at: UnixTimestamp,
        replacement: &Session,
        replacement_credential: &SessionCredentialDigest,
        replacement_csrf: &CsrfTokenDigest,
    ) -> Result<(), SessionStoreError>;
}

/// Safe session persistence error.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum SessionStoreError {
    /// Persistence is temporarily unavailable.
    #[error("session store unavailable")]
    Unavailable,
    /// Uniqueness or optimistic persistence rule was violated.
    #[error("session store conflict")]
    Conflict,
    /// Persisted data violates Session invariants.
    #[error("invalid persisted session")]
    CorruptRecord,
}

/// Session validation failure.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum SessionError {
    /// Expiry was not strictly later than issuance.
    #[error("invalid session lifetime")]
    InvalidLifetime,
    /// Current time predates issuance.
    #[error("session is not yet valid")]
    NotYetValid,
    /// Session reached its expiry.
    #[error("session expired")]
    Expired,
    /// Session was revoked.
    #[error("session revoked")]
    Revoked,
    /// Revocation timestamp predates session issuance.
    #[error("invalid session revocation time")]
    InvalidRevocationTime,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn time(seconds: u64) -> UnixTimestamp {
        UnixTimestamp::from_secs(seconds)
    }

    #[test]
    fn password_credential_version_must_be_positive() {
        assert!(PasswordCredentialVersion::new(0).is_none());
        assert_eq!(
            PasswordCredentialVersion::new(7)
                .expect("positive version")
                .get(),
            7
        );
    }

    #[test]
    fn lifetime_is_fail_closed() {
        let principal = PrincipalId::new();
        assert_eq!(
            Session::new(principal, time(10), time(10)),
            Err(SessionError::InvalidLifetime)
        );

        let session = Session::new(principal, time(10), time(20)).expect("valid session");
        assert_eq!(
            session.authenticate(time(9)),
            Err(SessionError::NotYetValid)
        );
        assert!(session.authenticate(time(10)).is_ok());
        assert!(session.authenticate(time(19)).is_ok());
        assert_eq!(session.authenticate(time(20)), Err(SessionError::Expired));
    }

    #[test]
    fn revocation_denies_and_rotation_changes_session_id() {
        let principal = PrincipalId::new();
        let mut session = Session::new(principal, time(10), time(30)).expect("valid session");
        let original_id = session.id();

        let replacement = session
            .rotate(time(20), time(40))
            .expect("rotation should succeed");

        assert_eq!(session.authenticate(time(21)), Err(SessionError::Revoked));
        assert_ne!(replacement.id(), original_id);
        assert_eq!(replacement.principal_id(), principal);
        assert!(replacement.authenticate(time(21)).is_ok());
    }

    #[test]
    fn failed_rotation_does_not_revoke_current_session() {
        let principal = PrincipalId::new();
        let mut session = Session::new(principal, time(10), time(40)).expect("valid session");
        let original_id = session.id();

        assert_eq!(
            session.rotate(time(20), time(20)),
            Err(SessionError::InvalidLifetime)
        );
        assert_eq!(session.id(), original_id);
        assert!(session.authenticate(time(21)).is_ok());
    }

    #[test]
    fn expired_or_revoked_session_cannot_rotate() {
        let principal = PrincipalId::new();
        let mut expired = Session::new(principal, time(10), time(20)).expect("valid session");
        assert_eq!(
            expired.rotate(time(20), time(30)),
            Err(SessionError::Expired)
        );

        let mut revoked = Session::new(principal, time(10), time(30)).expect("valid session");
        revoked.revoke(time(15)).expect("revocation should succeed");
        assert_eq!(
            revoked.rotate(time(16), time(40)),
            Err(SessionError::Revoked)
        );
    }

    #[test]
    fn revocation_keeps_first_timestamp() {
        let principal = PrincipalId::new();
        let mut session = Session::new(principal, time(10), time(40)).expect("valid session");

        session.revoke(time(20)).expect("revocation should succeed");
        session
            .revoke(time(30))
            .expect("repeated revocation should succeed");

        assert_eq!(session.revoked_at(), Some(time(20)));
    }

    #[test]
    fn persisted_session_is_revalidated() {
        let principal = PrincipalId::new();
        let id = SessionId::new();

        assert_eq!(
            Session::restore(id, principal, time(20), time(20), None),
            Err(SessionError::InvalidLifetime)
        );
        assert_eq!(
            Session::restore(id, principal, time(20), time(30), Some(time(19))),
            Err(SessionError::InvalidRevocationTime)
        );

        let restored = Session::restore(id, principal, time(20), time(30), Some(time(25)))
            .expect("valid persisted session");
        assert_eq!(restored.id(), id);
        assert_eq!(restored.revoked_at(), Some(time(25)));
    }

    #[test]
    fn credential_digest_debug_is_redacted() {
        let digest = SessionCredentialDigest::from_bytes([0xAB; 32]);
        let debug = format!("{digest:?}");
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("AB"));
    }

    #[test]
    fn password_hash_and_tokens_are_validated_and_redacted() {
        let hash = PasswordHash::new("$argon2id$v=19$m=19456,t=2,p=1$c2FsdA$ZGlnaWVzdA")
            .expect("Argon2id PHC string should be accepted");
        assert_eq!(
            hash.expose(),
            "$argon2id$v=19$m=19456,t=2,p=1$c2FsdA$ZGlnaWVzdA"
        );
        assert!(format!("{hash:?}").contains("[REDACTED]"));
        assert!(PasswordHash::new("$argon2i$v=19$bad").is_err());

        let encoded = "ab".repeat(32);
        let bearer = SessionBearerToken::new(encoded.clone()).expect("valid bearer");
        let csrf = CsrfToken::new(encoded).expect("valid csrf token");
        assert!(format!("{bearer:?}").contains("[REDACTED]"));
        assert!(format!("{csrf:?}").contains("[REDACTED]"));
        assert!(SessionBearerToken::new("AB".repeat(32)).is_err());
        assert!(CsrfToken::new("abc").is_err());

        let csrf_digest = CsrfTokenDigest::from_bytes([0xCD; 32]);
        assert!(format!("{csrf_digest:?}").contains("[REDACTED]"));
    }
}
