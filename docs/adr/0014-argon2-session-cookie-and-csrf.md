# ADR 0014: Argon2id password authentication, opaque session cookies, and CSRF

- Status: Accepted
- Date: 2026-09-27
- Refines: [ADR 0010](0010-session-and-tenant-authorization-context.md), [ADR 0013](0013-persistent-principals-sessions-and-memberships.md)
- Refined by: [ADR 0023](0023-form-csrf-flash-and-method-override.md) (CSRF token in HTML forms), [ADR 0026](0026-authentication-pages-and-setup.md) (generated sign-in pages, throttling)

## Context

Phase 3 has server-side session lifecycle and durable session storage, but still
needs concrete credential mechanisms. Password hashing must be intentionally slow
and memory-hard. Session bearer values must be unpredictable and non-replayable
from a database dump. Cookie-authenticated unsafe HTTP methods require a CSRF
control that is independent from the HttpOnly bearer cookie.

Implementing cryptographic primitives in Forge would create avoidable review and
correctness risk.

## Decision

Database-enabled generated applications use maintained ecosystem crates at the
outer infrastructure boundary:

- `argon2 0.6.0` from RustCrypto for Argon2id password hashing;
- `getrandom 0.4.3` for 256-bit secrets from the operating-system CSPRNG;
- `sha2 0.11.0` from RustCrypto for fixed-size SHA-256 bearer/CSRF digests;
- `cookie 0.18.2` for RFC cookie parsing/building.

Argon2id uses version 19 with an initial policy of m=19456 KiB, t=2, p=1 and a
32-byte output. Work runs through Tokio `spawn_blocking` instead of occupying
the async executor's core worker threads. Stored PHC strings remain
self-describing so parameters can be raised later. After a successful password
verification, the generated hasher compares the stored Argon2id version and
m/t/p parameters with the current policy. A weaker/outdated hash is rehashed
immediately and persisted through an optimistic credential version update.

The rehash update is deliberately fail-closed on races: the credential store
updates only when the principal is still enabled and its version matches the
version that was verified. A concurrent password change or disable therefore
produces a generic authentication denial instead of overwriting newer state.

Interactive password authentication returns one public denial for unknown login,
disabled account, or wrong password. An unknown/invalid login still performs one
Argon2 operation to reduce account-enumeration timing differences.

Each new session gets two independent 256-bit random secrets:

1. a bearer token carried only in the `__Host-forge_session` cookie;
2. a CSRF token returned separately to trusted same-origin client code.

Only SHA-256 digests of these tokens are stored. The session cookie is Secure,
HttpOnly, Path=/, has no Domain attribute, and uses SameSite=Lax. Unsafe methods
must provide exactly one `x-csrf-token` header whose digest matches the
authenticated server-side session. Safe GET/HEAD/OPTIONS/TRACE requests do not
require the CSRF header.

Cookie parsing is delegated to the cookie crate; Forge does not implement a
custom cookie grammar.

## Consequences

- A session database leak does not directly reveal replayable bearer or CSRF
  credentials.
- Bearer and CSRF secrets cannot substitute for each other.
- Browser JavaScript cannot read the bearer cookie.
- CSRF enforcement remains server-side and survives application restarts.
- Password hashing has a measurable memory/CPU cost and requires capacity tests.
- Successful authentication upgrades outdated Argon2id parameters without
  exposing infrastructure types to application code.
- Concurrent password changes/disables cannot be overwritten by a stale rehash.
- Applications need rate limiting/throttling in addition to expensive hashing.
- HTTP login/logout/tenant-selection endpoints remain application adapters built
  from these mechanisms rather than universal routes imposed by the framework.

Historical RustSec advisories for old cookie and sha2 releases are outside the
selected version ranges. Automated advisory/license/source checks remain release
gates and are not waived by this ADR.
