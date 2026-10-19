# ADR 0010: Server-side sessions and tenant-scoped authorization context

- Status: Accepted
- Date: 2026-09-27
- Refines: [ADR 0004](0004-tenancy-and-rls.md)
- Refined by: [ADR 0013](0013-persistent-principals-sessions-and-memberships.md), [ADR 0014](0014-argon2-session-cookie-and-csrf.md), [ADR 0024](0024-scaffold-generator.md)

## Context

Phase 3 introduces authentication, authorization, multi-tenancy, PostgreSQL RLS,
and audit behavior. These concerns cross HTTP, application, and persistence
boundaries, so ambiguous identity or ambient tenant state would create a large
security surface.

Roles stored inside long-lived credentials become stale when a membership changes.
A raw tenant UUID is also insufficient proof that the current caller belongs to
that tenant or is authorized for an operation.

## Decision

Forge separates three proofs:

1. A server-side session proves a principal identity and has explicit issuance,
   expiry, revocation, and rotation semantics.
2. A tenant membership binds that principal to one tenant and supplies
   tenant-scoped roles.
3. An RBAC policy evaluates those roles for an exact permission and produces an
   authorization grant only on explicit allow.

A TenantContext can be derived only when the authenticated principal matches an
active membership and policy grants the requested permission. Tenant-sensitive
repository contracts require TenantContext rather than accepting a bare tenant ID.

RBAC is deny-by-default. Missing roles, permissions, memberships, or identity
matches deny the operation. Role and permission names use a strict validated
grammar and are not silently normalized.

Sessions do not carry tenant roles. This allows membership suspension or role
changes to take effect when the membership is next resolved instead of waiting
for a credential to expire.

The initial session contract contains no bearer secret and implements no password
hashing, cookie signing, or token cryptography. Those mechanisms are infrastructure
adapters and require separate dependency/security review. Maintained cryptographic
libraries must be used when those adapters are implemented.

## Consequences

- Tenant scope is explicit in application and repository method signatures.
- Possession of a tenant identifier alone cannot construct an authorized context.
- Authorization policy can be unit-tested independently from HTTP and PostgreSQL.
- Session revocation and rotation are framework-visible lifecycle concepts.
- Membership lookup is required before tenant authorization; caching must preserve
  revocation/role-change semantics.
- PostgreSQL RLS integration can consume TenantContext in the next vertical slice
  and set transaction-local tenant/principal values.
- Audit records can bind the same principal, tenant, permission, and authorization
  outcome without trusting client-provided identity metadata.
