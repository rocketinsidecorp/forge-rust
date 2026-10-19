# Forge Architecture

Status: Phase 0 design baseline complete; Phase 1 implemented, unreleased.
Phase 2 behavior is implemented through PostgreSQL generation, reversible
migrations, transaction/repository contracts and optimistic locking, but the
GitHub Actions runner gate remains blocked before job steps execute. Phase 3 is
in progress: server-side session lifecycle, deny-by-default RBAC, explicit
membership and authorized TenantContext contracts, least-privilege PostgreSQL/RLS,
digest-only persistent sessions, Argon2id password verification, opaque
session/CSRF secrets, secure cookie helpers, and append-only audit persistence
are implemented, including optimistic Argon2id rehash-on-policy-upgrade after a
successful login. Login throttling, turnkey HTTP auth route composition, and
stronger audit integrity/retention controls remain open.

Forge is an opinionated Rust application framework for long-lived services. Its
value is the integration of explicit application architecture, secure defaults,
operations, and developer tooling. It is not an alternate HTTP executor or ORM.
Mature ecosystem crates implement mechanisms; Forge defines their lifecycle,
contracts, composition, and generated application shape.

## Design goals

1. Preserve domain independence through enforceable ports-and-adapters boundaries.
2. Make tenant, authorization, transaction, cancellation, and observability
   context explicit at application boundaries.
3. Deliver useful executable software in vertical phases instead of publishing a
   broad set of empty crates.
4. Keep generated code readable and editable; macros remove repetition but do not
   hide request or data flow.
5. Default to UUIDv7 identifiers, bounded concurrency, fail-closed authorization,
   and typed errors.
6. Permit replacement of infrastructure adapters without exposing vendor SDKs to
   application or domain code.
7. Generate a production-oriented Docker path for every application rather than
   treating containers as an optional deployment afterthought.

Non-goals for the initial releases are a general-purpose ORM, a new async runtime,
a new cryptographic implementation, transparent distributed transactions, and
source compatibility with Rails or another Rust web framework.

## Application architecture

Generated applications use four dependency rings and a composition root:

```text
external request / message / schedule
                 |
                 v
adapters --> application --> domain
    ^             |
    |             v
infrastructure implements application ports

bootstrap composes every concrete implementation and owns process lifecycle
```

- `domain` contains entities, value objects, domain services, domain events, and
  domain errors. It may depend only on Rust's standard library and explicitly
  approved foundational value crates. It does not know HTTP, databases, queues,
  filesystems, cloud vendors, or AI providers.
- `application` contains commands, queries, use cases, and ports. It depends on
  `domain`, never on concrete adapters. A use case owns orchestration and the
  transaction boundary.
- `adapters` translate external protocols into application contracts and map
  application results back to those protocols. HTTP handlers remain thin.
- `infrastructure` owns concrete database, cache, queue, telemetry, secret, and
  security mechanisms. Concrete implementations satisfy application ports.
- `bootstrap` is the only application module allowed to select implementations,
  read process configuration, and construct the dependency graph.

The generated source layout follows the layout in the project brief. Within a
crate, visibility defaults to private or `pub(crate)`; public APIs are intentional.
Cross-ring calls use narrow traits and data transfer types rather than importing
adapter types.

### Enforced dependency direction

Architecture checks will inspect each generated application's Cargo metadata and
source imports. CI must reject these edges:

| Source | Forbidden dependency |
| --- | --- |
| domain | application, adapters, infrastructure, bootstrap, protocol/provider crates |
| application | adapters, infrastructure, bootstrap, HTTP/database/provider SDK types |
| adapters | bootstrap and unrelated adapter implementations |
| infrastructure | bootstrap and adapter request/response types |

Some protocol crates have feature graphs that can smuggle dependencies into the
domain. Therefore checks cover both module imports and resolved Cargo packages.
See [ADR 0002](adr/0002-hexagonal-boundaries.md).

## Planned workspace

Crates are added only when a vertical phase uses them:

