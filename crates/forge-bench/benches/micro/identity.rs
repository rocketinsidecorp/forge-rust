//! `session_cookie_verify` and `password_verify`.

use std::collections::HashMap;
use std::hint::black_box;

use argon2::password_hash::{PasswordHasher as _, PasswordVerifier as _};
use argon2::{Algorithm, Argon2, Params, Version};
use async_trait::async_trait;
use cookie::Cookie;
use forge::auth::{
    CsrfTokenDigest, Session, SessionBearerToken, SessionCredentialDigest, SessionId, SessionStore,
    SessionStoreError, UnixTimestamp,
};
use forge::http::{Method, Request, header};
use forge::security::{Permission, PrincipalId, RbacPolicy, Role};
use forge::tenancy::{Membership, MembershipStore, MembershipStoreError, TenantContext, TenantId};
use forge_bench::Case;
use forge_bench::exec::block_on;
use sha2::{Digest as _, Sha256};

use crate::support::{RequestParts, TOKEN, must};

pub fn cases(cases: &mut Vec<Case>) {
    session_cookie_verify(cases);
    password_verify(cases);
}

// session_cookie_verify ------------------------------------------------------

const SESSION_COOKIE: &str = "__Host-forge_session";

/// A session store in memory, behind the same `async_trait` contract as the
/// PostgreSQL one a generated application uses.
struct Sessions(HashMap<SessionCredentialDigest, Session>);

#[async_trait]
impl SessionStore for Sessions {
    async fn insert(
        &self,
        _: &Session,
        _: &SessionCredentialDigest,
        _: &CsrfTokenDigest,
    ) -> Result<(), SessionStoreError> {
        Err(SessionStoreError::Unavailable)
    }

    async fn find_by_credential(
        &self,
        credential: &SessionCredentialDigest,
    ) -> Result<Option<Session>, SessionStoreError> {
        Ok(self.0.get(credential).cloned())
    }

    async fn verify_csrf(
        &self,
        _: SessionId,
        _: &CsrfTokenDigest,
    ) -> Result<bool, SessionStoreError> {
        Err(SessionStoreError::Unavailable)
    }

    async fn revoke(&self, _: SessionId, _: UnixTimestamp) -> Result<(), SessionStoreError> {
        Err(SessionStoreError::Unavailable)
    }

    async fn rotate(
        &self,
        _: SessionId,
        _: UnixTimestamp,
        _: &Session,
        _: &SessionCredentialDigest,
        _: &CsrfTokenDigest,
    ) -> Result<(), SessionStoreError> {
        Err(SessionStoreError::Unavailable)
    }
}

/// An in-memory membership store (async contract, like the real one).
struct Memberships(Vec<Membership>);

#[async_trait]
impl MembershipStore for Memberships {
    async fn find(
        &self,
        tenant_id: TenantId,
        principal_id: PrincipalId,
    ) -> Result<Option<Membership>, MembershipStoreError> {
        Ok(self
            .0
            .iter()
            .find(|membership| {
                membership.tenant_id() == tenant_id && membership.principal_id() == principal_id
            })
            .cloned())
    }

    async fn upsert(&self, _: &Membership) -> Result<(), MembershipStoreError> {
        unreachable!("the benchmark only reads memberships")
    }

    async fn suspend(&self, _: TenantId, _: PrincipalId) -> Result<(), MembershipStoreError> {
        unreachable!("the benchmark only reads memberships")
    }
}

