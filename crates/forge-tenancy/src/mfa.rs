//! TOTP multi-factor authentication of tenant sign-ins (Forge-Go's
//! `auth.MFAService` and its PostgreSQL store contract).
//!
//! [`MfaService`] owns the rules; [`MfaStore`] persists them atomically.
//! The cryptographic primitives (RFC 6238 codes, AES-256-GCM seeds, backup
//! codes, challenge tokens) are those of `forge_auth::mfa`.
//!
//! - Enrollment and rotation store a *pending* seed for ten minutes; the
//!   factor (and a fresh set of backup codes) is installed only when a live
//!   code of that seed confirms it. Installing, rotating and disabling a
//!   factor revoke every session of the account and consume its pending
//!   sign-in challenges, so the next sign-in exercises the new state.
//! - A correct password for an account with a factor yields a short-lived
//!   (five-minute) [`MfaChallengeToken`] instead of a session. The challenge
//!   is persisted as a digest and bound to the principal, the tenant it
//!   signed in through, the credential version the password step verified,
//!   the factor's seed digest, the session expiry and the browser session it
//!   was issued to ([`MfaChallengeBinding`]). A password change, a factor
//!   change, a disabled account or a suspended membership makes it useless.
//! - Completing a challenge counts an attempt per account and per network
//!   source *before* any code is checked ([`mfa_throttle_config`]), then
//!   consumes the challenge and records the TOTP period atomically, so a
//!   code is accepted once even under concurrent requests. A backup code is
//!   consumed in the same transaction instead.
//! - Every failure is uniform: wrong code, replayed code, foreign browser and
//!   unknown backup code are all [`MfaError::InvalidCode`].

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use forge_auth::{
    LoginThrottle, MFA_BACKUP_CODE_COUNT, MfaBackupCode, MfaBackupCodeDigest, MfaChallengeBinding,
    MfaChallengeDigest, MfaChallengeToken, MfaPrimitiveError, PasswordCredentialVersion,
    SealedTotpSecret, SecretCipher, ThrottleConfig, ThrottleDecision, ThrottleKey,
    TotpProvisioningUri, TotpSecret, TotpSecretDigest, UnixTimestamp,
};
use forge_security::PrincipalId;
use thiserror::Error;

use crate::{AccountStoreError, TenantId};

/// How long a pending enrollment or rotation waits for its confirming code.
pub const MFA_ENROLLMENT_LIFETIME_SECS: u64 = 10 * 60;
/// How long a sign-in challenge waits for its second factor.
pub const MFA_CHALLENGE_LIFETIME_SECS: u64 = 5 * 60;

/// The limits of second-factor attempts, Forge-Go's
/// `DefaultMFAThrottleConfig`: in five minutes, five attempts per account
/// (and per account and source), 60 per network source.
#[must_use]
pub fn mfa_throttle_config() -> ThrottleConfig {
    ThrottleConfig {
        window: Duration::from_secs(MFA_CHALLENGE_LIFETIME_SECS),
        pair_limit: 5,
        account_limit: 5,
        source_limit: 60,
        max_entries: 20_000,
    }
}

/// Which lifecycle operation a pending seed belongs to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MfaChange {
    /// The first factor of an account that has none.
    Enrollment,
    /// A replacement of the active factor; the current one stays active
    /// until the new seed is confirmed.
    Rotation,
}

/// The factor of an account, as the management page shows it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MfaStatus {
    /// Unused backup codes left.
    pub remaining_backup_codes: u32,
}

/// A seed waiting for its confirming code.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingTotp {
    /// The encrypted seed and its digest.
    pub sealed: SealedTotpSecret,
    /// Confirmation is refused at and after this time.
    pub expires_at: UnixTimestamp,
}

/// The active factor of an account.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActiveTotp {
    /// The encrypted seed and its digest.
    pub sealed: SealedTotpSecret,
    /// The last accepted TOTP period, `None` before the first.
    pub last_counter: Option<u64>,
}

