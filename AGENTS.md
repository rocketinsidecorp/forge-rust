# AGENTS.md

## Mission

Build Forge as a serious, production-grade Rust application framework: Rails-level productivity with Rust safety, explicit architecture, secure defaults, cloud-native operations, and first-class AI infrastructure.

This repository is not a tutorial and must not accumulate placeholder crates, fake providers, duplicated implementations, or broad skeletons with no executable vertical behavior.

## Mandatory workflow

UNDERSTAND → INSPECT → PLAN → IMPLEMENT → VERIFY → REVIEW → REPORT

Before changing code:

1. Read the relevant ADRs, architecture, threat model, dependency policy, and tests.
2. Inspect existing implementations and generated output before creating new abstractions.
3. Identify the current delivery phase and its exit criteria.
4. Prefer the smallest vertical slice that leaves executable software.
5. Preserve user work and avoid unrelated refactors.

## Architecture

Forge uses strict Hexagonal Architecture / Ports & Adapters.

Generated applications have these rings:

- `domain`: entities, value objects, domain services, events, typed domain errors.
- `application`: commands, queries, use cases, and ports.
- `adapters`: HTTP, persistence, messaging, storage, email, and AI translation.
- `infrastructure`: database, cache, queue, telemetry, and security mechanisms.
- `bootstrap`: the composition root and process lifecycle.

Dependency direction is inward. Domain code must not depend on HTTP, SQLx, PostgreSQL, Redis, filesystems, cloud vendors, AI SDKs, Tokio, or outer application rings. Application code must not depend on concrete adapters or infrastructure.

Architecture tests are product code: a change that weakens dependency enforcement requires an ADR and explicit review.

## Rust rules

Use Rust's strengths as architectural constraints, not decoration:

- ownership, borrowing, lifetimes, traits, generics, enums, and zero-cost abstractions;
- typed errors rather than stringly typed failures;
- typed IDs where entity confusion is possible;
- compile-time validation and typestate only where they materially improve correctness;
- explicit cancellation, deadlines, backpressure, bounded concurrency, and graceful shutdown.

Do not use advanced Rust merely to demonstrate sophistication.

Framework-authored code forbids unsafe Rust by default. If unsafe becomes unavoidable, isolate it, document invariants and soundness reasoning, test it, and require dedicated review.

Avoid `unwrap()` and `expect()` in production paths unless a documented invariant makes failure impossible. Do not suppress warnings or Clippy findings merely to make CI green.

## Identifiers

UUIDv7 is mandatory by default. Never introduce UUIDv4 as the framework default.

Generated entities and framework identity helpers must naturally support UUIDv7 generation, validation, serialization, sorting, database integration, test helpers, and CLI generation.

## Persistence

Forge is PostgreSQL-first. SQLx is the initial low-level adapter.

Do not introduce a custom ORM that hides SQL. SQLx/vendor types must not leak into stable domain/application contracts unless an ADR explicitly accepts that coupling.

Required persistence capabilities include migrations, transactions, pooling, advisory locks, optimistic locking, cursor pagination, JSONB, constraints, indexes, PostgreSQL RLS, and checked queries where practical.

PostgreSQL is the default backend. The owner decided (2026-10-14) that, like `rails new -d`, `forge new -d postgresql|mysql|mariadb|sqlite3` selects the backend; ADR 0021 is the capability and architecture review. PostgreSQL-only capabilities (RLS, runtime/migrator roles, `LISTEN/NOTIFY`) are refused explicitly on other backends, never emulated silently. Generated SQL never uses database UUID generators; IDs are UUIDv7 generated in Rust.

## Views

Forge is full-stack like Rails (ADR 0022): `forge new` generates server-rendered views by default and `--api` opts out. Views are compile-time checked, HTML auto-escaping templates in the HTTP adapter ring; generated forms carry CSRF tokens; Hotwire is served through an import map; assets are digested and embedded in the binary.

## Tenancy and security

Multi-tenancy is first-class and fail-closed.

Tenant-sensitive operations require explicit tenant context. Never silently fall back to a global tenant. PostgreSQL RLS is defense in depth, not a replacement for application authorization.

