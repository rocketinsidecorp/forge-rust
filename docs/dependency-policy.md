# Dependency Policy

Status: Accepted Phase 0 policy. Concrete dependency versions are selected and
recorded when the first consuming vertical slice is implemented.

## Principles

Dependencies are security-sensitive implementation choices, not shortcuts around
Forge's architecture. Forge prefers mature, focused crates for protocols,
cryptography, runtimes, parsing, and database drivers while owning its lifecycle,
contracts, policy, and developer experience.

Do not introduce a dependency until code in the same change uses it. Avoid a
framework-wide dependency when a leaf adapter can own it. Domain and application
contract crates keep the smallest practical graph and do not depend on vendor SDKs.

## Admission review

Every new direct dependency must document in the pull request:

- the capability it provides and why existing code or dependencies do not;
- maintenance activity, ownership concentration, release history, and issue
  responsiveness;
- advisory history and handling, use of `unsafe`, build scripts, proc macros,
  native code, network/build-time behavior, and transitive graph impact;
- license compatibility, default features, MSRV, platform support, binary-size and
  compile-time impact;
- whether its public types would leak into Forge APIs and the exit/replacement
  strategy.

Cryptography, authentication, parsers for hostile input, sandboxing, and release
tooling require security-owner review. Git dependencies are prohibited in releases
unless an incident exception pins an immutable commit, records provenance and
review, and has an expiry. Dependencies fetched from arbitrary registries are
prohibited unless that registry is explicitly approved and integrity-pinned.

## Allowed dependency roles

The architectural choices currently permit, but do not yet install:

- Tokio as the initial async runtime, behind lifecycle contracts.
- SQLx as the PostgreSQL implementation, behind repository/transaction ports.
- Maintained HTTP and TLS protocol crates behind Forge HTTP contracts.
- `serde`-ecosystem serialization at adapters and deliberately approved value
  boundaries.
- `tracing` and OpenTelemetry ecosystem crates behind Forge telemetry contracts.
- Maintained, audited password-hashing, cookie, signature, and TLS crates; no Forge
  cryptographic primitives.
- CLI and diagnostic crates that do not require application runtime linkage.

An allowed role is not blanket approval of any crate or feature set. Version and
feature choices still undergo admission review.

## Phase 2 PostgreSQL dependency decision

Phase 2 generated applications use SQLx 0.9 as the concrete PostgreSQL adapter.
The generated dependency disables SQLx default features and enables only the
capabilities Forge currently consumes: PostgreSQL, Tokio runtime integration,
migrations/macros, UUID, JSON, chrono, and Rustls with the ring/WebPKI root
backend.

SQLx remains an outer-layer implementation detail: Forge-owned database errors,
optimistic-version values and transaction contracts live in `forge-db`; domain
and application rings must not import SQLx. Generated bootstrap/infrastructure
code may use SQLx directly because those layers are adapters.

Rustls is enabled explicitly rather than relying on a runtime feature to imply
TLS. This keeps production PostgreSQL URLs that require TLS within the supported
generated configuration while avoiding a native OpenSSL dependency.

The dependency is not added to the framework workspace merely for convenience:
only generated applications that enable the database capability consume it.
`forge new -d none` removes SQLx entirely from a generated application's graph.

## Phase 3 persistence dependency decision

The persistent-session/audit slice introduces no new third-party package to the
workspace. It reuses the already-admitted `async-trait 0.1.92` contract helper
and the existing generated SQLx 0.9 PostgreSQL adapter. Database-enabled generated
applications now declare `async-trait` directly because their SQLx
`SessionStore` and `AuditSink` implementations implement framework traits
that use that macro; database-free generated applications do not add it.

## Phase 3 authentication mechanism dependency decision

The concrete authentication mechanism is confined to generated database-enabled
application infrastructure; the Forge workspace public contracts remain free of
third-party cryptographic types.

Admitted direct dependencies:

| Crate | Version | Purpose | Feature policy |
| --- | --- | --- | --- |
| `argon2` | `0.6.0` | Argon2id password hashing/verification | defaults disabled; `alloc,getrandom,password-hash,zeroize`; no Rayon parallel feature |
| `getrandom` | `0.4.3` | 256-bit bearer and CSRF secrets from the OS CSPRNG | default platform backend only |
| `sha2` | `0.11.0` | SHA-256 digests for high-entropy bearer/CSRF lookup | defaults disabled |
| `cookie` | `0.18.2` | RFC cookie parsing/building and security attributes | defaults disabled; signed/private cookie crypto is not enabled |
| `tokio` | `1.53.1` | `spawn_blocking` isolation for memory-hard hashing | `rt` only in the generated app |
| `thiserror` | `2.0.21` | bounded infrastructure errors | existing project dependency family |

Argon2 and SHA-2 are RustCrypto implementations; token entropy comes directly
from the operating-system source through getrandom. Forge does not implement
password hashing, randomness, SHA-256, or cookie grammar itself. The local helper
only hex-encodes random bytes and selects parameters/attributes.

The initial Argon2id policy is version 19, m=19456 KiB, t=2, p=1, 32-byte output.
This matches the current OWASP minimum profile and is a floor, not a permanent
performance target. Production sizing must benchmark authentication latency and
memory concurrency before raising parameters.