/// A sign-in challenge to persist (its token only as a digest).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MfaChallengeGrant {
    /// Digest of the challenge token.
    pub digest: MfaChallengeDigest,
    /// The account whose password was verified.
    pub principal_id: PrincipalId,
    /// The tenant the account signed in through.
    pub tenant_id: TenantId,
    /// The credential version the password step verified; a password change
    /// advances it and voids the challenge.
    pub credential_version: PasswordCredentialVersion,
    /// The browser session the challenge belongs to.
    pub binding: MfaChallengeBinding,
    /// The challenge is refused at and after this time.
    pub expires_at: UnixTimestamp,
    /// When the session the challenge leads to will expire.
    pub session_expires_at: UnixTimestamp,
}

/// A live challenge and the factor it was issued for.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MfaChallengeRecord {
    /// The account.
    pub principal_id: PrincipalId,
    /// The tenant it signed in through.
    pub tenant_id: TenantId,
    /// The browser session it belongs to.
    pub binding: MfaChallengeBinding,
    /// The factor the challenge was issued for.
    pub factor: ActiveTotp,
    /// The challenge is refused at and after this time.
    pub expires_at: UnixTimestamp,
    /// When the session will expire.
    pub session_expires_at: UnixTimestamp,
}

/// Persistence of factors, pending seeds, backup codes and challenges
/// (Forge-Go's `MFAStore` and `MFALifecycleStore`). Methods that change
/// state are atomic and serialize per account; a `false` result changed
/// nothing.
#[async_trait]
pub trait MfaStore: Send + Sync {
    /// The account's factor, `None` when it has none.
    async fn status(&self, principal: PrincipalId) -> Result<Option<MfaStatus>, AccountStoreError>;

    /// Stores (or replaces) the account's pending seed. `false` when the
    /// account is missing or disabled, or for an enrollment already has a
    /// factor, or for a rotation has none.
    async fn begin(
        &self,
        principal: PrincipalId,
        change: MfaChange,
        pending: &PendingTotp,
    ) -> Result<bool, AccountStoreError>;

    /// The account's unexpired pending seed.
    async fn pending(
        &self,
        principal: PrincipalId,
        now: UnixTimestamp,
    ) -> Result<Option<PendingTotp>, AccountStoreError>;

    /// Installs the unexpired pending seed whose digest is `digest` as the
    /// factor, with `counter` as its last accepted period and `backup_codes`
    /// as its only backup codes; revokes every session of the account,
    /// consumes its pending challenges and deletes the pending seed. `false`
    /// when there is no such seed, the account is disabled, or the
    /// [`MfaChange`] precondition no longer holds.
    async fn activate(
        &self,
        principal: PrincipalId,
        change: MfaChange,
        digest: &TotpSecretDigest,
        counter: u64,
        backup_codes: &[MfaBackupCodeDigest],
        now: UnixTimestamp,
    ) -> Result<bool, AccountStoreError>;

    /// Removes the factor, pending seed and backup codes; revokes every
    /// session and consumes pending challenges. `false` without a factor.
    async fn disable(
        &self,
        principal: PrincipalId,
        now: UnixTimestamp,
    ) -> Result<bool, AccountStoreError>;

    /// The active factor of an enabled account.
    async fn active(&self, principal: PrincipalId)
    -> Result<Option<ActiveTotp>, AccountStoreError>;

    /// Records `counter` as the factor's last accepted period when it is
    /// later than the recorded one and the factor's seed is still `digest`.
    async fn use_totp_counter(
        &self,
        principal: PrincipalId,
        digest: &TotpSecretDigest,
        counter: u64,
    ) -> Result<bool, AccountStoreError>;

    /// Consumes one unused backup code of the account.
    async fn use_backup_code(
        &self,
        principal: PrincipalId,
        code: &MfaBackupCodeDigest,
        now: UnixTimestamp,
    ) -> Result<bool, AccountStoreError>;

    /// Replaces every backup code of the account's factor. `false` without
    /// a factor.
    async fn replace_backup_codes(
        &self,
        principal: PrincipalId,
        backup_codes: &[MfaBackupCodeDigest],
        now: UnixTimestamp,
    ) -> Result<bool, AccountStoreError>;

    /// Persists a challenge for the account's current factor, consuming its
    /// other pending challenges. `false` when the account has no factor, is
    /// disabled, or its credential version is no longer `credential_version`.
    async fn create_challenge(&self, grant: &MfaChallengeGrant) -> Result<bool, AccountStoreError>;

