# ADR 0026: Authentication pages, sign-in redirects, first-owner setup and seeds

- Status: Accepted
- Date: 2026-10-19
- Refines: [ADR 0013](0013-persistent-principals-sessions-and-memberships.md), [ADR 0014](0014-argon2-session-cookie-and-csrf.md), [ADR 0023](0023-form-csrf-flash-and-method-override.md), [ADR 0024](0024-scaffold-generator.md)
- Refined by: [ADR 0032](0032-trusted-proxies-and-client-ip.md) (throttle source is the resolved client address), [ADR 0035](0035-mail.md) (reset links are emailed when mail is configured)

## Context

Phase 3 had the mechanisms of authentication (Argon2id, digest-only
persistent sessions, session-bound CSRF, memberships, RBAC, RLS, audit
storage) and, since ADR 0022 to 0024, server-rendered forms and scaffolds.
What `rails generate authentication` gives a Rails 8 developer was missing:

- no sign-in, sign-out or password-reset page; ADR 0014 left login routes to
  application code and `docs/cli.md` said Forge does not impose them;
- a tenant-scoped scaffold page answered a signed-out browser with a bare
  `401`, and its printed RBAC policy was empty, so nobody could use it;
- nothing created the first tenant and account without hand-written SQL, and
  there was no tenant record a person could name at sign-in;
- no login throttling, no reset tokens, no seed convention.

Forge-Go shipped the same feature (its ADR 0026); this decision keeps its
user-facing behavior and security decisions within Forge-Rust's
architecture, where concrete mechanisms (Argon2id, cookies, SQLx stores) are
generated infrastructure and framework crates hold contracts and rules.

## Decision

### Pages are generated code; security rules live in the framework

This reverses the "no universal login routes" stance deliberately, through a
generator: the routes are application code the developer can read and
change, not routes the framework mounts.

- `forge-auth`: `LoginThrottle` and the bounded `MemoryLoginThrottle`
  (SHA-256 keys; per 15 minutes 5 attempts per login and source, 50 per
  login, 300 per source, IPv6 counted per /64; at most 20,000 keys, and a
  full table evicts the oldest counters that block nothing, refusing only
  when every counter is blocking); `CanonicalLogin` (trimmed, ASCII only,
  lowercased: the one form used for storage, lookup, throttling and
  delivery; a non-ASCII login is an unknown account); the `NewPassword` policy (12 characters,
  at most 1024 bytes, no control characters); `PasswordResetToken` (256 bits
  from the OS CSPRNG, canonical hex, SHA-256 `PasswordResetTokenDigest`,
  redacted `Debug`); the `PasswordResetNotifier` port and its fail-closed
  default `UndeliverablePasswordResets`. `forge-auth` therefore now depends
  on the already admitted `getrandom` and `sha2`, as `forge-views` does.
