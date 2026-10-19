# Test Strategy

Status: Phase 0 quality contract. Commands become executable as their tools and
crates are introduced; this document does not claim that a test suite exists yet.

## Objectives

Tests must prove externally observable behavior, architecture boundaries, security
properties, data consistency, lifecycle behavior, and compatibility. Tests should
be deterministic, parallel-safe, and smallest-scope first. A production defect
normally receives a regression test that fails before the fix.

## Test layers

| Layer | Purpose | Typical mechanism |
| --- | --- | --- |
| Unit | Domain rules, value objects, policy decisions, parsers, retry math | in-crate `cargo test`, table tests |
| Compile/architecture | Dependency direction, public contracts, useful macro errors, typestate restrictions | Cargo metadata checks, compile-pass/fail fixtures |
| Property | UUIDv7, cursor, validation, idempotency, serialization round trips | generated inputs with persisted minimal failures |
| Fuzz | Hostile protocol, path, URL, webhook, schema, stream, and generator input | coverage-guided fuzz targets with seed corpus |
| Contract | All implementations of a port obey identical semantics | reusable suites from `forge-testing` |
| Integration | HTTP, PostgreSQL/RLS, migration, job/outbox, telemetry, providers | isolated real dependencies; transactional cleanup or unique schemas |
| End-to-end | Generated app and `forge` workflows from process boundary | build/run executable and exercise public protocol |
| Security | Abuse cases and fail-closed behavior from the threat model | negative integration, fuzz, static/dependency scans |
| Performance | Budgets, saturation, leak and tail-latency behavior | pinned benchmark environments and raw artifacts |

Mocks are appropriate for deterministic application orchestration, but they do not
replace contract tests against each concrete adapter or real-protocol integration.

## Architecture verification

CI parses `cargo metadata` for forbidden package edges and scans/import-compiles
generated application fixtures for forbidden ring imports. Compile-fail fixtures
prove, at minimum, that:

- domain cannot import adapter, HTTP, SQLx, provider SDK, filesystem, or bootstrap
  APIs;
- application cannot depend on concrete HTTP/database/provider implementations;
- one entity's typed ID cannot be passed as another entity's ID;
- tenant-sensitive repositories cannot be called without `TenantContext`;
- validated AI tool intent is not directly executable without an authorized plan.

The fixture generator runs before architecture checks so templates cannot diverge
from the constraints applied to handwritten examples.

## Required subsystem coverage

### Core, configuration, and lifecycle

Test source precedence, unknown keys, secret redaction, validation-before-start,
signal handling, cancellation propagation, bounded task supervision, readiness
transitions, drain deadlines, and child-task failure policies. Use paused/controlled
time where possible instead of sleeps.

### HTTP and contracts

Test routing, middleware order, extraction, limits, content type, malformed JSON
and form bodies, validation, error redaction/status mapping, request IDs,
deadlines, CORS, CSRF (missing, wrong, repeated, malformed, query-string and
cross-site tokens; method override only after verification), flash, CSP,
compression, streaming
backpressure, disconnect cancellation, SSE/WebSocket limits, and graceful shutdown.
The same endpoint declaration must produce runtime validation and matching OpenAPI;
contract snapshots are reviewed, not blindly rewritten.

### UUIDv7 and persistence

Use published UUIDv7 vectors plus property tests for version/variant, canonical
round trip, sorting at different timestamps, monotonic same-millisecond behavior,
clock rollback, and deterministic helpers. Database tests use supported PostgreSQL,
real migrations, constraints, indexes, transaction rollback, advisory-lock
serialization, optimistic conflicts, cursor boundaries, and query plan assertions
for critical paths.

Every tenant-sensitive repository runs a shared two-tenant isolation suite with
missing, invalid, and stale context plus pooled-connection reuse. The test role
must not bypass RLS. The generated PostgreSQL E2E additionally proves that the
runtime role cannot create schema objects, sees zero tenant rows when transaction
context is absent, sees only the selected tenant after transaction-local
tenant/principal settings are installed, and cannot write another tenant's row.