    /// The challenge when it is unconsumed and unexpired at `now`, its
    /// account enabled with the same credential version, its membership of
    /// the tenant active and its factor unchanged.
    async fn challenge(
        &self,
        digest: &MfaChallengeDigest,
        now: UnixTimestamp,
    ) -> Result<Option<MfaChallengeRecord>, AccountStoreError>;

    /// Consumes the live challenge and records `counter` on its factor (seed
    /// `factor`) when `counter` is later than the recorded period, in one
    /// transaction.
    async fn consume_challenge_with_totp(
        &self,
        digest: &MfaChallengeDigest,
        factor: &TotpSecretDigest,
        counter: u64,
        now: UnixTimestamp,
    ) -> Result<bool, AccountStoreError>;

    /// Consumes the live challenge and one unused backup code of its
    /// account, in one transaction.
    async fn consume_challenge_with_backup_code(
        &self,
        digest: &MfaChallengeDigest,
        code: &MfaBackupCodeDigest,
        now: UnixTimestamp,
    ) -> Result<bool, AccountStoreError>;
}

/// Why an MFA operation was refused. It never carries secret material.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum MfaError {
    /// `FORGE_AUTH_MFA_KEY` is not configured: MFA operations fail closed.
    #[error("multi-factor authentication is not configured")]
    NotConfigured,
    /// A wrong, replayed or unknown code, or a challenge presented by
    /// another browser session.
    #[error("multi-factor code is invalid")]
    InvalidCode,
    /// The challenge is unknown, used, expired or no longer valid for its
    /// account: sign in again.
    #[error("multi-factor challenge is invalid or expired")]
    ChallengeExpired,
    /// No unexpired pending seed to confirm.
    #[error("MFA enrollment is invalid or expired")]
    EnrollmentInvalid,
    /// Enrollment of an account that already has a factor.
    #[error("MFA is already enabled")]
    AlreadyEnabled,
    /// A change of a factor the account does not have.
    #[error("MFA is not enabled")]
    NotEnabled,
    /// Too many attempts; retry after the given time.
    #[error("too many multi-factor attempts")]
    Throttled {
        /// Time until the blocking window ends.
        retry_after: Duration,
    },
    /// The issuer or account label of the provisioning URI is invalid.
    #[error("invalid TOTP issuer or account label")]
    InvalidLabel,
    /// The store or the throttle is unavailable.
    #[error("multi-factor authentication unavailable")]
    Unavailable,
    /// Randomness, encryption or persisted data failed (a wrong
    /// `FORGE_AUTH_MFA_KEY` or a tampered seed shows here).
    #[error("multi-factor mechanism failed")]
    Mechanism,
}

impl From<AccountStoreError> for MfaError {
    fn from(error: AccountStoreError) -> Self {
        match error {
            AccountStoreError::Unavailable | AccountStoreError::Conflict => Self::Unavailable,
            AccountStoreError::Corrupt => Self::Mechanism,
        }
    }
}

impl From<MfaPrimitiveError> for MfaError {
    fn from(error: MfaPrimitiveError) -> Self {
        match error {
            MfaPrimitiveError::InvalidLabel => Self::InvalidLabel,
            MfaPrimitiveError::RandomnessUnavailable
            | MfaPrimitiveError::InvalidSecret
            | MfaPrimitiveError::Cipher => Self::Mechanism,
        }
    }
}

/// A rejected sign-in completion, with the account and tenant when the
/// challenge was known (for the audit trail).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MfaRejection {
    /// Why it was rejected.
    pub error: MfaError,
    /// The challenge's account, when the challenge was live.
    pub principal_id: Option<PrincipalId>,
    /// The tenant it signed in through, when the challenge was live.
    pub tenant_id: Option<TenantId>,
}

impl MfaRejection {
    fn new(error: MfaError) -> Self {
        Self {
            error,
            principal_id: None,
            tenant_id: None,
        }
    }

    fn of(record: &MfaChallengeRecord, error: MfaError) -> Self {
        Self {
            error,
            principal_id: Some(record.principal_id),
            tenant_id: Some(record.tenant_id),
        }
    }
}

/// Which second factor was accepted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SecondFactor {
    /// A TOTP code.
    Totp,
    /// A one-time backup code (now used up).
    BackupCode,
}