Security is secure-by-default. Reuse maintained, audited crates for cryptography and password/session primitives. Never implement custom cryptography.

AI or tool execution must follow:

Intent → Policy → Authorization → Validation → Approval when required → Execution → Audit

AI-generated code or commands are untrusted input and must never gain unrestricted filesystem, process, network, or secret access.

## Async and lifecycle

Every async task has an owner and lifecycle. Avoid detached tasks, unbounded channels, hidden background work, and unbounded concurrency.

Tokio is the initial runtime implementation, but application contracts should not unnecessarily expose runtime-specific types.

## Delivery phases

Work vertically. Do not start a later phase while an earlier phase is left in a broken or placeholder state.

1. Runtime, CLI, configuration, HTTP, health, graceful shutdown, Docker.
2. UUIDv7, PostgreSQL, migrations, repositories, transactions.
3. Authentication, authorization, RBAC, tenancy, RLS, audit logging.
4. Jobs, scheduler, events, outbox, idempotent consumers.
5. End-to-end logs, traces, metrics, correlation.
6. Contract-derived OpenAPI, conservative code generation, macros.
7. AI providers, routing, streaming, accounting, secure execution.
8. Security hardening and abuse tests.
9. Performance profiling and optimization.
10. Production certification, SBOM, signed artifacts, release checklist.

Every phase must end with executable software and passing applicable gates.

## Dependencies

Dependencies are security-sensitive.

Before adding a direct dependency, evaluate maintenance, advisory history, unsafe/native code, build scripts/proc macros, license, MSRV, transitive impact, default features, public API leakage, and replacement strategy.

Prefer focused mature crates. Keep dependency graphs small. Do not add a dependency until the same change uses it.

## Generators and CLI

Generated code is production code.

Generators must be deterministic, refuse unsafe overwrites, render every placeholder, preserve architecture boundaries, and produce code that formats, builds, tests, migrates, starts, serves, and shuts down.

Never generate duplicate or temporary names such as `*_v2`, `*_final`, `*_new`, or `*_backup`.

CLI behavior should remain transparent wrappers around documented Rust/Cargo operations where possible.

## Required validation

Run the narrowest relevant checks first, then broader gates:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-targets --locked
cargo test --workspace --doc --locked
```

For generated applications also run the golden/generated-app checks and, when relevant, the host/container E2E path in `scripts/e2e-generated-app.sh`.

Database, security, tenancy, jobs/events, AI, and release changes require their subsystem-specific integration/security gates described in `docs/test-strategy.md`.

Never claim a check passed unless it actually executed successfully.

## Agent orchestration

Use the least expensive/capable agent or model that can safely solve the subtask:

- simple searches, mechanical edits, formatting, and isolated tests: fast/low-cost agent;
- ordinary implementation and debugging: standard engineering agent;
- architecture, security boundaries, concurrency, migrations, tenancy/RLS, public API design, and difficult root-cause analysis: strongest reasoning agent available.

Parallelize only independent work. Never have multiple agents edit the same code simultaneously. Delegation must include objective, context, paths, constraints, acceptance criteria, validation, and expected return.

The primary agent remains responsible for integration, final diff review, and truthfulness of validation claims.

## Definition of done

A change is done only when:

- the requested behavior exists as executable software;
- the final diff is coherent and contains no accidental changes;
- relevant architecture/security contracts remain intact;
- targeted and required broader checks pass, or execution limitations are explicitly reported;
- documentation and generated examples match the implementation;
- README.md carries copyable commands to create, run and use every user-facing feature (owner decision, 2026-10-19);
- every user-facing capability exists in both Forge frameworks with the same commands, names and style, or a tracked follow-up records when (owner decision, 2026-10-20);
- nothing merges to main without a passing Claude Code Review (`@claude review`) that also screens for malicious or exfiltrating code (owner decision, 2026-10-20);
- no critical TODO, fake provider, duplicate implementation, or knowingly broken quality gate remains.

## Final report

Report:

1. Summary.
2. Files changed.
3. Key implementation/architecture decisions.
4. Tests and checks actually executed.
5. Remaining risks, limitations, or blocked gates.
