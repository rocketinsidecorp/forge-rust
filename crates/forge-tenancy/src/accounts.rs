//! Tenant-aware account contracts used by browser sign-in and password
//! resets (ADR 0026).
//!
//! Every sign-in names its tenant explicitly by [`TenantSlug`]; there is no
//! default tenant and Forge does not derive one from the `Host` header. A
//! password reset is bound to a principal *and* the tenant membership it was
//! requested through, so redeeming it requires that membership to still be
//! active.

use async_trait::async_trait;
use forge_auth::{PasswordHash, PasswordResetTokenDigest, UnixTimestamp};
use forge_security::PrincipalId;
use thiserror::Error;

use crate::TenantId;

/// Validated tenant slug: a lowercase ASCII letter, then 1 to 62 lowercase
/// letters, digits or hyphens (`acme`, `north-wind-2`).
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TenantSlug(String);

impl TenantSlug {
    /// Accepts only an already normalized slug.
    pub fn new(value: impl Into<String>) -> Result<Self, TenantSlugError> {
        let value = value.into();
        let bytes = value.as_bytes();
        let valid = (2..=63).contains(&bytes.len())
            && bytes.first().is_some_and(u8::is_ascii_lowercase)
            && bytes
                .iter()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-');
        if valid {
            Ok(Self(value))
        } else {
            Err(TenantSlugError)
        }
    }

    /// Normalizes what a person typed (surrounding whitespace removed, ASCII
    /// letters lowercased) and validates it.
    pub fn parse_input(input: &str) -> Result<Self, TenantSlugError> {
        Self::new(input.trim().to_ascii_lowercase())
    }

    /// The slug text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for TenantSlug {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Invalid tenant slug.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[error(
    "a tenant slug is a lowercase letter followed by 1 to 62 lowercase letters, digits or hyphens"
)]
pub struct TenantSlugError;

/// Safe failure of an account store (tenant directory or password resets).
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum AccountStoreError {
    /// Storage is unavailable.
    #[error("account store unavailable")]
    Unavailable,
    /// A uniqueness or concurrency rule prevented the write.
    #[error("account store conflict")]
    Conflict,
    /// Persisted data violates Forge invariants.
    #[error("invalid persisted account data")]
    Corrupt,
}

/// Resolves tenants by the slug people type at sign-in.
#[async_trait]
pub trait TenantDirectory: Send + Sync {
    /// The tenant named `slug`, if any.
    async fn find_by_slug(&self, slug: &TenantSlug) -> Result<Option<TenantId>, AccountStoreError>;
}

/// A password reset issued for one principal through one tenant membership.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PasswordResetGrant {
    principal_id: PrincipalId,
    tenant_id: TenantId,
    digest: PasswordResetTokenDigest,
    issued_at: UnixTimestamp,
    expires_at: UnixTimestamp,
}

impl PasswordResetGrant {
    /// Creates a grant whose expiry is strictly later than its issuance.
    pub fn new(
        principal_id: PrincipalId,
        tenant_id: TenantId,
        digest: PasswordResetTokenDigest,
        issued_at: UnixTimestamp,
        expires_at: UnixTimestamp,
    ) -> Result<Self, PasswordResetGrantError> {
        if expires_at <= issued_at {
            return Err(PasswordResetGrantError);
        }
        Ok(Self {
            principal_id,
            tenant_id,
            digest,
            issued_at,
            expires_at,
        })
    }

    /// The principal whose password the token may change.
    #[must_use]
    pub const fn principal_id(&self) -> PrincipalId {
        self.principal_id
    }

    /// The tenant the reset was requested through.
    #[must_use]
    pub const fn tenant_id(&self) -> TenantId {
        self.tenant_id
    }

    /// The token digest to persist (never the token).
    #[must_use]
    pub const fn digest(&self) -> &PasswordResetTokenDigest {
        &self.digest
    }

    /// Issuance time.
    #[must_use]
    pub const fn issued_at(&self) -> UnixTimestamp {
        self.issued_at
    }

    /// The token is refused at and after this time.
    #[must_use]
    pub const fn expires_at(&self) -> UnixTimestamp {
        self.expires_at
    }
}

/// A reset whose expiry is not after its issuance.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[error("a password reset must expire after it is issued")]
pub struct PasswordResetGrantError;

/// The account whose password a redeemed token changed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RedeemedPasswordReset {
    /// The principal whose password changed.
    pub principal_id: PrincipalId,
    /// The tenant the reset was requested through.
    pub tenant_id: TenantId,
}

/// Persistence of one-time password reset tokens.
#[async_trait]
pub trait PasswordResetStore: Send + Sync {
    /// Stores an issued reset (only its digest).
    async fn issue(&self, grant: &PasswordResetGrant) -> Result<(), AccountStoreError>;

    /// Redeems a token in one atomic step, or returns `None` and changes
    /// nothing when the token is unknown, already used, expired at `now`, or
    /// its principal is disabled or no longer an active member of the tenant
    /// it was issued through.
    ///
    /// On success the implementation must, atomically: mark the token used,
    /// replace the principal's password hash with `password_hash` (advancing
    /// its credential version), revoke every unrevoked session of the
    /// principal, and mark the principal's other unused reset tokens used.
    async fn redeem(
        &self,
        digest: &PasswordResetTokenDigest,
        now: UnixTimestamp,
        password_hash: &PasswordHash,
    ) -> Result<Option<RedeemedPasswordReset>, AccountStoreError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_are_strict_and_input_is_normalized() {
        for valid in [
            "ab",
            "acme",
            "north-wind-2",
            &format!("a{}", "b".repeat(62)),
        ] {
            assert!(TenantSlug::new(valid).is_ok(), "{valid}");
        }
        for invalid in [
            "",
            "a",
            "Acme",
            "2acme",
            "-acme",
            "ac me",
            "acme_co",
            "açme",
            &format!("a{}", "b".repeat(63)),
        ] {
            assert_eq!(TenantSlug::new(invalid), Err(TenantSlugError), "{invalid}");
        }
        assert_eq!(
            TenantSlug::parse_input("  ACME \n").map(|slug| slug.to_string()),
            Ok("acme".to_owned())
        );
    }

    #[test]
    fn a_grant_must_expire_after_issuance() {
        let digest = PasswordResetTokenDigest::from_bytes([7; 32]);
        let principal = PrincipalId::new();
        let tenant = TenantId::new();
        assert_eq!(
            PasswordResetGrant::new(
                principal,
                tenant,
                digest,
                UnixTimestamp::from_secs(10),
                UnixTimestamp::from_secs(10),
            ),
            Err(PasswordResetGrantError)
        );
        let grant = PasswordResetGrant::new(
            principal,
            tenant,
            digest,
            UnixTimestamp::from_secs(10),
            UnixTimestamp::from_secs(11),
        )
        .expect("valid grant");
        assert_eq!(grant.principal_id(), principal);
        assert_eq!(grant.tenant_id(), tenant);
        assert_eq!(grant.digest(), &digest);
        assert_eq!(grant.expires_at(), UnixTimestamp::from_secs(11));
    }
}