/// A started enrollment or rotation: shown once, never persisted in clear.
#[derive(Debug)]
pub struct TotpEnrollment {
    /// The seed (its Base32 text is typed into an authenticator app).
    pub secret: TotpSecret,
    /// The `otpauth://` URI authenticator apps import.
    pub provisioning_uri: TotpProvisioningUri,
    /// The confirming code must arrive before this time.
    pub expires_at: UnixTimestamp,
}

/// An issued sign-in challenge.
#[derive(Debug)]
pub struct IssuedMfaChallenge {
    /// The token for the challenge cookie.
    pub token: MfaChallengeToken,
    /// The challenge expires at this time.
    pub expires_at: UnixTimestamp,
}

/// A completed second factor: create the session now.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MfaSignIn {
    /// The signed-in account.
    pub principal_id: PrincipalId,
    /// The tenant it signed in through.
    pub tenant_id: TenantId,
    /// The expiry the password step chose for the session.
    pub session_expires_at: UnixTimestamp,
    /// The factor that completed it.
    pub factor: SecondFactor,
}

/// The MFA rules over an [`MfaStore`], a [`SecretCipher`] and a
/// [`LoginThrottle`] (Forge-Go's `MFAService`).
///
/// Without a cipher (`FORGE_AUTH_MFA_KEY` unset) every operation but
/// [`MfaService::status`] fails with [`MfaError::NotConfigured`], so an
/// account with a factor can never sign in with its password alone.
#[derive(Clone)]
pub struct MfaService {
    store: Arc<dyn MfaStore>,
    cipher: Option<Arc<dyn SecretCipher>>,
    throttle: Arc<dyn LoginThrottle>,
}

impl std::fmt::Debug for MfaService {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MfaService")
            .field("configured", &self.cipher.is_some())
            .finish_non_exhaustive()
    }
}

impl MfaService {
    /// Creates the service; `cipher` is `None` when MFA is not configured.
    /// `throttle` counts second-factor attempts (use
    /// [`mfa_throttle_config`] limits).
    #[must_use]
    pub fn new(
        store: Arc<dyn MfaStore>,
        cipher: Option<Arc<dyn SecretCipher>>,
        throttle: Arc<dyn LoginThrottle>,
    ) -> Self {
        Self {
            store,
            cipher,
            throttle,
        }
    }

    /// Whether a seed cipher is configured.
    #[must_use]
    pub fn is_configured(&self) -> bool {
        self.cipher.is_some()
    }

    /// The throttle second-factor attempts are counted with; step-up checks
    /// count theirs on it too.
    #[must_use]
    pub fn throttle(&self) -> &Arc<dyn LoginThrottle> {
        &self.throttle
    }

    fn cipher(&self) -> Result<&dyn SecretCipher, MfaError> {
        self.cipher.as_deref().ok_or(MfaError::NotConfigured)
    }

    /// The account's factor, `None` when it has none. Works without a
    /// cipher, so sign-in can refuse an enrolled account then.
    pub async fn status(&self, principal: PrincipalId) -> Result<Option<MfaStatus>, MfaError> {
        Ok(self.store.status(principal).await?)
    }

    /// Starts an enrollment or a rotation: a new seed, pending for
    /// [`MFA_ENROLLMENT_LIFETIME_SECS`]. Call only after step-up
    /// authentication of the signed-in account.
    pub async fn begin(
        &self,
        principal: PrincipalId,
        change: MfaChange,
        issuer: &str,
        account: &str,
        now: UnixTimestamp,
    ) -> Result<TotpEnrollment, MfaError> {
        let cipher = self.cipher()?;
        let secret = TotpSecret::generate()?;
        let provisioning_uri = secret.provisioning_uri(issuer, account)?;
        let sealed = secret.seal(cipher)?;
        let expires_at = later(now, MFA_ENROLLMENT_LIFETIME_SECS)?;
        let stored = self
            .store
            .begin(principal, change, &PendingTotp { sealed, expires_at })
            .await?;
        if !stored {
            return Err(match change {
                MfaChange::Enrollment => MfaError::AlreadyEnabled,
                MfaChange::Rotation => MfaError::NotEnabled,
            });
        }
        Ok(TotpEnrollment {
            secret,
            provisioning_uri,
            expires_at,
        })
    }

