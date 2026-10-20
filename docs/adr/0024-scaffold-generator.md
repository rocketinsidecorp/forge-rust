# ADR 0024: `forge generate scaffold`: printed wiring and deny-by-default access

- Status: Accepted
- Date: 2026-10-19
- Refines: [ADR 0022](0022-server-rendered-views.md) (decision 7), [ADR 0021](0021-selectable-database-backends.md), [ADR 0010](0010-session-and-tenant-authorization-context.md)
- Refined by: [ADR 0026](0026-authentication-pages-and-setup.md) (printed tenant policy grants the `owner` role; signed-out browsers redirected to sign in), [ADR 0031](0031-live-updates-over-server-sent-events.md) (`--broadcast` live updates)

## Context

ADR 0022 promises a `rails g scaffold` equivalent: one command that yields a
working, tested HTML CRUD through every hexagonal ring. Rails edits
`config/routes.rb` and serves the new controller to anyone; Forge cannot copy
either habit:

- Generated files are production code and generators must never silently
  rewrite hand-written code (AGENTS.md), but a Rust resource needs module
  declarations in five rings and composition-root lines that choose the
  repository and the access policy.
- Forge is multi-tenant and fail closed (ADR 0004, 0010). A new resource that
  is public by default, or whose tenant comes from a raw identifier, would
  ship an authorization hole with every scaffold.
- Backends differ (ADR 0021): PostgreSQL has RLS, runtime roles, session and
  membership stores; MySQL, MariaDB and SQLite have none of those yet.
- Domain and application code may not depend on SQLx, chrono or `forge::db`
  (the generated architecture test), yet a `timestamp` field and repository
  errors need types there.

## Decision

1. **Only new files, all or nothing.** The generator renders every file,
   formats the Rust ones with the application's pinned `rustfmt`, refuses if
   any target exists (or a `*_create_<plural>` migration does), and only then
   writes, removing what it wrote if a write fails. `--dry-run` prints the
   same file list and wiring without writing. Output is deterministic for a
   given resource, backend and migration version (golden fixtures in
   `crates/forge-cli/tests/fixtures/scaffold/`).
2. **Printed wiring.** The generator never edits an existing file. It prints
   the `pub mod` lines for each ring and the statements to append to the
   `compose` function of `src/bootstrap/mod.rs` (full paths, already
   formatted, so pasting keeps `cargo fmt --check` clean). `forge new`
   generates that `compose` function: `adapters::http::site` loads the site
   and its layers, `adapters::http::routes(router, &site)` adds pages, and
   scaffolds take `&site` plus the declared database.
3. **Deny by default, explicit decision to open.** Every generated handler
   first asks an access policy for `<plural>:read` or `<plural>:write`.
   `src/adapters/http/access.rs` (written by the first scaffold, reused
   after) defines `GlobalAccess`, `TenantAccess`, `DenyAll` and
   `SessionTenantAccess`.
   - Global resources are wired with `DenyAll` (`403`). Their rows are shared
     by every tenant, so tenant roles cannot authorize them; the developer
     replaces `DenyAll` with their own `GlobalAccess` (for example signed-in
     operators). That replacement is the documented decision point.
   - Tenant-scoped resources are routed under `/tenants/{tenant_id}/`. On
     PostgreSQL they are wired with `SessionTenantAccess`: the session cookie
     must name a live session, unsafe methods must present that session's CSRF
     token (ADR 0014, in addition to the form CSRF layer of ADR 0023), the
     principal must have an active membership in the route's tenant, and the
     RBAC policy must grant the permission. The printed policy is empty, so
     nothing is allowed until roles are granted; because RBAC matches
     permissions exactly, the printed example grants a role both
     `<plural>:read` and `<plural>:write`. The use cases re-check the
     `TenantContext` grant with the same rules as the routes: listing needs
     read, changes need write, and loading one record accepts either,
     because the edit and update pages load the record they change under the
     write grant (a write grant alone still cannot list or show through
     HTTP). Links are built from the authorized tenant, never from the raw
     path.
   - On MySQL, MariaDB and SQLite, which have no Forge session or membership
     persistence yet (ADR 0021 section 7), tenant-scoped resources are wired
     with `DenyAll` and the printed comment says why.
4. **Backend-specific persistence, no emulation.** The migration and
   repository are rendered for the detected backend (from the `sqlx`
   features in `Cargo.toml`). Tenant tables always carry `tenant_id` bound
   from `TenantContext` in every statement. PostgreSQL adds forced RLS with a
   policy on `forge.tenant_id`, runs each repository operation in
   `begin_tenant_transaction`, and grants the runtime role. Identifiers are
   UUIDv7 generated in Rust; `version` is the optimistic lock (`409` with the
   submitted values and the current version on a stale update); identifiers
   are quoted.
5. **Forge-owned value types.** `forge::core::Timestamp` (UTC, microseconds,
   RFC 3339 and HTML `datetime-local` text; forms render and accept whole
   seconds only, because browsers blank values with more than three
   fractional digits) and the re-exported
   `forge::core::Uuid` are the domain types of `timestamp` and `uuid` fields;
   repositories convert with chrono only inside infrastructure. Ports define
   their own typed error (`<Type>StoreError`) because the application ring
   may not use `forge::db`.
6. **Refusals.** `--api` applications are refused (the scaffold is HTML; JSON
   scaffolds are a later decision), as are `-d none` applications and
   applications missing the pieces the wiring needs (`compose`, the `forge`
   `testing` dev-dependency, `begin_tenant_transaction` for PostgreSQL tenant
   resources), each with an explanation.

## Consequences

- A scaffold is reviewable: new files plus a short printed patch the
  developer applies. The cost is a manual step per resource.
- No generated route is reachable until someone decides who may use it;
  tenant data is isolated by predicates everywhere and by RLS on PostgreSQL.
- Global resources need application code before they serve anyone. Tenant
  resources on non-PostgreSQL backends need session and membership stores.
- Generation requires `rustfmt` (a pinned toolchain component) and fails
  closed without it.
- Generated tests cover domain rules, the HTML flow with an in-memory port
  (CSRF refusal, `422`, `303` with flash, `404`, `409`) and the repository
  against `TestDatabase`, including cross-tenant isolation and, on
  PostgreSQL, RLS with a role that cannot bypass it.