```text
forge/
├── crates/
│   ├── forge               # facade: `forge::App` and re-exports (Phase 1)
│   ├── forge-core          # lifecycle, contexts, errors, typed IDs
│   ├── forge-config        # typed configuration and secret references
│   ├── forge-http          # protocol facade, middleware, streaming
│   ├── forge-router        # route declaration and introspection
│   ├── forge-db            # PostgreSQL ports and transaction context
│   ├── forge-migrations    # migration runner and metadata
│   ├── forge-security      # policy/capability primitives
│   ├── forge-auth          # authentication/session adapters
│   ├── forge-tenancy       # tenant context and RLS integration
│   ├── forge-jobs          # job contracts and PostgreSQL adapter
│   ├── forge-events        # events, outbox, consumer contracts
│   ├── forge-cache         # cache port and adapters
│   ├── forge-storage       # object storage port and adapters
│   ├── forge-telemetry     # OpenTelemetry lifecycle and context
│   ├── forge-openapi       # contract/schema projection
│   ├── forge-ai            # provider and controlled execution ports
│   ├── forge-cli           # `forge` executable
│   ├── forge-testing       # fixtures and contract test suites
│   └── forge-macros        # narrowly scoped derives/procedural macros
├── examples/               # executable applications added with features
├── tests/                  # workspace architecture and system tests
└── docs/
```

Splitting a crate requires a stable responsibility, an independent dependency or
feature boundary, and an actual consumer. Platform facades may use mature crates
internally without leaking those types into Forge's stable API.

Every generated application also includes `.dockerignore` and a secure multi-stage
`Dockerfile`. The build stage uses the pinned Rust toolchain and locked dependency
graph; the runtime stage contains only the non-root application, required runtime
libraries, and trust certificates. It exposes the configured application port,
defines a liveness health check, receives configuration through the same documented
environment mapping as bare-metal execution, and runs the application binary
directly. Build-time secrets must use ephemeral secret mounts and never enter a
layer or build argument. See [ADR 0008](adr/0008-docker-by-default.md).

## Core public contracts

The examples below express API direction; exact names stabilize through vertical
implementation and compatibility review.

### Lifecycle and cancellation

```rust
pub trait Application: Send + Sync + 'static {
    type Error: std::error::Error + Send + Sync + 'static;

    async fn start(&self, shutdown: ShutdownToken) -> Result<(), Self::Error>;
}
```

Bootstrap creates a root cancellation token, starts registered services under a
supervisor, stops accepting new work on shutdown, drains work up to a configured
deadline, and then cancels remaining tasks. Task spawning is exposed through a
supervisor that requires a name and shutdown behavior. Detached tasks and
unbounded channels are not part of the public API.

### Typed identifiers and errors

Generated entities use nominal IDs whose wire and persistence representation is
UUIDv7. ID parsing validates UUID version 7 by default; importing legacy IDs must
be an explicit compatibility path.

```rust
pub struct UserId(uuid::Uuid);

pub enum DomainError {
    Validation(ValidationError),
    NotFound,
    Conflict,
    Unauthorized,
    Forbidden,
    Infrastructure(InfrastructureError),
}
```

Errors expose stable machine code, category, safe client message, retryability,
and bounded metadata. Error sources and internal details are logged, not returned
to clients. HTTP status mapping belongs to the HTTP adapter.

### Request and use-case flow

```text
route match -> request limits -> authentication -> tenant resolution
-> authorization -> validation -> command/query -> use case
-> transaction/port -> response mapping -> telemetry
```

Use cases accept typed input and an explicit request context. The context carries
request/correlation identity, deadline/cancellation, authenticated principal, and
tenant scope where required. It must not become an untyped service locator.

### Authentication, authorization, and tenant context

Authentication, membership resolution, and authorization are separate proofs.
A server-side session establishes a principal identity with explicit issuance,
expiry, revocation, and rotation semantics. Tenant roles are not embedded in the
session; they are loaded from the tenant membership so suspension and role
changes do not wait for credential expiry.