    /// The pending seed again (after a wrong confirming code).
    pub async fn pending(
        &self,
        principal: PrincipalId,
        issuer: &str,
        account: &str,
        now: UnixTimestamp,
    ) -> Result<Option<TotpEnrollment>, MfaError> {
        let cipher = self.cipher()?;
        let Some(pending) = self.store.pending(principal, now).await? else {
            return Ok(None);
        };
        let secret = TotpSecret::open(cipher, &pending.sealed)?;
        let provisioning_uri = secret.provisioning_uri(issuer, account)?;
        Ok(Some(TotpEnrollment {
            secret,
            provisioning_uri,
            expires_at: pending.expires_at,
        }))
    }

    /// Confirms the pending seed with a live code of it: installs the
    /// factor and returns its [`MFA_BACKUP_CODE_COUNT`] backup codes, to
    /// show once. Every session of the account is revoked.
    pub async fn confirm(
        &self,
        principal: PrincipalId,
        change: MfaChange,
        code: &str,
        now: UnixTimestamp,
    ) -> Result<Vec<MfaBackupCode>, MfaError> {
        let cipher = self.cipher()?;
        let Some(pending) = self.store.pending(principal, now).await? else {
            return Err(MfaError::EnrollmentInvalid);
        };
        if pending.expires_at <= now {
            return Err(MfaError::EnrollmentInvalid);
        }
        let secret = TotpSecret::open(cipher, &pending.sealed)?;
        let counter = secret
            .verify(code, now, None)
            .ok_or(MfaError::InvalidCode)?;
        let (codes, digests) = MfaBackupCode::generate_set(MFA_BACKUP_CODE_COUNT)?;
        let activated = self
            .store
            .activate(
                principal,
                change,
                pending.sealed.digest(),
                counter,
                &digests,
                now,
            )
            .await?;
        if !activated {
            return Err(MfaError::EnrollmentInvalid);
        }
        Ok(codes)
    }

    /// Disables the factor (after step-up authentication). Every session of
    /// the account is revoked.
    pub async fn disable(
        &self,
        principal: PrincipalId,
        now: UnixTimestamp,
    ) -> Result<(), MfaError> {
        self.cipher()?;
        if self.store.disable(principal, now).await? {
            Ok(())
        } else {
            Err(MfaError::NotEnabled)
        }
    }

    /// Replaces the backup codes (after step-up authentication) and returns
    /// the new ones, to show once.
    pub async fn regenerate_backup_codes(
        &self,
        principal: PrincipalId,
        now: UnixTimestamp,
    ) -> Result<Vec<MfaBackupCode>, MfaError> {
        self.cipher()?;
        let (codes, digests) = MfaBackupCode::generate_set(MFA_BACKUP_CODE_COUNT)?;
        if self
            .store
            .replace_backup_codes(principal, &digests, now)
            .await?
        {
            Ok(codes)
        } else {
            Err(MfaError::NotEnabled)
        }
    }

    /// Checks a second factor of a signed-in account for step-up
    /// authentication: a TOTP code (its period is recorded, so it cannot be
    /// used again) or a backup code (used up). The caller counts the attempt
    /// on [`MfaService::throttle`] first.
    pub async fn verify_second_factor(
        &self,
        principal: PrincipalId,
        code: &str,
        now: UnixTimestamp,
    ) -> Result<SecondFactor, MfaError> {
        let cipher = self.cipher()?;
        if let Some(backup) = MfaBackupCode::parse(code) {
            return if self.store.use_backup_code(principal, &backup, now).await? {
                Ok(SecondFactor::BackupCode)
            } else {
                Err(MfaError::InvalidCode)
            };
        }
        let Some(active) = self.store.active(principal).await? else {
            return Err(MfaError::NotEnabled);
        };
        let secret = TotpSecret::open(cipher, &active.sealed)?;
        let counter = secret
            .verify(code, now, active.last_counter)
            .ok_or(MfaError::InvalidCode)?;
        if self
            .store
            .use_totp_counter(principal, active.sealed.digest(), counter)
            .await?
        {
            Ok(SecondFactor::Totp)
        } else {
            Err(MfaError::InvalidCode)
        }
    }

