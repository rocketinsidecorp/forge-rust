//! An in-memory [`MfaStore`] for tests (the `testing` feature): the
//! PostgreSQL adapter's contract in one process.
//!
//! It holds no identity data: it cannot see disabled accounts, credential
//! versions or memberships, which the PostgreSQL adapter checks on every
//! challenge. Session revocation is delegated to the callback given to
//! [`MemoryMfaStore::with_session_revoker`].

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use async_trait::async_trait;
use forge_auth::{
    MfaBackupCodeDigest, MfaChallengeDigest, PasswordCredentialVersion, TotpSecretDigest,
    UnixTimestamp,
};
use forge_security::PrincipalId;

use super::{
    ActiveTotp, MfaChallengeGrant, MfaChallengeRecord, MfaChange, MfaStatus, MfaStore, PendingTotp,
};
use crate::AccountStoreError;

/// Called with the account and time whenever the store would revoke every
/// session of the account.
pub type SessionRevoker = Arc<dyn Fn(PrincipalId, UnixTimestamp) + Send + Sync>;

struct Factor {
    active: ActiveTotp,
    backup_codes: Vec<(MfaBackupCodeDigest, bool)>,
}

struct Challenge {
    grant: MfaChallengeGrant,
    /// The factor's seed digest when the challenge was issued.
    factor: TotpSecretDigest,
    consumed: bool,
}

#[derive(Default)]
struct State {
    pending: HashMap<PrincipalId, PendingTotp>,
    factors: HashMap<PrincipalId, Factor>,
    challenges: Vec<Challenge>,
    versions: HashMap<PrincipalId, PasswordCredentialVersion>,
}

/// In-memory [`MfaStore`].
#[derive(Default)]
pub struct MemoryMfaStore {
    state: Mutex<State>,
    revoker: Option<SessionRevoker>,
}

impl std::fmt::Debug for MemoryMfaStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("MemoryMfaStore")
    }
}

impl MemoryMfaStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Calls `revoker` where the PostgreSQL adapter revokes every session of
    /// an account (installing, rotating and disabling a factor).
    #[must_use]
    pub fn with_session_revoker(mut self, revoker: SessionRevoker) -> Self {
        self.revoker = Some(revoker);
        self
    }

    /// Records a password change of `principal` to `version`, voiding its
    /// challenges issued for an older version (what a password reset does
    /// to the PostgreSQL adapter's challenges).
    pub fn set_credential_version(
        &self,
        principal: PrincipalId,
        version: PasswordCredentialVersion,
    ) {
        self.lock().versions.insert(principal, version);
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn revoke(&self, principal: PrincipalId, now: UnixTimestamp) {
        if let Some(revoker) = &self.revoker {
            revoker(principal, now);
        }
    }
}

impl State {
    /// Uses up `step_up` when it is an unused backup code of the account (or
    /// there is none); `false` otherwise. Call last, once every other
    /// precondition holds, so a refused change uses nothing up.
    fn spend(&mut self, principal: PrincipalId, step_up: Option<&MfaBackupCodeDigest>) -> bool {
        let Some(code) = step_up else {
            return true;
        };
        let Some(factor) = self.factors.get_mut(&principal) else {
            return false;
        };
        match factor
            .backup_codes
            .iter_mut()
            .find(|(stored, used)| stored == code && !used)
        {
            Some((_, used)) => {
                *used = true;
                true
            }
            None => false,
        }
    }

    fn consume_challenges_of(&mut self, principal: PrincipalId) {
        for challenge in &mut self.challenges {
            if challenge.grant.principal_id == principal {
                challenge.consumed = true;
            }
        }
    }

    fn live(&self, digest: &MfaChallengeDigest, now: UnixTimestamp) -> Option<usize> {
        self.challenges.iter().position(|challenge| {
            let grant = &challenge.grant;
            grant.digest == *digest
                && !challenge.consumed
                && grant.expires_at > now
                && grant.session_expires_at > now
                && self
                    .versions
                    .get(&grant.principal_id)
                    .is_none_or(|version| *version == grant.credential_version)
                && self
                    .factors
                    .get(&grant.principal_id)
                    .is_some_and(|factor| factor.active.sealed.digest() == &challenge.factor)
        })
    }
}

