//! Bearer sessions: the credentials of API clients (`forge new --api-auth`).
//!
//! A bearer session is a server-side [`Session`] issued by the JSON sign-in
//! (`POST /auth/login`) for **one tenant**, the one named at sign-in, like
//! Forge-Go's sessions bound to a membership. The client presents its token
//! in `Authorization: Bearer`; the store finds the session by the token's
//! SHA-256 digest only. The session proves the identity and fixes the
//! tenant; the membership and its roles are resolved again on every request
//! ([`BearerSession::authorize`]), so a suspended membership or a revoked
//! grant takes effect at once.
//!
//! Bearer sessions are kept apart from browser sessions (another table in
//! the generated PostgreSQL store): a cookie session is never accepted as a
//! bearer token, nor a bearer token as a cookie.

use async_trait::async_trait;
use forge_auth::{
    AuthenticatedPrincipal, Session, SessionCredentialDigest, SessionError, SessionId,
    SessionStoreError, UnixTimestamp,
};
use forge_security::{Permission, RbacPolicy};
use thiserror::Error;

use crate::{MembershipState, MembershipStore, TenantContext, TenantId};

/// A server-side session of an API client, bound to the tenant it signed in
/// to.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BearerSession {
    session: Session,
    tenant_id: TenantId,
}

impl BearerSession {
    /// Binds `session` to `tenant_id`.
    #[must_use]
    pub const fn new(session: Session, tenant_id: TenantId) -> Self {
        Self { session, tenant_id }
    }

    /// The session: its principal, lifetime and revocation.
    #[must_use]
    pub const fn session(&self) -> &Session {
        &self.session
    }

    /// The only tenant this session may act in.
    #[must_use]
    pub const fn tenant_id(&self) -> TenantId {
        self.tenant_id
    }

    /// Authorizes one request: the session must be live at `now`, the
    /// principal must have an active membership in the session's tenant,
    /// and `policy` must grant one of its roles `permission`. Every refusal
    /// is a [`BearerDenied`] that says which `401`/`403`/`503` to answer
    /// without saying which proof failed.
    pub async fn authorize(
        &self,
        now: UnixTimestamp,
        memberships: &dyn MembershipStore,
        policy: &RbacPolicy,
        permission: &Permission,
    ) -> Result<TenantContext, BearerDenied> {
        let principal = self.authenticate(now)?;
        let membership = memberships
            .find(self.tenant_id, principal.principal_id())
            .await
            .map_err(|_| BearerDenied::Unavailable)?
            .filter(|membership| {
                membership.tenant_id() == self.tenant_id
                    && membership.state() == MembershipState::Active
            })
            .ok_or(BearerDenied::Forbidden)?;
        TenantContext::authorize(principal, &membership, policy, permission)
            .map_err(|_| BearerDenied::Forbidden)
    }

    /// The identity the session proves at `now`: refused once it expired or
    /// was revoked.
    pub fn authenticate(&self, now: UnixTimestamp) -> Result<AuthenticatedPrincipal, BearerDenied> {
        self.session
            .authenticate(now)
            .map_err(|_: SessionError| BearerDenied::InvalidCredentials)
    }
}

/// Why a bearer request was refused. The response says only the status.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum BearerDenied {
    /// No `Authorization` header: `401 credentials_required`.
    #[error("authentication is required")]
    MissingCredentials,
    /// A malformed, unknown, expired, idle or revoked token: `401
    /// credentials_invalid`.
    #[error("credentials are invalid or expired")]
    InvalidCredentials,
    /// No active membership in the session's tenant, or no grant of the
    /// permission: `403 permission_denied`.
    #[error("permission denied")]
    Forbidden,
    /// A store failed: `503`.
    #[error("authentication unavailable")]
    Unavailable,
}

/// Persistence of bearer sessions.
///
/// Implementations store only [`SessionCredentialDigest`]s, never the token,
/// and keep bearer sessions apart from browser sessions.
#[async_trait]
pub trait BearerSessionStore: Send + Sync {
    /// Inserts a newly issued session and its credential digest.
    async fn insert(
        &self,
        session: &BearerSession,
        credential: &SessionCredentialDigest,
    ) -> Result<(), SessionStoreError>;