RBAC is deny-by-default. Roles and permissions use validated application-defined
names. A successful policy decision produces an AuthorizationGrant that
application code cannot manufacture directly.

TenantContext is derived only when an authenticated principal matches an active
membership and the tenant-scoped RBAC policy explicitly grants the requested
permission. Tenant-sensitive repository contracts accept TenantContext, never a
bare tenant identifier. Generated PostgreSQL infrastructure consumes that context
by installing tenant/principal values as transaction-local settings for RLS.
See [ADR 0010](adr/0010-session-and-tenant-authorization-context.md) and
[ADR 0011](adr/0011-postgresql-runtime-migration-roles-and-rls-context.md).

Generated database applications persist principal credentials and sessions through
Forge-owned contracts. Raw bearer and CSRF secrets never enter persistence:
`forge_sessions` stores fixed 256-bit digests, while
`PostgresSessionStore` performs lookup, revocation, CSRF binding, and atomic
rotation. Membership roles remain outside the session and are loaded from
`forge_tenant_memberships` so changes can take effect independently of session
expiry. See
[ADR 0013](adr/0013-persistent-principals-sessions-and-memberships.md).

Concrete authentication mechanisms remain in generated infrastructure. Passwords
use Argon2id on Tokio blocking workers; bearer and CSRF tokens are independent
256-bit OS-random values; persistence lookup uses SHA-256 digests; the bearer is
serialized in a Secure/HttpOnly/SameSite=Lax `__Host-forge_session` cookie; and
unsafe cookie-authenticated HTTP methods require the independent
`x-csrf-token` value. Public login denial does not reveal whether an account is
missing, disabled, or has the wrong password. Successfully verified passwords
whose PHC parameters no longer match policy are rehashed and updated with an
optimistic credential version; a concurrent password change/disable wins and the
login fails closed rather than overwriting it. See
[ADR 0014](adr/0014-argon2-session-cookie-and-csrf.md).

Full-stack generated applications protect every unsafe request, signed in or
not, with the `forge-views` CSRF layer: a 256-bit token in a host-only
`__Host-forge_csrf` cookie that pages render in a hidden field or meta tag, a
constant-time comparison before routing, Fetch Metadata/`Origin` rejection of
cross-site requests, and the `_method` override only after verification.
Flash messages travel in a host-only cookie and are consumed when rendered.
Request layers (`forge_http::Middleware`) run before the route table. See
[ADR 0023](adr/0023-form-csrf-flash-and-method-override.md) and
[views](views.md).

### Database and transactions

PostgreSQL is the first database. SQL remains visible, with compile-time checked
queries where the build environment permits. A transaction-scoped unit-of-work
provides repositories; application code cannot retain transaction references past
their lifetime. Optimistic locking uses an explicit version column. Cursor
pagination uses a deterministic indexed tuple, normally `(created_at, id)`.

Forge also provides a conventional typed `Repository` contract for ordinary
aggregates. It uses associated `Id`, `Entity`, and cursor types, bounded
`PageLimit`, `CursorPage`, and explicit optimistic `RecordVersion` checks.
Applications may define narrower domain-specific repository ports when CRUD
semantics are not appropriate. SQLx row/pool/query types never cross this
application-facing contract.

Tenant-sensitive repository methods require `TenantContext`. Generated
PostgreSQL infrastructure begins an explicit transaction and writes both
`forge.tenant_id` and `forge.principal_id` with transaction-local
`set_config(..., true)`; RLS policies read them with
`current_setting(..., true)`. Missing context therefore fails closed, and the
values disappear at transaction end instead of surviving on pooled connections.

Migration and runtime credentials are separate. The schema-owning migration role
is used only by `migrate`/`rollback`; the serving process uses a role without
schema ownership or `BYPASSRLS`. Administrative cross-tenant access is a
separate future capability/role and is never implicit. See
[ADR 0004](adr/0004-tenancy-and-rls.md) and
[ADR 0011](adr/0011-postgresql-runtime-migration-roles-and-rls-context.md).

