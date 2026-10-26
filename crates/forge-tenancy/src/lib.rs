//! Explicit tenant context for Forge applications.
//!
//! There is no default or ambient tenant. A TenantContext can be created only
//! by combining a currently authenticated principal, an active membership for
//! the same principal, and an explicit successful RBAC decision.
//!
//! Browser sign-in and password resets use the tenant-aware contracts
//! [`TenantDirectory`] and [`PasswordResetStore`]; people name a tenant by
//! its [`TenantSlug`]. Accounts with a TOTP second factor sign in through
//! [`MfaService`] over an [`MfaStore`] (the `testing` feature adds the
//! in-memory [`testing::MemoryMfaStore`]). API clients authenticate with
//! [`BearerSession`]s, bound to the tenant they signed in to and kept by a
//! [`BearerSessionStore`].

mod accounts;
mod bearer;
mod mfa;
#[cfg(any(test, feature = "testing"))]
pub mod testing;

use async_trait::async_trait;
use forge_auth::AuthenticatedPrincipal;
use forge_core::Id;
use forge_security::{
    AuthorizationError, AuthorizationGrant, Permission, PrincipalId, RbacPolicy, Role,
};
use thiserror::Error;

pub use accounts::{
    AccountStoreError, PasswordResetGrant, PasswordResetGrantError, PasswordResetStore,
    RedeemedPasswordReset, TenantDirectory, TenantSlug, TenantSlugError,
};

pub use bearer::{BearerDenied, BearerSession, BearerSessionStore, resolve_bearer};
pub use mfa::{
    ActiveTotp, IssuedMfaChallenge, MFA_CHALLENGE_LIFETIME_SECS, MFA_ENROLLMENT_LIFETIME_SECS,
    MfaChallengeGrant, MfaChallengeRecord, MfaChange, MfaChangeRequest, MfaError, MfaRejection,
    MfaService, MfaSignIn, MfaStatus, MfaStore, PendingTotp, SecondFactor, StepUpProof,
    TotpEnrollment, mfa_throttle_config,
};

/// Marker type for a tenant identity.
#[derive(Debug)]
pub enum TenantMarker {}

/// UUIDv7 tenant identifier.
pub type TenantId = Id<TenantMarker>;

/// Membership lifecycle state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MembershipState {
    /// Membership can participate in authorization.
    Active,
    /// Membership is retained for history but must not authorize access.
    Suspended,
}

/// Tenant membership and tenant-scoped roles.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Membership {
    tenant_id: TenantId,
    principal_id: PrincipalId,
    roles: Vec<Role>,
    state: MembershipState,
}

impl Membership {
    /// Creates an active tenant membership.
    #[must_use]
    pub fn active(tenant_id: TenantId, principal_id: PrincipalId, roles: Vec<Role>) -> Self {
        Self {
            tenant_id,
            principal_id,
            roles,
            state: MembershipState::Active,
        }
    }

    /// Restores a persisted membership with its current lifecycle state.
    #[must_use]
    pub fn restore(
        tenant_id: TenantId,
        principal_id: PrincipalId,
        roles: Vec<Role>,
        state: MembershipState,
    ) -> Self {
        Self {
            tenant_id,
            principal_id,
            roles,
            state,
        }
    }

    /// Tenant this membership belongs to.
    #[must_use]
    pub const fn tenant_id(&self) -> TenantId {
        self.tenant_id
    }

    /// Principal this membership belongs to.
    #[must_use]
    pub const fn principal_id(&self) -> PrincipalId {
        self.principal_id
    }

    /// Tenant-scoped roles used by authorization policy.
    #[must_use]
    pub fn roles(&self) -> &[Role] {
        &self.roles
    }

    /// Current membership state.
    #[must_use]
    pub const fn state(&self) -> MembershipState {
        self.state
    }

    /// Suspends this membership. Suspended memberships always fail closed.
    pub fn suspend(&mut self) {
        self.state = MembershipState::Suspended;
    }
}

/// Explicit tenant/authorization context passed to tenant-sensitive use cases.
///
/// Fields are private so callers cannot construct an authorized tenant context
/// by merely possessing a tenant UUID.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TenantContext {
    tenant_id: TenantId,
    principal_id: PrincipalId,
    authorization: AuthorizationGrant,
}

impl TenantContext {
    /// Resolves and authorizes tenant scope.
    pub fn authorize(
        authenticated: AuthenticatedPrincipal,
        membership: &Membership,
        policy: &RbacPolicy,
        permission: &Permission,
    ) -> Result<Self, TenantContextError> {
        if membership.state != MembershipState::Active {
            return Err(TenantContextError::InactiveMembership);
        }
        if membership.principal_id != authenticated.principal_id() {
            return Err(TenantContextError::PrincipalMismatch);
        }

        let authorization = policy
            .authorize(authenticated.principal_id(), membership.roles(), permission)
            .map_err(TenantContextError::Authorization)?;

        Ok(Self {
            tenant_id: membership.tenant_id,
            principal_id: authenticated.principal_id(),
            authorization,
        })
    }

    /// Authorized tenant.
    #[must_use]
    pub const fn tenant_id(&self) -> TenantId {
        self.tenant_id
    }

    /// Authorized principal.
    #[must_use]
    pub const fn principal_id(&self) -> PrincipalId {
        self.principal_id
    }

    /// Policy grant that established this context.
    #[must_use]
    pub const fn authorization(&self) -> &AuthorizationGrant {
        &self.authorization
    }
}