### Authentication and security

Test password/session lifecycle, Argon2id parameter selection and malformed PHC
handling, policy-upgrade detection and optimistic rehash conflicts, generic
credential denial, bounded password input, bearer/CSRF entropy
and independence, digest-only persistence, cookie flags, session revocation and
rotation, CSRF safe/unsafe method behavior, RBAC deny-by-default, role changes,
brute-force/rate limits, CORS, SSRF redirect and address cases, path
traversal/symlinks, upload limits, security headers, key rotation, webhook
signatures, replay windows, idempotency digest mismatch, audit completeness, and
all fail-closed rules in the [threat model](threat-model.md). Database-enabled
generated-app tests additionally prove the runtime audit writer can append one
event but cannot SELECT, UPDATE, or DELETE audit history.

### Jobs and events

Test transactional enqueue/outbox behavior, no publication before commit,
at-least-once duplicate delivery, idempotent consumers, retry jitter within bounds,
attempt ceilings, dead letters, scheduled-time semantics, worker crash/reclaim,
heartbeat expiry, cancellation, concurrency keys, fairness, and saturation.

### Telemetry

Capture an in-memory/export test sink and assert trace linkage across HTTP,
application, database, jobs/events, external calls, and AI. Test redaction and
cardinality rules. Exporter failure must not crash normal request processing or
create unbounded memory growth.

### AI providers and execution

Each provider adapter runs a shared contract suite for request mapping, streaming,
tool calls, structured output, cancellation, timeouts, rate-limit classification,
token accounting, retries, circuit breaker, and health. Recorded protocol fixtures
must contain no secrets and complement, not replace, opt-in live smoke tests.

Execution tests prove immutable plan binding, expiry, single-use replay rejection,
capability denial, path and argv validation, environment sanitization, network
denial, resource/output limits, approval binding, audit on success/denial/failure,
and fail-closed behavior when the host cannot enforce a requested sandbox profile.

### CLI and generators

Exercise commands as subprocesses. Golden fixtures verify deterministic trees and
content on Linux, macOS, and Windows where supported. Test invalid field grammars,
name collisions, path traversal, partial-write recovery, refusal to overwrite,
formatter failure, useful diagnostics, and that a generated app formats, checks,
tests, builds, migrates, starts, serves a route, and shuts down. Full-stack
apps additionally serve the HTML page with the CSP, the CSRF cookie and meta
tag and the digested Turbo/Stimulus modules, refuse a form `POST` without the
page's token (`403`) and accept it with the token; the e2e script checks this
over HTTP and runs the quality gates of an `--api` application.

`forge generate scaffold` has golden fixtures
(`crates/forge-cli/tests/fixtures/scaffold/`, regenerated with `FORGE_BLESS=1`),
unit tests for name validation, refusals, `--dry-run`, all-or-nothing writes
and formatter failure, and a slow end-to-end test
(`cargo test -p forge-cli --test scaffold -- --ignored`) that generates
PostgreSQL, SQLite and MySQL applications, scaffolds a global and a
tenant-scoped resource, applies the printed wiring and runs the
applications' `cargo fmt --check`, Clippy and tests. With
`FORGE_TEST_DATABASE_URL` it also runs the PostgreSQL repository and
row-level-security tests and the scaffold migrations' `migrate`/`rollback`.
Every scaffold ships tests: domain validation, the HTML flow through the real
CSRF/flash/method-override layers with an in-memory port (CSRF refusal, `422`,
`303`, `404`, `409`, the `DenyAll` default) and the repository against
`TestDatabase` (optimistic locking, cursor pages, cross-tenant isolation and,
on PostgreSQL, RLS with a role that cannot bypass it).