### Security audit evidence

Security audit events are structured immutable records, separate from ordinary
application logs. They carry a UUIDv7 event identity, trusted timestamp, actor,
optional tenant, validated action, explicit outcome and optional request linkage.
When tenant context exists, attribution is derived directly from the authorized
`TenantContext`.

Database-enabled generated applications include a PostgreSQL `AuditSink` and
the `forge_audit_events` migration. The generated runtime role can append but
cannot read, update or delete audit rows; a trigger also rejects mutation to
preserve append-only semantics. Cryptographic integrity chaining, immutable
external sinks, retention enforcement and privileged reader workflows remain
separate controls. See
[ADR 0012](adr/0012-structured-append-only-audit-evidence.md).

### Jobs and events

The first durable job adapter is PostgreSQL-backed. Enqueue and outbox writes can
share the application's transaction. Workers claim bounded batches using row
locking, heartbeats, exponential backoff with jitter, attempt ceilings, and a
dead-letter state. Job definitions declare idempotency behavior and concurrency
limits.

Domain events are in-process facts produced by domain behavior. Integration
events are versioned external contracts written to an outbox in the same commit as
domain state. Publishers deliver only committed rows. Consumers persist an
idempotency key before acknowledging. Forge does not promise exactly-once
delivery; it provides at-least-once delivery plus idempotent handling.

### HTTP, OpenAPI, WebSockets, and SSE

Forge will expose its own route and extractor contracts while using a mature HTTP
stack underneath. A single typed endpoint declaration feeds routing, validation,
OpenAPI, and client-generation hooks; schemas are not maintained independently.
Streaming responses inherit request cancellation and backpressure. Long-lived SSE
and WebSocket sessions have explicit connection limits, heartbeat policy, and
shutdown behavior.

### AI subsystem

Business logic depends on an `AiProvider` port and provider-neutral request,
stream, tool-call, usage, and error types. API, local-server, and CLI providers are
adapters. Provider routing applies capabilities, health, policy, budget, timeout,
retry, and circuit-breaker rules.

AI output is untrusted data. Tool execution is a distinct security pipeline:

```text
Intent -> Policy -> Authorization -> Validation -> Approval (when required)
       -> Sandboxed Execution -> Result Filtering -> Audit
```

The validated execution plan is immutable and binds principal, tenant,
capabilities, arguments, resource limits, policy version, expiry, and a unique
execution ID. Executors reject plans that are expired, replayed, broadened, or do
not match the current environment. Provider adapters cannot invoke executors
directly. See [ADR 0005](adr/0005-ai-provider-and-execution-boundary.md) and the
[threat model](threat-model.md).

## Configuration and secrets

Configuration has ordered sources: compiled defaults, configuration files,
environment variables, and explicit CLI overrides. The effective configuration
is deserialized into typed structures and validated before listeners or workers
start. Unknown keys fail in production mode; Phase 1 rejects unknown `FORGE_*`
environment variables in every environment
([ADR 0009](adr/0009-rails-style-generated-delivery-assets.md)). Secret values use opaque references
resolved by a secrets port; debug output is redacted and resolved secrets never
enter generated diagnostics or telemetry attributes.

## Observability and health

OpenTelemetry is the internal signal model. Trace context propagates through HTTP,
application calls, database queries, jobs, events, external APIs, and AI streams.
Low-cardinality identifiers are available in structured logs; tenant, user, and
prompt data require explicit classification and redaction. Metrics must not use
tenant or request IDs as labels.

- `/health/live` reports whether the process event loop is alive.
- `/health/ready` reports whether the instance can accept its configured work and
  checks only dependencies required by that role.
- `/health` is a summarized, access-controlled diagnostic endpoint.

Liveness never fails because an optional external dependency is unavailable.

## CLI and generated applications