The RustSec review recorded during admission found historical advisories in old
`cookie` and `sha2` releases (RUSTSEC-2017-0005 and RUSTSEC-2021-0100);
the selected versions are above the published patched ranges. This manual review
does not replace the repository's `cargo deny`/RustSec gate, which is now
configured in `deny.toml` (see [Supply-chain gates](#supply-chain-gates)).

The cookie crate's optional signed/private jar feature is intentionally disabled:
the cookie carries a 256-bit opaque server-side bearer value, not trusted claims.
Only its parser/builder and `Secure`, `HttpOnly`, `SameSite`, `Path`
attributes are required.


## Features and public API containment

- Set `default-features = false` when defaults add unused protocols, native
  dependencies, or runtime behavior; otherwise record why defaults are safe.
- Feature flags are additive capabilities. They must not silently weaken security
  or change data semantics.
- Platform/provider integrations live in leaf crates or private modules.
- Public Forge contracts use standard library or Forge-owned types unless an ADR
  deliberately commits to an ecosystem type.
- Multiple versions of security-sensitive or foundational crates are investigated
  and resolved when practical.

## Versioning, lockfiles, and MSRV

The workspace commits `Cargo.lock`, including libraries, so CI, examples, tools,
and releases evaluate one reviewable graph. Automated update pull requests run the
full policy and test gates; security updates are prioritized but never merged
without compatibility checks.

Initial development tracks stable Rust. Before the first supported release, the
workspace declares an exact MSRV in `rust-version` and CI tests both MSRV and the
current stable toolchain. Raising MSRV is a documented compatibility change and
must not happen accidentally through a dependency update.

Dependencies use compatible semver requirements and the lockfile pins resolution.
Release builds use `--locked`. Yanked or unmaintained packages require replacement
or a time-bounded exception.

## Supply-chain gates

Gates 1–3 are configured: [`deny.toml`](../deny.toml) encodes the advisory,
license, ban and source rules below, and
[`.github/workflows/supply-chain.yml`](../.github/workflows/supply-chain.yml)
runs `cargo metadata --locked` and `cargo deny check` on every push and pull
request and daily (RustSec advisories appear without code changes). The gate was
verified locally with cargo-deny 0.20.2, including negative checks (removing an
allowed license and banning an in-use crate both fail). It cannot be reported as
a passing CI gate until GitHub-hosted runners execute for this repository. Run it
locally with `cargo install --locked cargo-deny && cargo deny check`.

Workspace crates are `publish = false` until a release process exists: their
internal dependencies are path-only, which crates.io rejects. Publishing requires
versioned internal dependencies, and `cargo deny` refuses wildcard dependencies on
any crate that becomes publishable. Reviewed duplicate: `syn` 2 and 3 are both
present because the `async-trait` proc macro moved to `syn` 3; both are
build-time only.

CI and release automation will enforce:

1. `cargo metadata --locked` succeeds and unapproved sources are absent.
2. `cargo deny check` enforces advisories, licenses, bans, and sources.
3. RustSec-compatible advisory scanning runs on the locked graph; suppressions
   name the advisory, affected reachability, compensating control, owner, and
   expiry.
4. Unused and duplicate dependencies are reviewed with workspace tooling; findings
   are not blindly auto-removed.
5. Release artifacts include an SPDX or CycloneDX SBOM, source revision, toolchain,
   and dependency lock digest.
6. Releases build in an isolated, network-denied build step after dependencies are
   fetched and verified, then receive provenance and signatures.
7. Container images contain the application binary, certificates, and only
   required runtime files, run as non-root, and are scanned before publication.

Generated Dockerfiles pin builder and runtime images by immutable digest for
release/certification builds. Automated updates preserve human-readable version
annotations and run the image build, vulnerability scan, health check, and
generated-application end-to-end suite. Base images follow the same source,
advisory, owner, and expiry rules as Rust crates. The final stage does not contain
Cargo, a shell, compiler, source tree, package manager cache, or build credentials
unless a documented platform requirement makes a narrower exception necessary.

The Phase 2 PostgreSQL Compose profile pins an explicit supported PostgreSQL major
and image digest. It is development infrastructure, is excluded from production
artifact claims, and uses disposable local credentials documented as unsuitable
for deployment.

Reproducible builds are a target: timestamps, paths, and build metadata are
controlled. Until byte-for-byte reproduction is verified on documented builders,
release notes must not claim reproducibility.

## License policy

Permissive licenses commonly compatible with Apache-2.0/MIT distribution are
allowed through explicit `cargo-deny` configuration. Copyleft, source-available,
unknown, or unlicensed code requires legal review before inclusion. License text
and notices are preserved in source and binary distributions as required.

## Unsafe code and native dependencies

Framework-authored code defaults to `#![forbid(unsafe_code)]`. If `unsafe` is
unavoidable, isolate it in a small crate/module, change the lint locally, document
invariants and soundness reasoning, add Miri/property/fuzz tests as applicable,
and obtain a dedicated review. Transitive `unsafe` is assessed by role and attack
surface rather than assumed safe.

Native dependencies are avoided unless they provide a material security or
platform benefit. They require supported-platform, cross-compilation, patching,
and minimal-container analysis.

## Exceptions and removal

Exceptions are checked-in records containing scope, reason, risk, compensating
controls, owner, approval date, and expiry. Expired exceptions fail CI. Removing a
dependency also removes features, configuration, documentation, and transitive
allowlist entries made solely for it.

See [ADR 0001](adr/0001-incremental-modular-workspace.md),
[ADR 0006](adr/0006-tokio-and-supervised-concurrency.md), and
[ADR 0007](adr/0007-sqlx-postgresql-first.md), and
[ADR 0008](adr/0008-docker-by-default.md).
