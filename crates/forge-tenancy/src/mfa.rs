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
//! - Step-up authentication before a factor change
//!   ([`MfaService::verify_step_up`]) records a TOTP code's period at once
//!   but only checks a backup code: the change itself uses the code up, in
//!   the store transaction that applies it, so a change that fails (a wrong
//!   code of the new key, an expired rotation, a precondition that no longer
//!   holds) leaves the backup code unused.
//! - A pending seed is an enrollment's while the account has no factor and a
//!   rotation's while it has one ([`MfaStore::pending`] reads both in one
//!   snapshot), so the seed of one kind is never checked, confirmed or shown
//!   as the other.

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
    ///
    /// `step_up`, here and in the other changes, is a backup code given for
    /// step-up authentication: it is used up in the same transaction, and
    /// the change is refused (`false`, nothing changed) when it is not an
    /// unused code of the account.
    async fn begin(
        &self,
        principal: PrincipalId,
        change: MfaChange,
        pending: &PendingTotp,
        step_up: Option<&MfaBackupCodeDigest>,
    ) -> Result<bool, AccountStoreError>;

    /// The account's unexpired pending seed of kind `change`: an
    /// enrollment's when the account has no factor, a rotation's when it has
    /// one, decided in the same snapshot as the seed is read.
    async fn pending(
        &self,
        principal: PrincipalId,
        change: MfaChange,
        now: UnixTimestamp,
    ) -> Result<Option<PendingTotp>, AccountStoreError>;

    /// Installs the unexpired pending seed whose digest is `digest` as the
    /// factor, with `counter` as its last accepted period and `backup_codes`
    /// as its only backup codes; revokes every session of the account,
    /// consumes its pending challenges and deletes the pending seed. `false`
    /// when there is no such seed, the account is disabled, the
    /// [`MfaChange`] precondition no longer holds or `step_up` is not an
    /// unused code.
    #[allow(clippy::too_many_arguments)]
    async fn activate(
        &self,
        principal: PrincipalId,
        change: MfaChange,
        digest: &TotpSecretDigest,
        counter: u64,
        backup_codes: &[MfaBackupCodeDigest],
        step_up: Option<&MfaBackupCodeDigest>,
        now: UnixTimestamp,
    ) -> Result<bool, AccountStoreError>;

    /// Removes the factor, pending seed and backup codes; revokes every
    /// session and consumes pending challenges. `false` without a factor or
    /// when `step_up` is not an unused code.
    async fn disable(
        &self,
        principal: PrincipalId,
        step_up: Option<&MfaBackupCodeDigest>,
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

    /// Whether `code` is an unused backup code of the account's factor,
    /// without using it.
    async fn has_backup_code(
        &self,
        principal: PrincipalId,
        code: &MfaBackupCodeDigest,
    ) -> Result<bool, AccountStoreError>;

    /// Replaces every backup code of the account's factor. `false` without
    /// a factor or when `step_up` is not an unused code. Like Forge-Go's, it
    /// revokes no session and voids no pending challenge: neither depends on
    /// the backup codes, and an old code stops completing a challenge
    /// because it is gone.
    async fn replace_backup_codes(
        &self,
        principal: PrincipalId,
        backup_codes: &[MfaBackupCodeDigest],
        step_up: Option<&MfaBackupCodeDigest>,
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
    /// The backup code given for step-up authentication was used or
    /// replaced before the change could use it up: nothing changed.
    #[error("multi-factor step-up code is no longer valid")]
    StepUpInvalid,
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

/// A step-up second factor [`MfaService::verify_step_up`] accepted. Pass it
/// to the change it authorizes: a backup code is used up only by that
/// change, in the transaction that applies it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StepUpProof {
    backup_code: Option<MfaBackupCodeDigest>,
}

impl StepUpProof {
    /// Which factor was given.
    #[must_use]
    pub const fn factor(&self) -> SecondFactor {
        match self.backup_code {
            Some(_) => SecondFactor::BackupCode,
            None => SecondFactor::Totp,
        }
    }

    fn backup_code(proof: Option<&Self>) -> Option<&MfaBackupCodeDigest> {
        proof.and_then(|proof| proof.backup_code.as_ref())
    }
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
    /// authentication of the signed-in account; `step_up` is its proof (a
    /// backup code is used up only when the seed is stored).
    pub async fn begin(
        &self,
        principal: PrincipalId,
        change: MfaChange,
        issuer: &str,
        account: &str,
        step_up: Option<&StepUpProof>,
        now: UnixTimestamp,
    ) -> Result<TotpEnrollment, MfaError> {
        let cipher = self.cipher()?;
        let secret = TotpSecret::generate()?;
        let provisioning_uri = secret.provisioning_uri(issuer, account)?;
        let sealed = secret.seal(cipher)?;
        let expires_at = later(now, MFA_ENROLLMENT_LIFETIME_SECS)?;
        let stored = self
            .store
            .begin(
                principal,
                change,
                &PendingTotp { sealed, expires_at },
                StepUpProof::backup_code(step_up),
            )
            .await?;
        if !stored {
            let otherwise = match change {
                MfaChange::Enrollment => MfaError::AlreadyEnabled,
                MfaChange::Rotation => MfaError::NotEnabled,
            };
            return Err(self.refusal(principal, change, step_up, otherwise).await?);
        }
        Ok(TotpEnrollment {
            secret,
            provisioning_uri,
            expires_at,
        })
    }

    /// The pending seed of kind `change` again (after a wrong confirming
    /// code): `None` when none is pending, or the one pending is of the other
    /// kind. Call only after step-up authentication: the seed is a
    /// credential.
    pub async fn pending(
        &self,
        principal: PrincipalId,
        change: MfaChange,
        issuer: &str,
        account: &str,
        now: UnixTimestamp,
    ) -> Result<Option<TotpEnrollment>, MfaError> {
        let cipher = self.cipher()?;
        let Some(pending) = self.store.pending(principal, change, now).await? else {
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

    /// Confirms the pending seed of kind `change` with a live code of it:
    /// installs the factor and returns its [`MFA_BACKUP_CODE_COUNT`] backup
    /// codes, to show once. Every session of the account is revoked. An
    /// enrollment of an account that has a factor is
    /// [`MfaError::AlreadyEnabled`] and a rotation of one without is
    /// [`MfaError::NotEnabled`], whatever is pending: the code is never
    /// checked against a seed of the other kind. A wrong code is
    /// [`MfaError::InvalidCode`] and uses up no step-up backup code.
    pub async fn confirm(
        &self,
        principal: PrincipalId,
        change: MfaChange,
        code: &str,
        step_up: Option<&StepUpProof>,
        now: UnixTimestamp,
    ) -> Result<Vec<MfaBackupCode>, MfaError> {
        let cipher = self.cipher()?;
        let pending = self
            .store
            .pending(principal, change, now)
            .await?
            .filter(|pending| pending.expires_at > now);
        let Some(pending) = pending else {
            return Err(self
                .refusal(principal, change, None, MfaError::EnrollmentInvalid)
                .await?);
        };
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
                StepUpProof::backup_code(step_up),
                now,
            )
            .await?;
        if !activated {
            return Err(self
                .refusal(principal, change, step_up, MfaError::EnrollmentInvalid)
                .await?);
        }
        Ok(codes)
    }

    /// Why the store refused a change of kind `change` (nothing changed):
    /// the factor itself (an enrollment of an account that has one, a
    /// rotation of one without), then a step-up backup code used or replaced
    /// meanwhile, else `otherwise`.
    async fn refusal(
        &self,
        principal: PrincipalId,
        change: MfaChange,
        step_up: Option<&StepUpProof>,
        otherwise: MfaError,
    ) -> Result<MfaError, MfaError> {
        match (change, self.store.status(principal).await?) {
            (MfaChange::Enrollment, Some(_)) => return Ok(MfaError::AlreadyEnabled),
            (MfaChange::Rotation, None) => return Ok(MfaError::NotEnabled),
            _ => {}
        }
        Ok(self
            .step_up_refusal(principal, step_up)
            .await?
            .unwrap_or(otherwise))
    }

    /// [`MfaError::StepUpInvalid`] when `step_up` is a backup code that is
    /// no longer unused.
    async fn step_up_refusal(
        &self,
        principal: PrincipalId,
        step_up: Option<&StepUpProof>,
    ) -> Result<Option<MfaError>, MfaError> {
        let Some(code) = StepUpProof::backup_code(step_up) else {
            return Ok(None);
        };
        Ok(
            (!self.store.has_backup_code(principal, code).await?)
                .then_some(MfaError::StepUpInvalid),
        )
    }

    /// Disables the factor (after step-up authentication; `step_up` is its
    /// proof, a backup code used up only with the factor). Every session of
    /// the account is revoked.
    pub async fn disable(
        &self,
        principal: PrincipalId,
        step_up: Option<&StepUpProof>,
        now: UnixTimestamp,
    ) -> Result<(), MfaError> {
        self.cipher()?;
        if self
            .store
            .disable(principal, StepUpProof::backup_code(step_up), now)
            .await?
        {
            return Ok(());
        }
        if self.store.status(principal).await?.is_none() {
            return Err(MfaError::NotEnabled);
        }
        Err(self
            .step_up_refusal(principal, step_up)
            .await?
            .unwrap_or(MfaError::NotEnabled))
    }

    /// Replaces the backup codes (after step-up authentication; `step_up`
    /// is its proof, a backup code used up only with the replacement) and
    /// returns the new ones, to show once.
    pub async fn regenerate_backup_codes(
        &self,
        principal: PrincipalId,
        step_up: Option<&StepUpProof>,
        now: UnixTimestamp,
    ) -> Result<Vec<MfaBackupCode>, MfaError> {
        self.cipher()?;
        let (codes, digests) = MfaBackupCode::generate_set(MFA_BACKUP_CODE_COUNT)?;
        if self
            .store
            .replace_backup_codes(principal, &digests, StepUpProof::backup_code(step_up), now)
            .await?
        {
            return Ok(codes);
        }
        if self.store.status(principal).await?.is_none() {
            return Err(MfaError::NotEnabled);
        }
        Err(self
            .step_up_refusal(principal, step_up)
            .await?
            .unwrap_or(MfaError::NotEnabled))
    }

    /// Checks a second factor of a signed-in account for step-up
    /// authentication before a factor change: a TOTP code (its period is
    /// recorded at once, so it cannot be used again) or an unused backup
    /// code, which is only checked here. Pass the proof to the change
    /// ([`MfaService::begin`], [`MfaService::confirm`],
    /// [`MfaService::disable`], [`MfaService::regenerate_backup_codes`]):
    /// it uses the backup code up in the transaction that applies the
    /// change, so a change that fails costs no backup code. The caller
    /// counts the attempt on [`MfaService::throttle`] first. A wrong,
    /// replayed or used code is [`MfaError::InvalidCode`], an account
    /// without a factor [`MfaError::NotEnabled`].
    pub async fn verify_step_up(
        &self,
        principal: PrincipalId,
        code: &str,
        now: UnixTimestamp,
    ) -> Result<StepUpProof, MfaError> {
        self.cipher()?;
        if let Some(backup) = MfaBackupCode::parse(code) {
            return if self.store.has_backup_code(principal, &backup).await? {
                Ok(StepUpProof {
                    backup_code: Some(backup),
                })
            } else {
                Err(MfaError::InvalidCode)
            };
        }
        self.verify_totp(principal, code, now).await?;
        Ok(StepUpProof { backup_code: None })
    }

    /// Checks and uses up a second factor of a signed-in account: a TOTP
    /// code (its period is recorded, so it cannot be used again) or a backup
    /// code (used up at once). For a step-up before a factor change use
    /// [`MfaService::verify_step_up`] instead, so a failed change keeps the
    /// backup code. The caller counts the attempt on
    /// [`MfaService::throttle`] first.
    pub async fn verify_second_factor(
        &self,
        principal: PrincipalId,
        code: &str,
        now: UnixTimestamp,
    ) -> Result<SecondFactor, MfaError> {
        self.cipher()?;
        if let Some(backup) = MfaBackupCode::parse(code) {
            return if self.store.use_backup_code(principal, &backup, now).await? {
                Ok(SecondFactor::BackupCode)
            } else {
                Err(MfaError::InvalidCode)
            };
        }
        self.verify_totp(principal, code, now).await?;
        Ok(SecondFactor::Totp)
    }

    /// Checks a TOTP code of the active factor and records its period.
    async fn verify_totp(
        &self,
        principal: PrincipalId,
        code: &str,
        now: UnixTimestamp,
    ) -> Result<(), MfaError> {
        let cipher = self.cipher()?;
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
            Ok(())
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
