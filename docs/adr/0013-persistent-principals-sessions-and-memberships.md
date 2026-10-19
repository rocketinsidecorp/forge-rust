# ADR 0013: Persistent principals, credential-digest sessions, and memberships

- Status: Accepted
- Date: 2026-09-27
- Refines: [ADR 0010](0010-session-and-tenant-authorization-context.md)
- Refined by: [ADR 0014](0014-argon2-session-cookie-and-csrf.md), [ADR 0026](0026-authentication-pages-and-setup.md)

## Context

Phase 3 defines authenticated principals, server-side session lifecycle, tenant
memberships, and deny-by-default authorization. Those contracts need durable
PostgreSQL storage without persisting replayable bearer credentials.

Roles must remain outside long-lived session credentials so membership suspension
and role changes take effect on the next authorization lookup.

## Decision

Database-enabled generated applications receive framework-owned identity tables:

- `forge_principals` stores UUIDv7 principal identity, normalized login lookup,
  an optional password PHC string, disable state, timestamps, and version;
- `forge_sessions` stores UUIDv7 session identity, principal identity, fixed
  256-bit bearer and CSRF digests, issuance/expiry/revocation timestamps, and no
  raw bearer or CSRF secret;
- `forge_tenant_memberships` stores tenant/principal membership, current roles,
  lifecycle state, timestamps, and version.

The framework `SessionStore` resolves sessions by bearer digest, records
revocation, validates persisted lifecycle invariants, and rotates sessions
atomically. The generated SQLx adapter never accepts or persists the raw bearer.

The generated membership adapter resolves current roles independently from the
session. Authorization still requires an `AuthenticatedPrincipal`, matching
membership, and explicit RBAC decision before a `TenantContext` exists.

Runtime database privileges are narrower than migration privileges. The runtime
role can read principals for authentication, and read/insert/update session and
membership lifecycle state, but does not receive schema ownership or session
DELETE authority. Session identity, digests, and lifetime are immutable after
insert; revocation is monotonic.

Security audit storage remains a separate concern defined by
[ADR 0012](0012-structured-append-only-audit-evidence.md).

## Consequences

- A database disclosure does not directly expose replayable session or CSRF
  values.
- Session rotation and revocation survive process restarts.
- Corrupt or invalid persisted session timestamps fail closed.
- Membership changes are not embedded in session credentials and can take effect
  on the next lookup.
- Login/password verification can be layered onto the principal credential
  record without leaking third-party cryptographic types into Forge contracts.
- Credential throttling, password upgrade policy, and turnkey HTTP auth routes
  remain separate controls.