    /// Issues the challenge of a sign-in whose password was right for an
    /// account with a factor. `binding` ties it to the browser session.
    pub async fn issue_challenge(
        &self,
        principal: PrincipalId,
        tenant: TenantId,
        credential_version: PasswordCredentialVersion,
        binding: MfaChallengeBinding,
        session_expires_at: UnixTimestamp,
        now: UnixTimestamp,
    ) -> Result<IssuedMfaChallenge, MfaError> {
        self.cipher()?;
        if session_expires_at <= now {
            return Err(MfaError::Mechanism);
        }
        let token = MfaChallengeToken::generate()?;
        let expires_at = later(now, MFA_CHALLENGE_LIFETIME_SECS)?;
        let grant = MfaChallengeGrant {
            digest: token.digest(),
            principal_id: principal,
            tenant_id: tenant,
            credential_version,
            binding,
            expires_at,
            session_expires_at,
        };
        if !self.store.create_challenge(&grant).await? {
            return Err(MfaError::NotEnabled);
        }
        Ok(IssuedMfaChallenge { token, expires_at })
    }

    /// Completes a sign-in challenge with a TOTP or backup code presented by
    /// the browser session `binding` from `source`. The attempt is counted
    /// per account and per source before the code is checked; success
    /// consumes the challenge (and the code) atomically.
    pub async fn complete(
        &self,
        token: &str,
        binding: &MfaChallengeBinding,
        code: &str,
        source: Option<IpAddr>,
        now: UnixTimestamp,
    ) -> Result<MfaSignIn, MfaRejection> {
        let cipher = self.cipher().map_err(MfaRejection::new)?;
        let token = MfaChallengeToken::new(token.trim())
            .map_err(|_| MfaRejection::new(MfaError::ChallengeExpired))?;
        let digest = token.digest();
        let record = self
            .store
            .challenge(&digest, now)
            .await
            .map_err(|error| MfaRejection::new(error.into()))?
            .filter(|record| record.expires_at > now && record.session_expires_at > now)
            .ok_or(MfaRejection::new(MfaError::ChallengeExpired))?;

        let key = ThrottleKey::new("mfa", &record.principal_id.to_string(), source);
        match self.throttle.attempt(&key).await {
            Ok(ThrottleDecision::Allowed) => {}
            Ok(ThrottleDecision::Throttled { retry_after }) => {
                return Err(MfaRejection::of(
                    &record,
                    MfaError::Throttled { retry_after },
                ));
            }
            Err(_) => return Err(MfaRejection::of(&record, MfaError::Unavailable)),
        }
        let invalid = || MfaRejection::of(&record, MfaError::InvalidCode);
        if record.binding != *binding {
            return Err(invalid());
        }

        let (consumed, factor) = if let Some(backup) = MfaBackupCode::parse(code) {
            let consumed = self
                .store
                .consume_challenge_with_backup_code(&digest, &backup, now)
                .await
                .map_err(|error| MfaRejection::of(&record, error.into()))?;
            (consumed, SecondFactor::BackupCode)
        } else {
            let secret = TotpSecret::open(cipher, &record.factor.sealed)
                .map_err(|error| MfaRejection::of(&record, error.into()))?;
            let Some(counter) = secret.verify(code, now, record.factor.last_counter) else {
                return Err(invalid());
            };
            let consumed = self
                .store
                .consume_challenge_with_totp(&digest, record.factor.sealed.digest(), counter, now)
                .await
                .map_err(|error| MfaRejection::of(&record, error.into()))?;
            (consumed, SecondFactor::Totp)
        };
        if !consumed {
            return Err(invalid());
        }
        if self.throttle.success(&key).await.is_err() {
            return Err(MfaRejection::of(&record, MfaError::Unavailable));
        }
        Ok(MfaSignIn {
            principal_id: record.principal_id,
            tenant_id: record.tenant_id,
            session_expires_at: record.session_expires_at,
            factor,
        })
    }
}

fn later(now: UnixTimestamp, seconds: u64) -> Result<UnixTimestamp, MfaError> {
    now.as_secs()
        .checked_add(seconds)
        .map(UnixTimestamp::from_secs)
        .ok_or(MfaError::Mechanism)
}

#[cfg(any(test, feature = "testing"))]
pub mod testing;

#[cfg(test)]
mod tests;
