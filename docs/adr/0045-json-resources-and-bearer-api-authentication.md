# ADR 0045: JSON resources and bearer-token API authentication

> **Update (2026-10-26, D29, [forge-rust#93](https://github.com/rocketinsidecorp/forge-rust/issues/93)).**
> Tenant-scoped scaffold pages are now at `/<plural>`, the tenant being the
> one the session signed in to (the `__Host-forge_tenant` cookie; membership
> and RBAC are checked on every request); live-update streams keep their
> `/tenants/{tenant_id}/streams` path. The text below is kept as decided.

- Status: Accepted
- Date: 2026-10-26
- Refines: [ADR 0024](0024-scaffold-generator.md),
  [ADR 0026](0026-authentication-pages-and-setup.md),
  [ADR 0038](0038-generators-and-destroy.md)
- Parity with: Forge-Go's `forge generate resource`, its scaffold's JSON
  API and `forge new --api-auth` (its `auth.Middleware`, `NewLoginHandler`,
  `NewLogoutHandler` and `GET /v1/auth/session`)

## Context

Forge-Go generates a JSON CRUD API for every resource (`forge generate
resource`, and the scaffold writes it next to its pages) behind bearer
tokens: `forge new --api-auth` mounts a JSON sign-in that issues opaque
session tokens bound to the tenant named at sign-in. Forge-Rust had neither:
its scaffold was HTML-only, its sessions were browser cookies whose tenant
came from the route (`/tenants/{tenant_id}/...`), and `docs/rails-parity.md`
claimed the JSON CRUD generator anyway.

## Decision

1. **Bearer sessions are their own records.** `forge new --api-auth` (or
   `forge generate authentication --api-auth`) adds the generated migration
   `00000000000000000007_forge_bearer_sessions`, a
   `PostgresBearerSessionStore` implementing `forge::tenancy::
   BearerSessionStore`, and `src/adapters/http/api_authentication.rs` with
   Forge-Go's `POST /auth/login`, `POST /auth/logout` and
   `GET /v1/auth/session`. A bearer session is a `forge::auth::Session` plus
   the tenant it was issued for (`forge::tenancy::BearerSession`), kept apart
   from `forge_sessions` so a cookie is never accepted as a bearer token or
   the reverse. Tokens are `SessionBearerToken::generate` (32 bytes from the
   OS CSPRNG through `getrandom`); only their SHA-256 digest (`sha2`) is
   stored and looked up, so no comparison involves the secret (in-memory
   stores compare digests with `subtle`).
2. **Fail closed.** `forge::auth::bearer_credential` accepts exactly one
   `Authorization: Bearer <token>` header with a canonical token; nothing
   else authenticates. `forge::tenancy::resolve_bearer` refuses a missing
   (`401 credentials_required`), malformed, unknown, expired, idle or revoked
   token (`401 credentials_invalid`); `BearerSession::authorize` then
   requires an active membership **in the token's tenant** and an RBAC grant
   (`403 permission_denied`). `forge::web::bearer_refusal` answers with
   Forge-Go's codes and `WWW-Authenticate: Bearer realm="forge"`.
3. **Sessions end with the account's credentials.** The store binds each
   session to `forge_principals.version` at issuance and resolves it only
   while the version matches, the account is enabled and has no TOTP factor:
   a password reset (or hash upgrade), disabling the account and turning
   two-factor on end every bearer session without touching
   `forge_sessions` code. The sign-in re-reads the verified version after the
   insert, so a reset committing in between is refused like a wrong password.
   The idle timeout (`FORGE_AUTH_SESSION_IDLE_TIMEOUT`) and the
   once-a-minute touch are those of browser sessions.
4. **Same throttle, same audit.** The JSON sign-in counts attempts with the
   pages' `sign_in` keys in their `PgLoginThrottle`, so it adds no guessing
   budget, answers every wrong field with one `401`, costs one Argon2id hash
   for unknown logins, and records `auth.sign_in` and `auth.sign_out`.
5. **No bearer token for a two-factor account.** Until Forge-Rust ports
   Forge-Go's `/auth/mfa/complete`, such an account gets `403 mfa_required`
   and signs in through the pages.
6. **Resources.** `forge generate resource` writes the model files of
   `forge generate model` byte for byte plus `src/adapters/http/
   <plural>_api.rs` and its tests; the scaffold writes the same files. The
   tenant comes from the token, never the request; the shared
   `src/adapters/http/api_access.rs` holds `ApiTenantAccess`,
   `ApiGlobalAccess`, `DenyAll` and `BearerTenantAccess`. Without bearer
   tokens, on MySQL, MariaDB and SQLite, and for global resources the printed
   wiring is `DenyAll`. Routes are described with `Operation::bearer()`
   (Forge-Go's `bearerAuth` scheme, JSON `401`/`403`). `PATCH` uses
   `forge::web::Patch` (Forge-Go's `PatchField`).
7. **CSRF.** Like Forge-Go's `csrf.Protection`, the CSRF layer lets a request
   with one `Authorization: Bearer` header or a JSON body pass without a
   token after its cross-site check (a cross-site page cannot send either;
   Forge has no CORS grant), unverified and without a method override; the
   cookie-authenticated routes keep their session-bound check.
8. **Permissions stay in code.** Forge-Rust has no `forge_permissions`
   table (ADR 0038), so no migration registers `PLURAL:read` and
   `PLURAL:write`: the use cases declare them and the printed `RbacPolicy`
   grants them to `owner`.

## Consequences

- `HTML + JSON CRUD generator` is true for Forge-Rust; `show_json` can be
  benchmarked once the load generator drives a bearer token.
- Forge-Go's `/auth/mfa/complete`, `/auth/recovery/*` and `/auth/cookie/*`
  JSON routes remain follow-ups, as do bearer authentication of `--api`
  applications (whose resources are wired `DenyAll`: they have no identity)
  and reading global resources with tenant roles, which Forge-Go allows.
- `GET /v1/auth/session` lists the membership's `roles`, where Forge-Go
  lists permissions from its table.
- Token format differs from Forge-Go's (64 hex digits, not 43 base64url
  characters); the two frameworks never share a database.
