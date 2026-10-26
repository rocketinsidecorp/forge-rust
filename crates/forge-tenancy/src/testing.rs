//! Test support (the `testing` feature): in-memory stores with the contracts
//! of the PostgreSQL adapters, for application tests that need no database.

use std::sync::{Mutex, MutexGuard};

use async_trait::async_trait;
use forge_auth::{SessionCredentialDigest, SessionId, SessionStoreError, UnixTimestamp};
use forge_security::PrincipalId;

pub use crate::mfa::testing::*;
use crate::{
    BearerSession, BearerSessionStore, Membership, MembershipStore, MembershipStoreError,
    MembershipStoreErrorKind, TenantId,
};

/// In-memory [`BearerSessionStore`]. It keeps no idle time: a session is
/// found until it is revoked or expires (which [`BearerSession::authorize`]
/// checks). Digests are compared in constant time.
#[derive(Default)]
pub struct MemoryBearerSessionStore {
    sessions: Mutex<Vec<(BearerSession, SessionCredentialDigest)>>,
}

impl std::fmt::Debug for MemoryBearerSessionStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("MemoryBearerSessionStore")
    }
}

impl MemoryBearerSessionStore {
    fn sessions(
        &self,
    ) -> Result<MutexGuard<'_, Vec<(BearerSession, SessionCredentialDigest)>>, SessionStoreError>
    {
        self.sessions
            .lock()
            .map_err(|_| SessionStoreError::Unavailable)
    }

    /// Every stored session, for assertions.
    pub fn all(&self) -> Result<Vec<BearerSession>, SessionStoreError> {
        Ok(self
            .sessions()?
            .iter()
            .map(|(session, _)| session.clone())
            .collect())
    }
}

#[async_trait]
impl BearerSessionStore for MemoryBearerSessionStore {
    async fn insert(
        &self,
        session: &BearerSession,
        credential: &SessionCredentialDigest,
    ) -> Result<(), SessionStoreError> {
        let mut sessions = self.sessions()?;
        if sessions.iter().any(|(stored, digest)| {
            stored.session().id() == session.session().id() || same(digest, credential)
        }) {
            return Err(SessionStoreError::Conflict);
        }
        sessions.push((session.clone(), *credential));
        Ok(())
    }

    async fn find_by_credential(
        &self,
        credential: &SessionCredentialDigest,
    ) -> Result<Option<BearerSession>, SessionStoreError> {
        Ok(self
            .sessions()?
            .iter()
            .find(|(_, digest)| same(digest, credential))
            .map(|(session, _)| session.clone()))
    }

    async fn revoke(
        &self,
        session_id: SessionId,
        revoked_at: UnixTimestamp,
    ) -> Result<(), SessionStoreError> {
        for (stored, _) in self.sessions()?.iter_mut() {
            if stored.session().id() == session_id {
                let mut session = stored.session().clone();
                session
                    .revoke(revoked_at)
                    .map_err(|_| SessionStoreError::Conflict)?;
                *stored = BearerSession::new(session, stored.tenant_id());
            }
        }
        Ok(())
    }
}

fn same(left: &SessionCredentialDigest, right: &SessionCredentialDigest) -> bool {
    left.matches(right)
}

/// In-memory [`MembershipStore`].
#[derive(Debug, Default)]
pub struct MemoryMembershipStore {
    memberships: Mutex<Vec<Membership>>,
}

impl MemoryMembershipStore {
    fn memberships(&self) -> Result<MutexGuard<'_, Vec<Membership>>, MembershipStoreError> {
        self.memberships.lock().map_err(|_| {
            MembershipStoreError::new(
                MembershipStoreErrorKind::Unavailable,
                "membership store unavailable",
                true,
            )
        })
    }
}

#[async_trait]
impl MembershipStore for MemoryMembershipStore {
    async fn find(
        &self,
        tenant_id: TenantId,
        principal_id: PrincipalId,
    ) -> Result<Option<Membership>, MembershipStoreError> {
        Ok(self
            .memberships()?
            .iter()
            .find(|membership| {
                membership.tenant_id() == tenant_id && membership.principal_id() == principal_id
            })
            .cloned())
    }

    async fn upsert(&self, membership: &Membership) -> Result<(), MembershipStoreError> {
        let mut memberships = self.memberships()?;
        memberships.retain(|stored| {
            !(stored.tenant_id() == membership.tenant_id()
                && stored.principal_id() == membership.principal_id())
        });
        memberships.push(membership.clone());
        Ok(())
    }

    async fn suspend(
        &self,
        tenant_id: TenantId,
        principal_id: PrincipalId,
    ) -> Result<(), MembershipStoreError> {
        for membership in self.memberships()?.iter_mut() {
            if membership.tenant_id() == tenant_id && membership.principal_id() == principal_id {
                membership.suspend();
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use forge_auth::{Session, SessionBearerToken};

    use super::*;

    #[tokio::test]
    async fn bearer_sessions_are_found_by_digest_and_revoked() {
        let store = MemoryBearerSessionStore::default();
        let token = SessionBearerToken::generate().expect("token");
        let session = Session::new(
            PrincipalId::new(),
            UnixTimestamp::from_secs(1),
            UnixTimestamp::from_secs(9),
        )
        .expect("session");
        let bearer = BearerSession::new(session.clone(), TenantId::new());
        store
            .insert(&bearer, &token.digest())
            .await
            .expect("insert");
        assert_eq!(
            store.insert(&bearer, &token.digest()).await,
            Err(SessionStoreError::Conflict)
        );
        assert_eq!(
            store.find_by_credential(&token.digest()).await,
            Ok(Some(bearer.clone()))
        );
        let other = SessionBearerToken::generate().expect("token");
        assert_eq!(store.find_by_credential(&other.digest()).await, Ok(None));

        store
            .revoke(session.id(), UnixTimestamp::from_secs(2))
            .await
            .expect("revoke");
        let found = store
            .find_by_credential(&token.digest())
            .await
            .expect("find")
            .expect("still stored");
        assert_eq!(
            found.session().revoked_at(),
            Some(UnixTimestamp::from_secs(2))
        );
    }
}