`forge` is the stable workflow entry point. Initial phases implement commands only
when their backing feature is real. `forge doctor` reports toolchain, configuration,
and dependency readiness; `forge routes` and `forge openapi` introspect the same
compiled contract used at runtime. Generators are deterministic, format output,
refuse destructive overwrites unless explicitly approved, and modify existing
modules structurally rather than producing duplicate `*_new` files.

`forge new` emits the application Docker assets in Phase 1 and `forge build` can
produce the same release binary locally or in the container build. A development
container workflow may be offered, but Docker is not required to run the ordinary
Cargo commands. Phase 2 adds PostgreSQL by default to newly generated applications. The Compose
development profile pins PostgreSQL, uses health-based service ordering, a named
data volume and a one-shot migration service. SQLx remains in the generated
outer infrastructure/bootstrap boundary; framework-owned application-facing
contracts live in `forge-db`. `forge new -d none` creates a database-free
application. Compose remains a local/development convenience, not the production
orchestration contract.

Following Rails 8's `rails new`, Phase 1 also generates `.github/workflows/ci.yml`
(format check, Clippy with warnings denied, tests, image build) and
`.github/dependabot.yml` (cargo, docker, github-actions). Docker and CI assets
can be skipped with `--skip-docker` and `--skip-ci`. The runtime image is
distroless (`gcr.io/distroless/cc-debian13:nonroot`): it has no shell, runs as
numeric uid 65532, and uses the binary as its entrypoint. The image health check
runs the application's own `healthcheck` subcommand, which probes
`/health/live` over loopback. See
[ADR 0009](adr/0009-rails-style-generated-delivery-assets.md), the
[CLI reference](cli.md) and the [deployment guide](deployment.md).

The generated application remains an ordinary Cargo project. `forge build`,
`test`, `check`, `lint`, and `format` orchestrate documented Cargo commands and do
not replace them.

## Delivery sequence and exit criteria

Each phase must end with an executable vertical behavior and passing applicable
quality gates:

1. Runtime, CLI, configuration, HTTP, graceful shutdown, and health.
   The generated application includes a production-oriented Dockerfile and is
   exercised as both a host binary and a container.
2. UUIDv7, PostgreSQL, migrations, typed repository, transactions, and a
   development-only PostgreSQL Compose profile.
3. Authentication, authorization, tenancy, RLS, and audit logging.
4. PostgreSQL jobs, scheduler, events, outbox, and idempotent consumer.
5. End-to-end logs, traces, metrics, and correlation.
6. Contract-derived OpenAPI and conservative code generation/macros.
7. One production API AI adapter, one CLI adapter, routing, streaming, accounting,
   and the execution security pipeline.
8. Security abuse tests and hardening against the threat model.
9. Profile-guided optimization against published baselines.
10. Reproducible release, SBOM, signed artifacts, deployment examples, and a
    production certification checklist.

## Known tradeoffs and limitations

- Strict contexts and ports add explicit types and ceremony. Generators and
  focused derives should reduce repetition without weakening boundaries.
- PostgreSQL-first choices make the earliest releases unsuitable for applications
  requiring another primary database.
- RLS reduces tenant-leak impact but does not replace application authorization;
  both layers add integration-test cost.
- Runtime and HTTP internals may initially depend on Tokio and a mature HTTP
  implementation. Forge's contracts minimize lock-in but cannot promise effortless
  runtime substitution.
- Docker is the generated deployment baseline, but runtime contracts remain
  portable to Kubernetes, systemd, and bare metal. The framework does not promise
  that Compose is production orchestration.
- Provider-neutral AI contracts expose a common, intentionally limited capability
  model; vendor-specific features require explicit extensions.
- Performance targets are budgets to measure, not current results. See
  [performance targets](performance-targets.md).

## Decision index

See [the ADR index](adr/README.md), [threat model](threat-model.md),
[dependency policy](dependency-policy.md), [performance targets](performance-targets.md),
and [test strategy](test-strategy.md).