/// Failure while deriving tenant context.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum TenantContextError {
    /// Membership is suspended or otherwise inactive.
    #[error("tenant membership is inactive")]
    InactiveMembership,
    /// Authenticated identity does not match the membership.
    #[error("authenticated principal does not match tenant membership")]
    PrincipalMismatch,
    /// Tenant-scoped RBAC policy denied the requested permission.
    #[error("tenant authorization denied")]
    Authorization(#[source] AuthorizationError),
}

/// Safe persistence error categories for tenant memberships.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MembershipStoreErrorKind {
    /// Membership storage is unavailable.
    Unavailable,
    /// A concurrent membership transition prevented the requested write.
    Conflict,
    /// Persisted membership data violated Forge invariants.
    Corrupt,
}

/// Membership persistence failure with a bounded safe message.
#[derive(Debug, Error)]
#[error("{message}")]
pub struct MembershipStoreError {
    kind: MembershipStoreErrorKind,
    message: &'static str,
    retryable: bool,
}

impl MembershipStoreError {
    /// Creates a classified membership storage error.
    #[must_use]
    pub const fn new(
        kind: MembershipStoreErrorKind,
        message: &'static str,
        retryable: bool,
    ) -> Self {
        Self {
            kind,
            message,
            retryable,
        }
    }

    /// Stable storage error category.
    #[must_use]
    pub const fn kind(&self) -> MembershipStoreErrorKind {
        self.kind
    }

    /// Whether a bounded retry may be reasonable.
    #[must_use]
    pub const fn retryable(&self) -> bool {
        self.retryable
    }
}

/// Persistence contract for tenant membership and current tenant-scoped roles.
///
/// Memberships are resolved independently from sessions so role changes and
/// suspension take effect on the next authorization decision.
#[async_trait]
pub trait MembershipStore: Send + Sync {
    /// Loads one principal's membership for one tenant.
    async fn find(
        &self,
        tenant_id: TenantId,
        principal_id: PrincipalId,
    ) -> Result<Option<Membership>, MembershipStoreError>;

    /// Inserts or replaces the current roles/state for a membership.
    async fn upsert(&self, membership: &Membership) -> Result<(), MembershipStoreError>;

    /// Idempotently suspends a membership.
    async fn suspend(
        &self,
        tenant_id: TenantId,
        principal_id: PrincipalId,
    ) -> Result<(), MembershipStoreError>;
}

#[cfg(test)]
mod tests {
    use forge_auth::{Session, UnixTimestamp};
    use forge_security::{Permission, RbacPolicy, Role};

    use super::*;

    fn authenticated(principal: PrincipalId) -> AuthenticatedPrincipal {
        Session::new(
            principal,
            UnixTimestamp::from_secs(10),
            UnixTimestamp::from_secs(20),
        )
        .expect("valid session")
        .authenticate(UnixTimestamp::from_secs(11))
        .expect("session should authenticate")
    }

    #[test]
    fn context_requires_matching_identity_membership_and_permission() {
        let principal = PrincipalId::new();
        let tenant = TenantId::new();
        let admin = Role::new("admin").expect("valid role");
        let permission = Permission::new("users:read").expect("valid permission");
        let membership = Membership::active(tenant, principal, vec![admin.clone()]);
        let mut policy = RbacPolicy::new();
        policy.allow(admin, permission.clone());

        let context =
            TenantContext::authorize(authenticated(principal), &membership, &policy, &permission)
                .expect("authorized membership should create context");

        assert_eq!(context.tenant_id(), tenant);
        assert_eq!(context.principal_id(), principal);
        assert_eq!(context.authorization().permission(), &permission);
    }

    #[test]
    fn another_principal_cannot_reuse_membership() {
        let member = PrincipalId::new();
        let attacker = PrincipalId::new();
        let membership = Membership::active(
            TenantId::new(),
            member,
            vec![Role::new("admin").expect("valid role")],
        );
        let permission = Permission::new("users:read").expect("valid permission");
        let mut policy = RbacPolicy::new();
        policy.allow(Role::new("admin").expect("valid role"), permission.clone());

        assert_eq!(
            TenantContext::authorize(authenticated(attacker), &membership, &policy, &permission,),
            Err(TenantContextError::PrincipalMismatch)
        );
    }

    #[test]
    fn restored_membership_preserves_current_state() {
        let tenant = TenantId::new();
        let principal = PrincipalId::new();
        let role = Role::new("member").expect("valid role");

        let membership = Membership::restore(
            tenant,
            principal,
            vec![role.clone()],
            MembershipState::Suspended,
        );

        assert_eq!(membership.tenant_id(), tenant);
        assert_eq!(membership.principal_id(), principal);
        assert_eq!(membership.roles(), &[role]);
        assert_eq!(membership.state(), MembershipState::Suspended);
    }

    #[test]
    fn suspended_membership_and_missing_permission_deny() {
        let principal = PrincipalId::new();
        let permission = Permission::new("users:write").expect("valid permission");
        let mut membership = Membership::active(
            TenantId::new(),
            principal,
            vec![Role::new("member").expect("valid role")],
        );
        let policy = RbacPolicy::new();

        assert_eq!(
            TenantContext::authorize(authenticated(principal), &membership, &policy, &permission,),
            Err(TenantContextError::Authorization(
                AuthorizationError::Denied
            ))
        );

        membership.suspend();
        assert_eq!(
            TenantContext::authorize(authenticated(principal), &membership, &policy, &permission,),
            Err(TenantContextError::InactiveMembership)
        );
    }
}