/// What the generated application's `SessionTenantAccess` does for a GET to a
/// tenant-scoped page, with in-memory stores: find the session cookie in the
/// `Cookie` header, decode the token, take its SHA-256 digest, look the
/// session up, check it at the current time, load the membership of the
/// route's tenant and let the RBAC policy authorize the permission, giving
/// the `TenantContext`.
///
/// Those functions live in the generated application
/// (`src/infrastructure/security`, `src/adapters/http/access.rs`), not in a
/// framework crate a benchmark can import, so this restates them step for
/// step with the framework's public types. A real store adds one database
/// round trip, which the end-to-end scenarios measure.
fn authorize(
    sessions: &dyn SessionStore,
    memberships: &dyn MembershipStore,
    policy: &RbacPolicy,
    request: &Request,
    tenant: &str,
    permission: &Permission,
    now: UnixTimestamp,
) -> Result<TenantContext, &'static str> {
    let mut bearer = None;
    for value in request.headers().get_all(header::COOKIE) {
        let value = value.to_str().map_err(|_| "cookie header")?;
        for parsed in Cookie::split_parse(value).flatten() {
            if parsed.name() == SESSION_COOKIE {
                bearer = Some(
                    SessionBearerToken::new(parsed.value().to_owned())
                        .map_err(|_| "session cookie")?,
                );
            }
        }
    }
    let bearer = bearer.ok_or("no session cookie")?;
    let mut digest = [0_u8; 32];
    digest.copy_from_slice(&Sha256::digest(bearer.expose().as_bytes()));
    let session =
        block_on(sessions.find_by_credential(&SessionCredentialDigest::from_bytes(digest)))
            .map_err(|_| "session store")?
            .ok_or("unknown session")?;
    let principal = session.authenticate(now).map_err(|_| "session")?;
    let tenant_id: TenantId = tenant.parse().map_err(|_| "tenant id")?;
    let membership = block_on(memberships.find(tenant_id, principal.principal_id()))
        .map_err(|_| "membership store")?
        .ok_or("no membership")?;
    TenantContext::authorize(principal, &membership, policy, permission).map_err(|_| "forbidden")
}

fn session_cookie_verify(cases: &mut Vec<Case>) {
    cases.push(Case::new("session_cookie_verify", "", 200_000, || {
        let bearer = must(SessionBearerToken::new(TOKEN), "bearer");
        let mut digest = [0_u8; 32];
        digest.copy_from_slice(&Sha256::digest(bearer.expose().as_bytes()));
        let principal = PrincipalId::new();
        let tenant = TenantId::new();
        let now = UnixTimestamp::from_secs(1_792_000_000);
        let session = must(
            Session::new(
                principal,
                UnixTimestamp::from_secs(now.as_secs() - 60),
                UnixTimestamp::from_secs(now.as_secs() + 3_600),
            ),
            "session",
        );
        let sessions = Sessions(HashMap::from([(
            SessionCredentialDigest::from_bytes(digest),
            session,
        )]));
        let owner = must(Role::new("owner"), "role");
        let permission = must(Permission::new("posts:read"), "permission");
        let mut policy = RbacPolicy::new();
        policy.allow(owner.clone(), permission.clone());
        let memberships = Memberships(vec![Membership::active(tenant, principal, vec![owner])]);
        let request = RequestParts::new(Method::GET, &format!("/tenants/{tenant}/posts"))
            .named_header(
                "cookie",
                &format!("{SESSION_COOKIE}={TOKEN}; __Host-forge_csrf={TOKEN}"),
            )
            .build();
        let tenant = tenant.to_string();
        let context = must(
            authorize(
                &sessions,
                &memberships,
                &policy,
                &request,
                &tenant,
                &permission,
                now,
            ),
            "authorize",
        );
        assert_eq!(context.principal_id(), principal);
        Box::new(move |iterations| {
            for _ in 0..iterations {
                black_box(
                    authorize(
                        &sessions,
                        &memberships,
                        &policy,
                        black_box(&request),
                        &tenant,
                        &permission,
                        now,
                    )
                    .is_ok(),
                );
            }
        })
    }));
}

// password_verify ------------------------------------------------------------

/// The generated application's password hasher (`Argon2idPasswordHasher`):
/// **Argon2id**, version 19, memory 19456 KiB, 2 passes, 1 lane, 32-byte
/// output (the OWASP minimum profile), verified from the parameters in the
/// stored PHC string. Deliberately slow: the work factor is the defense, so
/// nobody should "optimize" this figure, and it is the only one that is not
/// microseconds or less. Forge-Go uses PBKDF2-HMAC-SHA256 with 600000
/// iterations; the two are not comparable beyond "tens of milliseconds".
fn password_verify(cases: &mut Vec<Case>) {
    // Five verifications per run: about a tenth of a second.
    cases.push(Case::new("password_verify", "", 5, || {
        let params = must(Params::new(19_456, 2, 1, Some(32)), "argon2 parameters");
        let hasher = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
        let encoded = must(
            hasher.hash_password(b"correct horse battery staple"),
            "hash",
        )
        .to_string();
        Box::new(move |iterations| {
            for _ in 0..iterations {
                let parsed = must(argon2::PasswordHash::new(&encoded), "phc string");
                black_box(
                    Argon2::default()
                        .verify_password(black_box(b"correct horse battery staple"), &parsed)
                        .is_ok(),
                );
            }
        })
    }));
}
