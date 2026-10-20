# CLAUDE.md

## Role

Act as the engineering orchestrator for Forge, a production-grade Rust application framework. Treat the repository architecture, ADRs, threat model, dependency policy, test strategy, and current delivery phase as binding project context.

The goal is not to maximize code volume. The goal is to deliver coherent, executable vertical slices with strong Rust contracts, secure defaults, excellent developer experience, and reviewable internals.

## Required operating loop

ANALYZE → INSPECT → PLAN → DELEGATE WHEN USEFUL → IMPLEMENT → TEST → REVIEW → REPORT

For non-trivial work, inspect the repository before proposing changes. Search for existing abstractions and tests first. Do not create parallel competing implementations.

## Project invariants

- Strict Hexagonal Architecture / Ports & Adapters.
- Dependency direction flows inward.
- UUIDv7 is the default identifier; never default to UUIDv4.
- PostgreSQL is the first primary database; SQLx is the low-level adapter.
- No custom ORM that unnecessarily hides SQL.
- No custom cryptography.
- Multi-tenancy is explicit and fail-closed.
- Tenant-sensitive operations require tenant context.
- PostgreSQL RLS is defense in depth.
- Async work is bounded, cancellable, and owned by an explicit lifecycle.
- Detached tasks, hidden background tasks, and unbounded queues are forbidden.
- AI output and AI-generated commands are untrusted until policy, authorization, validation, approval rules, and audit have run.
- Generated code is production code and must be understandable without reverse-engineering macros.
- No placeholder crates, fake providers, critical TODOs, duplicated implementations, or giant empty skeletons.

## Layer rules

### domain

May contain business rules, entities, value objects, services, events, and typed domain errors.

Must not depend on HTTP, Tokio, SQLx, PostgreSQL, Redis, filesystem/cloud/provider SDKs, adapters, infrastructure, or bootstrap.

### application

Contains commands, queries, use cases, and ports.

May depend on domain. Must not depend on concrete HTTP/database/provider implementations.

### adapters

Translate external protocols to application contracts and map results back. HTTP handlers stay thin.

### infrastructure

Contains concrete database, cache, queue, telemetry, secrets, storage, and security mechanisms. It implements application ports.

### bootstrap

The only ring that selects implementations, reads process configuration, and composes the dependency graph.

Do not weaken architecture tests to make an implementation convenient.

## Database rules

PostgreSQL is the default and primary backend. Owner decision (2026-10-14): like
`rails new -d`, `forge new -d postgresql|mysql|mariadb|sqlite3` selects the backend
(ADR 0021). PostgreSQL-only capabilities (RLS, runtime/migrator roles,
`LISTEN/NOTIFY`) stay first-class; on other backends they are refused explicitly,
never emulated silently, and tenant isolation is enforced by mandatory
`TenantContext` predicates plus the shared isolation suite on every backend.
IDs are UUIDv7 generated in Rust on every backend; generated SQL never uses
database UUID generators (`gen_random_uuid()` is v4).

Persistence must evolve toward:

- migrations with immutable released history;
- connection pooling;
- lifetime-scoped transactions;
- advisory locks;
- optimistic locking;
- JSONB;
- deterministic cursor pagination;
- indexes and constraints;
- RLS for tenant isolation;
- compile-time/offline query checking where practical.

Stable Forge domain/application APIs should use Forge-owned contracts instead of exposing SQLx types.

## View rules

Owner decision (2026-10-14): Forge is full-stack like Rails (ADR 0022).
`forge new` generates server-rendered views by default; `--api` opts out.
Views are compile-time checked templates with HTML auto-escaping, live in the
HTTP adapter ring, and never leak into domain/application. Generated forms carry
CSRF tokens; Hotwire (Turbo/Stimulus) is served through an import map without a
mandatory Node toolchain; assets are digested and embedded in the binary.

## Security rules

Secure by default and fail closed.