#[async_trait]
impl MfaStore for MemoryMfaStore {
    async fn status(&self, principal: PrincipalId) -> Result<Option<MfaStatus>, AccountStoreError> {
        Ok(self.lock().factors.get(&principal).map(|factor| MfaStatus {
            remaining_backup_codes: factor
                .backup_codes
                .iter()
                .filter(|(_, used)| !used)
                .count()
                .try_into()
                .unwrap_or(u32::MAX),
        }))
    }

    async fn begin(
        &self,
        principal: PrincipalId,
        change: MfaChange,
        pending: &PendingTotp,
        step_up: Option<&MfaBackupCodeDigest>,
    ) -> Result<bool, AccountStoreError> {
        let mut state = self.lock();
        let enabled = state.factors.contains_key(&principal);
        if enabled != (change == MfaChange::Rotation) || !state.spend(principal, step_up) {
            return Ok(false);
        }
        state.pending.insert(principal, pending.clone());
        Ok(true)
    }

    async fn pending(
        &self,
        principal: PrincipalId,
        change: MfaChange,
        now: UnixTimestamp,
    ) -> Result<Option<PendingTotp>, AccountStoreError> {
        let state = self.lock();
        if state.factors.contains_key(&principal) != (change == MfaChange::Rotation) {
            return Ok(None);
        }
        Ok(state
            .pending
            .get(&principal)
            .filter(|pending| pending.expires_at > now)
            .cloned())
    }

    async fn activate(
        &self,
        principal: PrincipalId,
        change: MfaChange,
        digest: &TotpSecretDigest,
        counter: u64,
        backup_codes: &[MfaBackupCodeDigest],
        step_up: Option<&MfaBackupCodeDigest>,
        now: UnixTimestamp,
    ) -> Result<bool, AccountStoreError> {
        {
            let mut state = self.lock();
            let Some(pending) = state.pending.get(&principal).cloned() else {
                return Ok(false);
            };
            let enabled = state.factors.contains_key(&principal);
            if pending.sealed.digest() != digest
                || pending.expires_at <= now
                || enabled != (change == MfaChange::Rotation)
                || !state.spend(principal, step_up)
            {
                return Ok(false);
            }
            state.factors.insert(
                principal,
                Factor {
                    active: ActiveTotp {
                        sealed: pending.sealed,
                        last_counter: Some(counter),
                    },
                    backup_codes: backup_codes.iter().map(|code| (*code, false)).collect(),
                },
            );
            state.pending.remove(&principal);
            state.consume_challenges_of(principal);
        }
        self.revoke(principal, now);
        Ok(true)
    }

    async fn disable(
        &self,
        principal: PrincipalId,
        step_up: Option<&MfaBackupCodeDigest>,
        now: UnixTimestamp,
    ) -> Result<bool, AccountStoreError> {
        {
            let mut state = self.lock();
            if !state.factors.contains_key(&principal) || !state.spend(principal, step_up) {
                return Ok(false);
            }
            state.factors.remove(&principal);
            state.pending.remove(&principal);
            state.consume_challenges_of(principal);
        }
        self.revoke(principal, now);
        Ok(true)
    }

    async fn active(
        &self,
        principal: PrincipalId,
    ) -> Result<Option<ActiveTotp>, AccountStoreError> {
        Ok(self
            .lock()
            .factors
            .get(&principal)
            .map(|factor| factor.active.clone()))
    }

    async fn use_totp_counter(
        &self,
        principal: PrincipalId,
        digest: &TotpSecretDigest,
        counter: u64,
    ) -> Result<bool, AccountStoreError> {
        let mut state = self.lock();
        let Some(factor) = state.factors.get_mut(&principal) else {
            return Ok(false);
        };
        if factor.active.sealed.digest() != digest
            || factor
                .active
                .last_counter
                .is_some_and(|last| counter <= last)
        {
            return Ok(false);
        }
        factor.active.last_counter = Some(counter);
        Ok(true)
    }