    /// The session of a credential digest.
    ///
    /// Like [`forge_auth::SessionStore::find_by_credential`], a store that
    /// enforces a [`forge_auth::SessionIdleTimeout`] returns `None` for a
    /// session unused for longer than the timeout and records the use of a
    /// live one; a failure to record it is an error (fail closed).
    async fn find_by_credential(
        &self,
        credential: &SessionCredentialDigest,
    ) -> Result<Option<BearerSession>, SessionStoreError>;

    /// Revokes one session; revoking a revoked session changes nothing.
    async fn revoke(
        &self,
        session_id: SessionId,
        revoked_at: UnixTimestamp,
    ) -> Result<(), SessionStoreError>;
}

#[cfg(test)]
mod tests {
    use forge_security::{PrincipalId, Role};

    use super::*;
    use crate::Membership;
    use crate::testing::MemoryMembershipStore;

    fn time(seconds: u64) -> UnixTimestamp {
        UnixTimestamp::from_secs(seconds)
    }

    struct Fixture {
        bearer: BearerSession,
        memberships: MemoryMembershipStore,
        policy: RbacPolicy,
        permission: Permission,
    }

    async fn setup(granted: bool) -> Fixture {
        let principal = PrincipalId::new();
        let tenant = TenantId::new();
        let session = Session::new(principal, time(10), time(100)).expect("session");
        let role = Role::new("owner").expect("role");
        let memberships = MemoryMembershipStore::default();
        memberships
            .upsert(&Membership::active(tenant, principal, vec![role.clone()]))
            .await
            .expect("membership");
        let permission = Permission::new("posts:read").expect("permission");
        let mut policy = RbacPolicy::new();
        if granted {
            policy.allow(role, permission.clone());
        }
        Fixture {
            bearer: BearerSession::new(session, tenant),
            memberships,
            policy,
            permission,
        }
    }

    #[tokio::test]
    async fn a_live_session_of_an_active_granted_member_is_authorized_in_its_tenant() {
        let fixture = setup(true).await;
        let context = fixture
            .bearer
            .authorize(
                time(20),
                &fixture.memberships,
                &fixture.policy,
                &fixture.permission,
            )
            .await
            .expect("authorized");
        assert_eq!(context.tenant_id(), fixture.bearer.tenant_id());
    }

    #[tokio::test]
    async fn expired_revoked_ungranted_and_suspended_are_refused() {
        let fixture = setup(true).await;
        for now in [time(5), time(100), time(500)] {
            assert_eq!(
                fixture
                    .bearer
                    .authorize(
                        now,
                        &fixture.memberships,
                        &fixture.policy,
                        &fixture.permission
                    )
                    .await
                    .map(|_| ()),
                Err(BearerDenied::InvalidCredentials),
                "{now:?}"
            );
        }

        let mut revoked = fixture.bearer.session().clone();
        revoked.revoke(time(15)).expect("revoke");
        let revoked = BearerSession::new(revoked, fixture.bearer.tenant_id());
        assert_eq!(
            revoked
                .authorize(
                    time(20),
                    &fixture.memberships,
                    &fixture.policy,
                    &fixture.permission
                )
                .await
                .map(|_| ()),
            Err(BearerDenied::InvalidCredentials)
        );

        let ungranted = setup(false).await;
        assert_eq!(
            ungranted
                .bearer
                .authorize(
                    time(20),
                    &ungranted.memberships,
                    &ungranted.policy,
                    &ungranted.permission
                )
                .await
                .map(|_| ()),
            Err(BearerDenied::Forbidden)
        );

        let principal = fixture.bearer.session().principal_id();
        fixture
            .memberships
            .suspend(fixture.bearer.tenant_id(), principal)
            .await
            .expect("suspend");
        assert_eq!(
            fixture
                .bearer
                .authorize(
                    time(20),
                    &fixture.memberships,
                    &fixture.policy,
                    &fixture.permission
                )
                .await
                .map(|_| ()),
            Err(BearerDenied::Forbidden)
        );
    }

    #[tokio::test]
    async fn another_tenants_membership_does_not_count() {
        let fixture = setup(true).await;
        let elsewhere = BearerSession::new(fixture.bearer.session().clone(), TenantId::new());
        assert_eq!(
            elsewhere
                .authorize(
                    time(20),
                    &fixture.memberships,
                    &fixture.policy,
                    &fixture.permission
                )
                .await
                .map(|_| ()),
            Err(BearerDenied::Forbidden)
        );
    }
}