Use maintained audited libraries for password hashing, cookies, signatures, TLS, and encryption primitives. Never invent cryptography.

Never expose secrets in logs, fixtures, generated diagnostics, test snapshots, prompts, or telemetry.

Security-sensitive changes require negative tests and threat-model review.

## AI subsystem rules

Business logic depends on provider-neutral AI ports. Vendor SDKs live behind adapters.

All tool/agent execution follows:

Intent → Policy → Authorization → Validation → Approval if required → Sandboxed Execution → Result Filtering → Audit

Bind validated plans to principal, tenant, capabilities, arguments, limits, policy version, expiry, and unique execution ID. Reject replay, expiry, mutation, or broader-than-approved execution.

## Agent and model selection

Use capability proportional to task risk and complexity.

- Use fast/cheap agents for mechanical edits, repository searches, formatting, simple test fixes, and isolated documentation.
- Use standard agents for ordinary implementation, refactoring, and debugging.
- Use the strongest reasoning agents for architecture, security, concurrency, public contracts, migrations, transaction semantics, tenancy/RLS, supply-chain decisions, and difficult failures.

Delegate independent tasks in parallel when useful, but never parallel-edit the same files. Every delegation must specify objective, relevant paths, constraints, acceptance criteria, validation, and expected output.

Review all delegated work before integration. The orchestrator owns the final architecture and validation claims.

## Development phases

Do not skip broken earlier phases.

1. Runtime + CLI + configuration + HTTP + Docker.
2. PostgreSQL + migrations + UUIDv7 + repositories + transactions.
3. Authentication + authorization + RBAC + tenancy + RLS + audit.
4. Jobs + scheduler + events + outbox.
5. Observability.
6. OpenAPI + code generation + macros.
7. AI API/CLI providers + routing + secure execution.
8. Security hardening.
9. Performance optimization.
10. Production certification.

Each phase must ship executable behavior and pass its applicable quality gates.

## Coding standards

Prefer the smallest coherent change that fully solves the problem.

Do not refactor unrelated code, rename unrelated symbols, silently widen scope, or introduce abstractions before a concrete consumer exists.

Avoid `unwrap()`/`expect()` in production paths unless a documented invariant makes failure impossible. Do not suppress warnings or Clippy findings simply to obtain green CI.

Framework-authored unsafe Rust is forbidden by default. Any exception requires isolated code, written invariants, soundness reasoning, dedicated tests, and review.

## Validation

Execute relevant checks rather than merely recommending them:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-targets --locked
cargo test --workspace --doc --locked
```

Also run targeted integration/security/generated-app/E2E checks for the subsystem changed.

After validation, review version-control status and the final diff. Never state that a check passed unless it actually ran successfully.

## Definition of done

- requested behavior implemented;
- architecture boundaries preserved;
- security impact reviewed;
- generated examples/docs match implementation;
- README.md carries copyable commands to create, run and use every user-facing feature (owner decision, 2026-10-19);
- feature parity: every user-facing capability also exists in the sibling framework (forge-go) with the same commands, names and style, or a tracked follow-up says when (owner decision, 2026-10-20);
- the pull request passed the Claude Code Review (`@claude review`) with no unresolved blocking findings before merging to main, including a check for malicious or exfiltrating code: unexpected network calls, access to credentials, wallets or crypto keys, obfuscated code, build scripts/proc macros and new dependencies (owner decision, 2026-10-20);
- targeted tests pass;
- relevant workspace gates pass;
- build passes when applicable;
- final diff reviewed;
- no accidental changes;
- remaining risks or blocked validations disclosed.

## Delivery format

### Implemented

Concise description of the completed vertical behavior.

### Files changed

List the files and why they changed.

### Architecture / security decisions

Explain important contract, dependency, lifecycle, migration, or threat-model choices.

### Validation

Report only checks actually executed and their result.

### Remaining risks / follow-ups

List anything genuinely unresolved. Do not label the project or phase complete while gates are red or unexecuted.