Phase 1 additionally builds every generated application's multi-stage Dockerfile,
inspects the final image for non-root identity and forbidden build contents, starts
it with the documented configuration, waits for the image health check, exercises
HTTP, sends the container stop signal, and verifies graceful termination. A
read-only-root-filesystem run is required unless an application feature explicitly
declares a writable mount. `scripts/e2e-generated-app.sh` implements this Phase 1
container E2E, along with the host-binary checks (build, start, health, the
`healthcheck` subcommand, SIGTERM exit status). CI runs it, and
`FORGE_E2E_SKIP_DOCKER=1` limits it to the host checks. Phase 2 repeats the flow with the generated PostgreSQL
Compose development profile, validates health-based startup, migrations,
persistence across application restart, and teardown without treating Compose as a
production configuration. Phase 3 extends the database path with distinct
migration/runtime credentials and negative RLS checks: migration commands reject
runtime-only configuration, the serving process receives only the runtime URL,
the runtime role has no DDL authority, missing tenant settings deny visibility,
and two-tenant reads/writes remain isolated. The same generated-app path verifies
that Forge identity migrations create principal/session/membership storage, bearer
and CSRF digests are persisted without raw secrets, session mutation is limited to
monotonic revocation, membership state persists independently from session
credentials, and runtime password rehash updates are restricted to hash metadata
with stale versions rejected.

## Concurrency correctness

Rust prevents data races in safe code but not deadlocks, lost wakeups, task leaks,
bad cancellation, or semantic races. Tests use controlled schedulers/models for
small synchronization components where appropriate, randomized stress tests,
worker kill/restart scenarios, and leak checks after shutdown. Thread sanitizer or
Miri jobs are added for supported targeted code; neither replaces behavioral
concurrency tests.

## Test data and isolation

- Tests generate unique UUIDv7 values and tenant namespaces; ordering tests use an
  injected clock/entropy source.
- Database integration uses disposable PostgreSQL or unique databases/schemas and
  applies migrations from zero. Shared developer databases are not accepted.
- No test contacts the public internet by default. Live provider tests are
  separately named, credential-gated, spend-capped, and non-blocking for ordinary
  contributors unless release policy requires them.
- Secrets and production data never enter fixtures, recordings, snapshots, or
  failure artifacts.

## Quality gates by change

The canonical commands will be wrapped by `forge` but remain directly runnable:

1. Every change: `cargo fmt --check`, workspace compile/check, targeted tests,
   Clippy with warnings denied for project code, architecture checks.
2. Dependency changes: locked metadata, policy/license/advisory checks, MSRV, and
   dependency graph review.
3. Database/security/AI changes: affected contract/integration/security suites and
   threat-model review.
4. Main/nightly: all features/targets matrix, broader integration, fuzz smoke,
   Miri/model tests where supported, docs, and benchmark signal.
5. Release: clean generated-app E2E, supported platforms, migration upgrade path,
   prolonged fuzz/soak, performance budgets, SBOM/provenance/signature, minimal
   container scan, and production checklist.

Feature combinations are selected deliberately to avoid combinatorial explosion:
minimal defaults, each leaf feature, supported production bundles, and all features.
Optional features may not become permanently untested.

## Flakes, coverage, and evidence

Flaky tests are defects. Quarantine requires an owner, linked issue, expiry, and a
separate non-blocking job; retries do not turn a red test green. Timeouts capture
task, thread, log, and dependency diagnostics without secrets.

Coverage identifies blind spots but is not a correctness proxy. Critical policy,
tenant isolation, migration, execution, and error branches require explicit tests;
the project will establish a measured baseline before setting a numeric line target.

CI retains test reports, failing minimized property cases, fuzz crash inputs,
OpenAPI/generator diffs, dependency reports, and benchmark raw data. A quality gate
is reported as passed only when the recorded command exits successfully.

See [architecture](architecture.md), [performance targets](performance-targets.md),
and [dependency policy](dependency-policy.md).
