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

Phase 4 implementation: `forge-jobs` and `forge-events` run their PostgreSQL
suites as a per-test non-superuser, non-owner runtime role
(`forge_jobs::testing::RlsTestDatabase`); they are `#[ignore]`d unless
`FORGE_TEST_DATABASE_URL` is set and `--include-ignored` is passed. The cron
evaluator is checked against a minute-by-minute oracle on generated
expressions and against arbitrary input that must never panic. Concurrency
keys, per-tenant fairness and saturation tests remain open.

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

The generated sign-in pages ([ADR 0026](adr/0026-authentication-pages-and-setup.md))
ship `tests/authentication_http.rs` (the pages through the real CSRF and
flash layers with in-memory ports: success with fresh session and CSRF
token, one uniform failure for every wrong field, throttling with
`Retry-After`, CSRF refusals, open-redirect return paths, sign-out, session
fixation, reset non-enumeration, single-use and expired reset links, sessions
revoked on reset, the sign-in redirect) and `tests/authentication_postgres.rs`
(`setup` refusing password arguments and short passwords, creating one owner
and refusing afterwards, concurrent `setup` runs creating exactly one tenant,
and reset redemption against PostgreSQL). With `FORGE_TEST_DATABASE_URL` the
slow scaffold test also runs `setup` and `seed` and drives a browser session
over HTTP against the running server: sign-in redirect, sign-in, tenant
scaffold pages, sign-out, revoked session; a second slow test restores the
pages with `forge generate authentication`. `forge-cli` unit tests check that
the generator's files are byte-identical to `forge new`'s and its refusals.

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
coverage is measured and gated as described in [Coverage](#coverage).

CI retains test reports, failing minimized property cases, fuzz crash inputs,
OpenAPI/generator diffs, dependency reports, and benchmark raw data. A quality gate
is reported as passed only when the recorded command exits successfully.

See [architecture](architecture.md), [performance targets](performance-targets.md),
and [dependency policy](dependency-policy.md).

## Database tests in CI

The PostgreSQL, MySQL and MariaDB tests are `#[ignore]`d so that a plain
`cargo test` needs no server. CI's `database` job runs them all,
`scripts/test-databases.sh` (`cargo test --workspace --all-targets
--all-features --locked -- --include-ignored`, the generated-application
suite `forge-cli --test scaffold` excepted, which the `scaffold` job runs),
twice: against PostgreSQL 18 and against PostgreSQL 16, each time with
MySQL 8.4 and MariaDB 11.8 and a real `sshd` (`FORGE_TEST_REQUIRE_SSHD=true`).
Locally:

```sh
FORGE_TEST_DATABASE_URL=postgres://postgres:postgres@127.0.0.1:5432/postgres \
FORGE_TEST_MYSQL_URL=mysql://root:forge@127.0.0.1:3306/mysql \
FORGE_TEST_MARIADB_URL=mariadb://root:forge@127.0.0.1:3307/mysql \
scripts/test-databases.sh
```

An ignored test that needs something CI cannot provide must skip on its own
environment variable with a reason (as `write_rust_values_for_go` does without
`FORGE_INTEROP_DIR`), so `--include-ignored` stays green; it is never deleted
or weakened.

## Coverage

CI's `coverage` job measures line coverage of the framework crates with
[cargo-llvm-cov](https://github.com/taiki-e/cargo-llvm-cov) and fails when it
drops below the committed floor in `.coverage-floor`:

```sh
rustup component add llvm-tools-preview
cargo install cargo-llvm-cov --version 0.9.1 --locked
scripts/coverage.sh                                  # measure, print the table, gate
COVERAGE_JSON=summary.json scripts/coverage.sh       # report and gate an existing summary
```

- **How it is measured.** The test set of the `database` job
  (`scripts/test-databases.sh llvm-cov --no-report`, the ignored database
  tests included) against PostgreSQL 18, MySQL and MariaDB, then
  `cargo llvm-cov report`. The job writes the total and a per-crate table to
  the run's summary and uploads `target/coverage/summary.json` and the
  line-level `target/coverage/lcov.info` (`DA:<line>,<hits>` per line of each
  file; a line with `0` hits is what no test reaches).
  cargo-llvm-cov builds into its own `target/llvm-cov-target`, a second full
  build: run it locally only with the disk to spare.
- **Two totals.** The script prints both. The *gross* total counts everything
  measured except the exclusions below. The *gated* total, the one the floor
  checks, also leaves out the contract test kits listed under "Contract test
  kits". `target/coverage/summary.json` is the gross measurement; the per-crate
  table shows both.
- **Pinned tools.** cargo-llvm-cov 0.9.1 (2026-09-06, MSRV 1.87, not
  yanked: the newest release at least two weeks old that builds with the
  pinned Rust 1.98.1), installed `--locked` through
  `.github/actions/cargo-tool`, cached under its exact version; llvm-tools
  come from the pinned toolchain.
- **The floor only rises.** It starts at 0 until the first CI run reports
  the real number, is then set to that number rounded down to 0.1, and is
  raised, never lowered, as tests are added, toward the project's target of
  95%. Raise it in the pull request that raises coverage.
- **Exclusions.** Only these, each for the reason given; nothing else may be
  excluded, and a new exclusion needs a reason here:

  | Excluded | Why |
  | --- | --- |
  | `forge-bench`, `forge-loadgen` | The benchmark and load-generation harnesses: they measure the framework, they are not part of it. `fuzz/` is outside the workspace. |
  | `crates/forge-cli/templates/` | Generated-application templates: text that `forge new` renders, not code compiled into the framework. Their output is tested by the `scaffold` job. |
  | `forge-cli --test scaffold` | The generated-application suite builds separate applications in other target directories; it runs in the `scaffold` job. |
  | `tests/` directories, `tests.rs`, `*_tests.rs`, `test_support.rs` | Test code (cargo-llvm-cov's default for `tests/`; the others are `#[cfg(test)]` modules kept in their own files). |

  The `testing` modules behind the `testing` features are shipped API and stay
  in the gross total; only the contract test kits below leave the gated one.

### Contract test kits (decision D7)

Coverage measures production code. These `src/testing.rs` files are harnesses
that every backend's tests call; their only code that does not run is the
failure branch of an assertion (a `panic!` or `assert!` when a backend breaks
the contract), which cannot run while every backend passes. They stay verified
by being run against every real backend (PostgreSQL 18 and 16, MySQL, MariaDB,
SQLite) in CI, where a broken kit or a broken backend fails the suite. They
are excluded from the gated total only, and still appear in the gross one:

`crates/forge-cache/src/testing.rs`, `crates/forge-db/src/testing.rs`,
`crates/forge-jobs/src/testing.rs`, `crates/forge-mail/src/testing.rs`,
`crates/forge-ratelimit/src/testing.rs`, `crates/forge-storage/src/testing.rs`,
`crates/forge-telemetry/src/testing.rs`.

The list is explicit in `scripts/coverage.sh` (`kits`), exactly those files and
never a pattern over `testing`, so production code cannot be hidden by a naming
accident. A new kit is added to the list, with its reason, in the pull request
that adds it.

The Forge-Go twin (`scripts/coverage.sh`, `.coverage-floor`, the same job
name) measures statement coverage with `go test -coverpkg`; see its
`docs/TESTING.md`.

## Live updates tests

`forge-broadcast` (ADR 0031) mirrors the Forge-Go negative tests:

- **Tokens:** round trips, byte-for-byte equality with tokens Forge-Go
  printed, forged/tampered/foreign-key/oversized/padded tokens invalid, expiry
  at the exact second (an expired forgery stays invalid), key rotation,
  `FORGE_STREAMS_KEY` required in production only and never echoed.
- **Handler:** one identical `403` for every invalid token, `401`/`403`/`503`
  from the access policy, another tenant (member, principal or anonymous)
  refused, per-principal and per-tenant caps with `Retry-After`, anonymous
  viewers per address with IPv6 `/64` and IPv4-mapped IPv6, a full broker,
  shutdown ending open streams, heartbeats and the maximum age, a slow
  subscriber dropped, SSE framing that cannot be broken out of.
- **Transport:** `forge-http` sends streamed bodies chunked, disconnects a
  real client that stops reading, keeps a slow reader that pauses for ten
  request timeouts (the 30 second stalled-write floor); a real server drains at once when shutdown
  begins although a stream is open (`crates/forge/tests/broadcast.rs`).
- **PostgreSQL fanout** (`--include-ignored` with `FORGE_TEST_DATABASE_URL`):
  two brokers on one database deliver once to each replica (multi-chunk
  messages included), oversized messages are refused, `publish_in` delivers
  only on commit, malformed and foreign notifications are ignored, and a
  terminated listener connection is re-established. Non-PostgreSQL backends
  are refused (`postgres_fanout_is_refused_on_sqlite_not_emulated`).
- **Generated applications:** `forge generate scaffold --broadcast` output
  is golden-tested; the generated tests cover the broadcasts, forged/unknown/
  unauthorized subscriptions, tenant isolation and the production key; the
  scaffold end-to-end test opens a real stream on the running PostgreSQL
  application (`401` signed out, `200` signed in) and receives a create.

## Rate limiting, cache and storage tests

Each of `forge-ratelimit`, `forge-cache` and `forge-storage` ships its
conformance suite behind a `testing` feature so a new store passes the same
guarantees, plus negative tests that mirror the Forge-Go reviews:

- **Rate limiting:** the limit and remaining count, independent keys, window
  reset, retry-after, 160 concurrent hits allowing exactly the limit, invalid
  keys, pruning of expired buckets only; bounded memory under a flood of distinct
  keys (`EvictSoonest`, `Deny`); `429` with `Retry-After`, HTML for browsers,
  `503` when the limiter fails and only `fail_open` lets requests through;
  spoofed `X-Forwarded-For`/`X-Real-IP`/`Forwarded` buying no fresh bucket; IPv6
  `/64` keying; database pruning bounded per call and under 1500 hits of new,
  instantly expiring keys.
- **Cache:** the store contract (copies, binary and empty values, expiry,
  overwrite, default ttl, invalid keys/values/ttls, entry bound), 24 concurrent
  misses computing once, a cancelled or panicking leader never failing its
  waiters, errors never cached, store outages degrading to a miss without logging
  keys or values, length-prefixed key collisions (namespace included), tenant
  keys, undecodable JSON as a miss, keys stored only as hashes, and the entry bound
  after 2000 writes of new keys.
- **Storage:** key traversal impossible by construction, symlinks planted below
  and in place of storage directories, a symlinked metadata file, atomic writes
  and `tmp/` hygiene, 0600/0750, tampered/expired/duplicated/percent-encoded/
  forged/bound/rotated signatures with the store never reached, active content and
  polyglots served as sandboxed downloads, header injection through stored
  metadata, disguised types refused on the sniffed bytes, the body cap, the
  repeated text-field budget, cleanup after a store failure, and a strict
  multipart parser table plus a differential/mutation test.

The SQLite suites always run. PostgreSQL, MySQL and MariaDB suites are
`#[ignore]`d unless requested with `-- --include-ignored` and
`FORGE_TEST_DATABASE_URL`, `FORGE_TEST_MYSQL_URL` or `FORGE_TEST_MARIADB_URL`
(the same convention as `forge-db`).

## Client address tests

The trusted-proxy resolver (ADR 0032) mirrors the Forge-Go ADR 0037 tests:

- **Configuration:** empty by default; masked parsing; the `/8` and `/16`
  minimums accepted; empty, malformed, zoned, IPv4-mapped, `/0` and too broad
  entries refused by position without echoing the value, also through
  `TrustedProxy::new` and deserialization.
- **Resolver:** the Forge-Go resolution table (untrusted peers, appending
  proxies, trusted chains over several header lines, malformed entries,
  zones, IPv4-mapped forms), the 32-hop limit, `X-Real-IP` and `Forwarded`
  never read, and a deterministic generated property test (no proptest
  dependency: an xorshift generator over 20 000 chains) asserting the result
  is always the peer or an entry of the header.
- **Server and consumers:** a real server resolves before its first layer and
  ignores headers from untrusted peers; `by_client_ip`, the stream cap and a
  sign-in throttle over TCP (`crates/forge/tests/client_ip.rs`) give two
  clients behind one proxy independent counters, keep forged prefixes and
  direct forgers in their own counters, and group IPv6 by `/64`; the
  generated `clients_behind_one_trusted_proxy_have_their_own_sign_in_counters`
  does the same through the real sign-in pages.

## Telemetry tests

`forge-telemetry` (ADR 0033) tests the port against Forge-Go's behavior:

- **Configuration:** every `FORGE_TELEMETRY_*` key and Go duration, the
  production rules (https unless plaintext is allowed, no `log` exporter,
  loopback metrics unless confirmed public), header and endpoint values never
  echoed and redacted in `Debug` and serialization, disabled telemetry not
  validated further.
- **Propagation:** strict `traceparent`/`tracestate` parsing with a
  deterministic fuzz loop; incoming context ignored unless trusted, over real
  connections (`crates/forge-http/tests/telemetry.rs`).
- **Export** against an OTLP receiver (`forge_telemetry::testing::Receiver`,
  a hyper server): span, metric and log JSON documents field by field, gzip
  and secret headers, retries on 503 and none on 400, a full queue dropping
  without blocking, a shutdown bounded by its deadline against a collector
  that never answers, and a failing log export that does not feed itself
  (its own binary, with a global subscriber).
- **HTTP:** route templates and empty routes as labels (raw paths and queries
  never appear), requests answered by a layer, a panicking handler recorded
  as 5xx with the active gauge back to zero, spans named after the template
  and log records carrying the server span's ids.
- **Jobs and outbox** (`--include-ignored`): outcomes counted by job name
  and event type, panicking attempts included, payloads never exported.
- **Catalogue:** `tests/catalogue.rs` holds the metrics shared with Forge-Go
  (name, kind, unit, labels, Prometheus name), requires every other Go metric
  to be explained in `docs/observability.md`, and every registered metric to
  be documented there.
- **End to end:** the generated-app script scrapes the separate metrics
  listener, checks route-template labels, that the application port never
  serves `/metrics`, and that an OTLP endpoint error does not echo it.
- **Local stack** (`crates/forge-cli/tests/observability.rs`, ADR 0034): the
  overlay and `observability/` files are generated for every backend and
  shape (world-readable), parse as YAML/JSON, publish only loopback ports,
  pin images by version and digest, require the Grafana password with no
  default and hold no secret; the default `compose.yaml` stays free of the
  stack; the app's overlay environment loads through `AppConfig::from_vars`
  as the production image sees it; collector, Tempo, Loki, Prometheus and
  Grafana agree on hosts and ports; every metric the dashboard and data
  sources query is in the documented catalogue; YAML-keyword application
  names stay strings. The Docker path of the end-to-end script runs
  `docker compose config` on the overlay and smoke-tests the running stack
  (Grafana login and data sources, dashboard, metrics in Prometheus, traces
  in Tempo, logs in Loki).

## Deployment tests

`forge deploy` ([ADR 0042](adr/0042-forge-deploy.md)) is tested at three
levels, without a Docker daemon:

- **Unit and golden** (`crates/forge-cli/src/deploy/tests.rs`): configuration
  validation including option, `;`, `$(...)` and newline injections; the
  exact ssh argv and remote command text; quoting round-tripped through a real
  `sh`; and the dry-run plans of deploy, rollback, prune and setup compared
  byte for byte with Forge-Go's golden files
  (`crates/forge-cli/tests/fixtures/deploy/*.golden`, copied from Forge-Go;
  `FORGE_BLESS=1` rewrites them only after review). Failure paths (unhealthy
  host, failed revert, interruption, failed migration, failing hook, held
  lock) assert their exit codes against a recording runner.
- **Real SSH** (`crates/forge-cli/src/deploy/ssh_tests.rs`): two private
  `sshd` instances and the system `ssh` client, with a docker stand-in
  (`crates/forge-cli/tests/support/fake_docker.rs`, built by `cargo test`)
  first on each session's PATH: setup, two deploys, a failed migration, a
  health failure on one host reverting both, lock contention, rollback,
  pruning, logs with redaction and with `--follow` cancellation (the remote
  `docker logs` processes must exit), `app exec`, details, a dry run that
  changes nothing, and no secret in any docker argv. It is skipped with a
  notice without `sshd`, `ssh`, `ssh-keygen` or the stand-in;
  `FORGE_TEST_REQUIRE_SSHD=true` makes that a failure.
- **forge-proxy over sockets** (`crates/forge-proxy/tests`): switches under
  concurrent load with zero failed requests, health gating, draining and
  drain timeouts, routing, forwarding headers, body limits and slow bodies,
  upgrades, the control socket and state persistence, the command line, and
  HTTPS with an `openssl`-made CA plus a full ACME HTTP-01 issuance against an
  in-test RFC 8555 server (skipped with a notice without `openssl`).