    async fn use_backup_code(
        &self,
        principal: PrincipalId,
        code: &MfaBackupCodeDigest,
        _now: UnixTimestamp,
    ) -> Result<bool, AccountStoreError> {
        let mut state = self.lock();
        let Some(factor) = state.factors.get_mut(&principal) else {
            return Ok(false);
        };
        match factor
            .backup_codes
            .iter_mut()
            .find(|(stored, used)| stored == code && !used)
        {
            Some((_, used)) => {
                *used = true;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    async fn has_backup_code(
        &self,
        principal: PrincipalId,
        code: &MfaBackupCodeDigest,
    ) -> Result<bool, AccountStoreError> {
        Ok(self.lock().factors.get(&principal).is_some_and(|factor| {
            factor
                .backup_codes
                .iter()
                .any(|(stored, used)| stored == code && !used)
        }))
    }

    async fn replace_backup_codes(
        &self,
        principal: PrincipalId,
        backup_codes: &[MfaBackupCodeDigest],
        step_up: Option<&MfaBackupCodeDigest>,
        _now: UnixTimestamp,
    ) -> Result<bool, AccountStoreError> {
        let mut state = self.lock();
        if !state.spend(principal, step_up) {
            return Ok(false);
        }
        let Some(factor) = state.factors.get_mut(&principal) else {
            return Ok(false);
        };
        factor.backup_codes = backup_codes.iter().map(|code| (*code, false)).collect();
        Ok(true)
    }

    async fn create_challenge(&self, grant: &MfaChallengeGrant) -> Result<bool, AccountStoreError> {
        let mut state = self.lock();
        let Some(factor) = state
            .factors
            .get(&grant.principal_id)
            .map(|factor| *factor.active.sealed.digest())
        else {
            return Ok(false);
        };
        if state
            .versions
            .get(&grant.principal_id)
            .is_some_and(|version| *version != grant.credential_version)
        {
            return Ok(false);
        }
        state.consume_challenges_of(grant.principal_id);
        state.challenges.push(Challenge {
            grant: grant.clone(),
            factor,
            consumed: false,
        });
        Ok(true)
    }

    async fn challenge(
        &self,
        digest: &MfaChallengeDigest,
        now: UnixTimestamp,
    ) -> Result<Option<MfaChallengeRecord>, AccountStoreError> {
        let state = self.lock();
        let Some(index) = state.live(digest, now) else {
            return Ok(None);
        };
        let grant = &state.challenges[index].grant;
        let Some(factor) = state.factors.get(&grant.principal_id) else {
            return Ok(None);
        };
        Ok(Some(MfaChallengeRecord {
            principal_id: grant.principal_id,
            tenant_id: grant.tenant_id,
            credential_version: grant.credential_version,
            binding: grant.binding,
            factor: factor.active.clone(),
            expires_at: grant.expires_at,
            session_expires_at: grant.session_expires_at,
        }))
    }

    async fn consume_challenge_with_totp(
        &self,
        digest: &MfaChallengeDigest,
        factor: &TotpSecretDigest,
        counter: u64,
        now: UnixTimestamp,
    ) -> Result<bool, AccountStoreError> {
        let mut state = self.lock();
        let Some(index) = state.live(digest, now) else {
            return Ok(false);
        };
        let principal = state.challenges[index].grant.principal_id;
        let Some(active) = state.factors.get_mut(&principal) else {
            return Ok(false);
        };
        if active.active.sealed.digest() != factor
            || active
                .active
                .last_counter
                .is_some_and(|last| counter <= last)
        {
            return Ok(false);
        }
        active.active.last_counter = Some(counter);
        state.challenges[index].consumed = true;
        Ok(true)
    }

    async fn consume_challenge_with_backup_code(
        &self,
        digest: &MfaChallengeDigest,
        code: &MfaBackupCodeDigest,
        now: UnixTimestamp,
    ) -> Result<bool, AccountStoreError> {
        let mut state = self.lock();
        let Some(index) = state.live(digest, now) else {
            return Ok(false);
        };
        let principal = state.challenges[index].grant.principal_id;
        let Some(factor) = state.factors.get_mut(&principal) else {
            return Ok(false);
        };
        let Some((_, used)) = factor
            .backup_codes
            .iter_mut()
            .find(|(stored, used)| stored == code && !used)
        else {
            return Ok(false);
        };
        *used = true;
        state.challenges[index].consumed = true;
        Ok(true)
    }
}