- `forge-tenancy`: `TenantSlug`, the `TenantDirectory` port and the
  `PasswordResetStore` contract (tenant-bound, atomic single-use redemption
  that changes the password, revokes every session and uses up the
  account's other tokens).
- `forge-views`: `return_path` (local paths only, at most 2048 bytes, the
  `see_other` rules), `prefers_html`, and the `SignInRedirect` layer that
  turns a `401` into a `303` to the sign-in page for HTML clients (`GET` and
  `HEAD` keep the page as `return_to`). The CSP layer now keeps a
  `Referrer-Policy` the handler chose.
- `forge` facade: `App::command` for administrative commands with
  `CommandContext` (strict option parsing that refuses password arguments,
  secrets from one piped line, a known `FORGE_*` variable or a terminal
  prompt with echo off through `rustix` termios, and a pool connected with
  `FORGE_MIGRATION_DATABASE_URL`), and `forge::web::report`.
- Generated (`forge new` on full-stack PostgreSQL, or
  `forge generate authentication`): `src/adapters/http/authentication.rs`
  with the handlers, `AuthenticationPorts` and the `SignInRedirect` layer;
  view models and templates; `src/infrastructure/accounts` with
  `PgTenantDirectory`, `PgPasswordResetStore` and `provision_owner`; the
  `00000000000000000003_forge_authentication` migration; `src/bootstrap/setup.rs`;
  and tests. Handlers call the existing generated mechanisms
  (`authenticate_password`, `issue_persisted_session`, `enforce_csrf`,
  cookie helpers), as `SessionTenantAccess` already does.

### Routes and behavior

| Route | Behavior |
| --- | --- |
| `GET /session/new` | organization (tenant slug, prefilled from `?tenant=`), email, password, hidden validated `return_to`; a sign-out button when signed in |
| `POST /session` | `303` to the validated return path (default `/`); `422` with one message for any wrong field; `429` with `Retry-After` |
| `DELETE /session` | session-bound CSRF check, revoke, clear the session cookie, rotate the CSRF token |
| `GET /passwords/new`, `POST /passwords` | same `303` and notice whether or not the account exists or a link was delivered |
| `GET /passwords/edit?token=`, `PATCH /passwords` | format check, `no-store`, `Referrer-Policy: no-referrer`; redeem once, revoke every session |

- **CSRF fails closed in the handler.** Every credential-changing handler
  requires `forge::views::csrf_verified`; without the layer, an exemption or
  a non-form body it answers `403`. The sign-in form's token is issued
  before any session (login CSRF).
- **Session fixation.** Sign-in issues a new server-side session and a new
  CSRF secret (ADR 0014) and sets both cookies; the form CSRF cookie becomes
  the session's CSRF token (ADR 0023), never the pre-login token; a session
  the browser already held is revoked. Sign-out and reset rotate the CSRF
  cookie with a fresh token.
- **Uniform failures and timing.** The password is verified before the
  tenant is resolved, so an unknown organization still costs one Argon2id
  hash; a wrong organization, email or password, a disabled account and an
  inactive membership all render the same `422` message.
- **Tenant selection** is an explicit organization field, as in Go: every
  sign-in names its tenant, there is no default tenant, and Forge does not
  derive the tenant from `Host` without a trusted-proxy policy. A session
  proves identity only; tenant roles are still resolved per request.
- **Throttling** counts every attempt before credentials are checked, keyed
  on the submitted identity whether or not it exists, and separately for
  reset requests and reset submissions. The default is per process; a
  persistent shared throttle is a follow-up (there was no existing
  persistent pattern to follow).
- **Reset requests are not timed by the account.** The page validates and
  queues the request on a bounded `WorkQueue` (`Composition::work_queue`,
  owned by the server's lifecycle, stopped at shutdown, full queue drops and
  logs) and answers no sooner than 100 ms; the worker looks the account up,
  issues the token and delivers it to the login as stored. Sign-in runs the
  tenant and membership lookups with random identifiers when the
  organization is malformed or unknown, and a reset submission checks the
  token before Argon2id work.
- **Reset delivery.** Forge has no mailer. The composition wires
  `UndeliverablePasswordResets`, which delivers nothing and returns
  `NotConfigured`; the failure is logged without the token and the page
  answers the same. The mailer milestone provides an adapter. Tokens expire
  after 15 minutes (Rails' default).
- **MFA** is not provided: `forge-auth` has no TOTP support yet. It is a
  follow-up, not a placeholder.

### Data

Migration `00000000000000000003_forge_authentication` adds `forge_tenants`
(UUIDv7 id from Rust, unique checked slug, display name) and
`forge_password_resets` (digest only, issuance/expiry/use timestamps, a
trigger that makes everything but a first `used_at` immutable). The runtime
role may read tenants and issue/redeem resets (`UPDATE (used_at)` only); it
cannot create tenants or principals. Existing memberships keep their
`tenant_id` without a foreign key so the migration applies to older
applications; a membership whose tenant has no `forge_tenants` row simply
cannot be signed into by slug.

### First owner: `setup`

`<app> setup --tenant SLUG --email EMAIL [--organization NAME]
[--password-stdin]` runs with the migration credential and calls
`provision_owner`: one transaction, a transaction-scoped advisory lock,
refusal once any tenant exists (so it can never add an account to, or take
over, a running installation), the tenant, the principal, an active
membership with the `owner` role, and an `auth.setup.owner_provisioned`
audit event in the same transaction. The password is never an argument,
never printed, at least 12 characters, and comes from an echo-off prompt
(typed twice), one piped line, or `FORGE_SETUP_PASSWORD` (a known
configuration key read only by `setup`).

The RBAC policy is code, not data, in Forge-Rust. The `owner` role is
therefore given every permission by the code the generators print: the
tenant-scoped scaffold wiring now grants `owner` both `<plural>:read` and
`<plural>:write` (refining ADR 0024, whose printed policy was empty) and
denies every other role until the developer grants it.

### Seeds

Every database application gets `src/bootstrap/seeds.rs` and `<app> seed`:
the application's `seed` function runs in one transaction with the migration
credential and commits only on success. Seeds must be idempotent and are not
for accounts or credentials.

### Generation and CLI

`forge new` (PostgreSQL, full-stack) generates and wires everything.
`forge generate authentication [--dry-run]` writes the same files,
byte-identical, into an older PostgreSQL application, refuses to overwrite
any of them, never edits existing files, and prints the module declarations,
the `compose` statements and the `.command("setup", ...)` line. It refuses
`--api` (HTML pages), `-d none`, MySQL, MariaDB and SQLite applications with
the reason: identities, sessions and memberships are persisted only in
PostgreSQL today (ADR 0021). There is no `forge setup` or `forge seed`
wrapper because the CLI wraps Cargo, not application commands (there is no
`forge migrate` either); `cargo run -- setup` is the development form.

## Consequences

- `forge new`, `cargo run -- migrate`, `cargo run -- setup`, `cargo run`, sign
  in, then `forge generate scaffold ... --tenant-scoped` works end to end
  without hand-written SQL; the slow scaffold test proves it against a
  running server.
- Applications must keep `CsrfProtection` in front of the pages; without it
  every credential change fails closed with `403`.
- Throttle counters are per process; multiple replicas multiply the budget
  until a shared throttle exists. The socket peer is the source, so clients
  behind one proxy share a budget.
- Password resets are not delivered until a notifier adapter is wired.
- New direct dependency: `rustix` (facade, Unix only, `termios`); `forge-auth`
  now uses `getrandom` and `sha2` (see
  [dependency policy](../dependency-policy.md)).
- Not provided: MFA, user invitation and management pages, "remember me",
  host-based tenant selection, a signed-in indicator in the layout (the
  `sessions/_sign_out.html` partial can be included where wanted), and
  identity persistence for MySQL, MariaDB and SQLite.
