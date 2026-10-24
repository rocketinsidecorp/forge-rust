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


## Benchmark dependency decision

[Benchmarks](benchmarks.md) add two workspace crates, `forge-bench` and
`forge-loadgen`, that no other crate depends on and no release build contains.
`criterion` is not in the graph and is not added: `forge-bench` carries a small
harness on stable Rust instead. `forge-loadgen` uses only the standard library.

`forge-bench` adds one third-party package to the workspace lockfile family:
`argon2 0.6.0`, with the features and defaults the generated application
declares (see the authentication decision above), and its RustCrypto
dependencies `password-hash`, `phc`, `blake2` and `base64ct`. It is needed
only because the `password_verify` scenario must time the same Argon2id
parameters the generated application's `Argon2idPasswordHasher` uses (version
19, m=19456 KiB, t=2, p=1), and that hasher lives in generated application
code, not in a framework crate. `cargo deny check` passes (advisories, bans,
licenses, sources). No framework crate depends on `argon2`.

`forge-bench` also contains the repository's only `unsafe`: the counting
global allocator of `src/alloc.rs`, which forwards every call to `System` and
adds two atomic counters so the benchmarks can report `B/op` and `allocs/op`.
It is an exception to "no framework-authored unsafe" under the conditions the
project requires: isolated in one module of a bench-only crate (the crate sets
`unsafe_code = "deny"` and the one `unsafe impl` carries a local `allow`, with
`undocumented_unsafe_blocks` denied), written invariants and a soundness
argument in the module documentation, dedicated tests (`tests/alloc.rs`), and
review through the pull request. Every other crate keeps
`unsafe_code = "forbid"`.

## OpenAPI dependency decision

[ADR 0039](adr/0039-openapi-from-typed-routes.md) adds two workspace crates,
`forge-openapi` (the document core: `forge-core`, `serde_json`) and
`forge-macros` (the `OpenApiSchema` derive), and **no new third-party
package**. `forge-macros` depends directly on `syn` 2 (features
`clone-impls`, `derive`, `parsing`, `printing`, `proc-macro`, all already
enabled in the graph), `quote` and `proc-macro2`, which `serde_derive`,
`thiserror-impl`, `sqlx-macros` and `tracing-attributes` already bring in at
the locked versions. It is a proc macro: it runs at compile time, reads only
the item it derives for, has no build script, performs no I/O or network
access and contains no `unsafe`. `utoipa` and `schemars` were considered and
not admitted (ADR 0039 records why); none of these types appears in a Forge
API (`Schema` is a `serde_json::Value`).

## Server-rendered views dependency decision

The form CSRF/flash slice ([ADR 0023](adr/0023-form-csrf-flash-and-method-override.md))
adds three direct dependencies to `forge-views` (feature `runtime`) and makes
`askama` a direct dependency of full-stack generated applications.

| Crate | Version | Purpose | Feature policy |
| --- | --- | --- | --- |
| `cookie` | `0.18.2` | parse `Cookie` headers and build `Set-Cookie` values for `__Host-forge_csrf` and `__Host-forge_flash` | defaults disabled; `percent-encode` only (flash values); signed/private jars disabled |
| `getrandom` | `0.4.3` | 256-bit CSRF tokens from the OS CSPRNG | default platform backend only |
| `subtle` | `2.6.1` | constant-time comparison of the presented and cookie tokens | defaults disabled |
| `askama` (generated apps) | `0.16.1` | the `Template` derive for the application's view models | defaults disabled; `derive`, `std`; same version as `forge-views` |

Admission review:

- `cookie` and `getrandom` were admitted in the Phase 3 authentication
  decision above with the same versions; this slice adds a framework consumer.
  `cookie` brings `time` 0.3 (`time`, `time-core`, `time-macros`, `deranged`,
  `num-conv`, `powerfmt`; MIT OR Apache-2.0, maintained by the time-rs
  project, `time-macros` is a build-time proc macro) for cookie expiry; Forge
  uses only `Max-Age=0` removal. `percent-encoding` was already in the graph.
- `subtle` (dalek-cryptography, BSD-3-Clause, no build script or proc macro,
  no unsafe in the comparison path, no RustSec advisories) was already in the
  locked graph through RustCrypto crates; it is used only for
  `ConstantTimeEq` on fixed-length token bytes. Forge does not implement the
  comparison itself.
- None of the three types appears in a public `forge-views` signature; tokens
  stay `forge_auth::CsrfToken` and responses `forge_http::Response`.
  Replacement: any RFC 6265 cookie parser, any CSPRNG crate, any audited
  constant-time equality.
- Full-stack generated applications declare `askama` because the derive must
  be named by the application crate; `--api` applications do not. Their
  `build.rs` depends on `forge-views` with `default-features = false,
  features = ["build"]` (SHA-2 and the standard library only).
- The vendored Turbo 8.0.23 and Stimulus 3.2.2 modules were admitted with
  their pinned digests in earlier slices; this slice only serves them.

## Authentication pages dependency decision

[ADR 0026](adr/0026-authentication-pages-and-setup.md) admits one new direct
dependency and moves two admitted crates into `forge-auth`.

| Crate | Version | Used by | Purpose | Feature policy |
| --- | --- | --- | --- | --- |
| `rustix` | `1.1.5` | `forge` (Unix targets only) | turn terminal echo off (`tcgetattr`/`tcsetattr`) while `setup` reads the owner password | defaults disabled; `std`, `termios` |
| `getrandom` | `0.4.3` | `forge-auth` | 256-bit password reset tokens from the OS CSPRNG | as admitted above |
| `sha2` | `0.11.0` | `forge-auth` | SHA-256 digests of reset tokens and throttle keys | defaults disabled |

`rustix` review: it was already in the workspace `Cargo.lock` (through
`tempfile`); it is maintained by the Bytecode Alliance, MIT/Apache-2.0,
widely used by `std`-adjacent crates, has no build-time network access, and
on Linux uses its own `linux-raw-sys` syscall backend. It contains `unsafe`
code for system calls; Forge's own code stays `forbid(unsafe_code)` and calls
only its safe termios API. No rustix type appears in a Forge public API; the
replacement strategy is a different safe termios binding or refusing the
interactive prompt (`--password-stdin` and `FORGE_SETUP_PASSWORD` keep
working). Non-Unix targets do not compile it and refuse the prompt.

`forge-auth` previously used no cryptographic crate. It now calls the same
admitted `getrandom` and `sha2` releases as `forge-views` and generated
applications; it still implements no cryptographic primitive (it hex-encodes
random bytes and hashes with `sha2`).

## Phase 4 jobs and events dependency decision

`forge-jobs` and `forge-events` ([ADR 0025](adr/0025-durable-jobs-scheduler-and-outbox.md))
add **no new third-party package** to the locked graph. They reuse admitted
workspace dependencies: `sqlx` 0.9 with the `postgres` feature (already used
by `forge-db`; no `macros` or `json` feature: migrations are embedded with
`include_str!` and payloads are bound as text and cast to `jsonb`), `tokio`
(`JoinSet`, timers), `serde`/`serde_json` (job and event payloads),
`async-trait` (object-safe handler, publisher and subscriber traits),
`thiserror`, `tracing`, `uuid` (UUIDv7 ids and lease tokens) and `getrandom`
0.4.3 (retry and poll jitter only; a jitter failure falls back to the
un-jittered delay, it is not a security property).

Cron parsing is hand-written (about 400 lines with documentation, no `unsafe`) instead of
admitting a cron crate: the five-field UTC grammar is small, the evaluator is
property-tested against a minute-by-minute oracle and fuzzed with arbitrary
input, and it avoids a time-zone database dependency. No SQLx, Tokio or
serde_json type appears in a stable domain/application contract: handlers see
`Job`, `JobContext`, `JobError`, `Timestamp` and Forge ids; SQLx types appear
only in the PostgreSQL adapter functions that take a `forge_db::DbTransaction`.

## Live updates dependency decision

[ADR 0031](adr/0031-live-updates-over-server-sent-events.md) adds the workspace
crate `forge-broadcast` and **no third-party package to `Cargo.lock`**. It
declares only crates already admitted for the workspace: `hmac`, `sha2` and
`subtle` (HMAC-SHA256 stream tokens, constant-time compare), `base64` (token
encoding; `forge-config` also decodes `FORGE_STREAMS_KEY` with it), `getrandom`
(per-process development keys and notification ids), `tokio`, `bytes`,
`serde`, `serde_json`, `async-trait`, `thiserror`, `tracing`, and, behind its
`postgres` feature, `sqlx` with `postgres` (already enabled by `forge-db`) for
`PgListener` and `pg_notify`. No `hmac`, `sqlx` or `tokio` type appears in the
stable channel, signer or access contracts; `PgBroker::publish_in` takes a
`sqlx::PgConnection` in the PostgreSQL infrastructure module, like the other
transactional adapters. `forge-http` streams response bodies with its own
unfold body over the existing `hyper` `Body` trait, without `futures`.

Accepted exception: `sqlx::PgListener` keeps notifications in an unbounded
internal channel and its `Drop` spawns a detached `UNLISTEN *` task. Both are
inside the driver, bounded in practice (the listener runs no statement after
`LISTEN`, so the channel only buffers what arrives during that round trip),
and the listener's lifetime is owned by the `PgBroker::listen` lifecycle
worker ([ADR 0031](adr/0031-live-updates-over-server-sent-events.md#accepted-residuals-and-capacity-edges)).

## Rate limiting, cache and storage dependency decision

[ADR 0027](adr/0027-rate-limiting.md), [0028](adr/0028-cache.md) and
[0029](adr/0029-file-storage.md) add three workspace crates and **no new
package to `Cargo.lock`**: every third-party crate they name was already
resolved. Newly declared direct dependencies:

| Crate | Version | Used by | Purpose | Feature policy |
| --- | --- | --- | --- | --- |
| `hmac` | `0.13.0` | `forge-storage` | HMAC-SHA256 of signed links | defaults disabled; same RustCrypto `digest` 0.11 as `sha2` 0.11 |
| `base64` | `0.22.1` | `forge-config`, `forge-storage` | canonical unpadded base64url of the signing key and signatures | defaults disabled; `alloc` only |
| `rustix` | `1.1.5` | `forge-storage` (Unix) | safe `openat`/`mkdirat`/`renameat`/`unlinkat` so every path component is opened with `O_NOFOLLOW` | defaults disabled; `fs`, `std` |
| `sha2`, `subtle`, `bytes`, `uuid`, `async-trait`, `serde`, `serde_json`, `thiserror`, `tokio`, `tracing`, `sqlx` | as admitted | the three crates | digests, constant-time compare, buffers, UUIDv7, contracts | unchanged workspace entries; `sqlx` adds only `macros` (embedded migrations) |

Admission review:

- `hmac` (RustCrypto, MIT OR Apache-2.0) was already in the graph through
  `sqlx-postgres`; it has no build script, proc macro, native code or `unsafe` of
  its own, and no RustSec advisories. It only appears in `forge-storage`'s
  private signing code; no `hmac` type is in a public signature. Replacement:
  any audited HMAC-SHA256.
- `base64` (MIT OR Apache-2.0, `#![forbid(unsafe_code)]`) was already in the
  graph through `sqlx-core`. The URL-safe no-padding engine rejects padding,
  non-alphabet bytes and non-canonical trailing bits, which is what makes the
  signing key and the signature have exactly one spelling.
- `rustix` (Bytecode Alliance, Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR
  MIT) was already in the graph through `tempfile`. It contains `unsafe`
  internally to issue syscalls; Forge code stays `forbid(unsafe_code)` and uses
  its safe API. It replaces `cap-std` (not admitted) for race-free,
  symlink-refusing path traversal; a canonicalize-and-compare in `std` would
  have a check-then-use window. Linux/macOS/BSD only; the adapter is
  `cfg(unix)`. It does not appear in public Forge signatures.
- **Multipart parsing and content sniffing are not outsourced.** `multer`,
  `infer` and `mime_guess` were evaluated and not admitted: they would add
  `mime`, `encoding_rs`, `spin` and a `futures` stream surface to a hostile-input
  path that needs security-owner review, for a streaming capability Forge cannot
  use because `forge-http` buffers bodies. Forge's own parser and sniffer are
  small, `forbid(unsafe_code)`, bounded, and covered by exhaustive negative,
  differential and mutation tests (ADR 0029).
- The supply-chain gate (`cargo deny check`) is unchanged and was run with these
  crates in the graph.

## Telemetry dependency decision

[ADR 0033](adr/0033-telemetry.md) ports Forge-Go's dependency-free telemetry
as the workspace crate `forge-telemetry` instead of adopting the
OpenTelemetry SDK (`opentelemetry`, `opentelemetry_sdk`,
`opentelemetry-otlp` with `tonic`/`prost` or `reqwest`,
`tracing-opentelemetry`, `opentelemetry-prometheus` with `prometheus`): that
stack would add several dozen packages, a second HTTP client, gRPC and
protobuf code generation, pre-1.0 crates with frequent breaking releases and
background tasks outside Forge's lifecycle, for a wire format Forge can
produce with serde_json over the hyper and rustls it already ships. New
packages in `Cargo.lock`:

| Crate | Version | Used by | Purpose | Feature policy |
| --- | --- | --- | --- | --- |
| `tokio-rustls` | `0.26.6` | `forge-telemetry` | TLS for `https` OTLP endpoints over the existing `rustls` | defaults disabled (no `aws_lc_rs`, no `logging`); `ring`, `tls12` |
| `flate2` | `1.1.10` | `forge-telemetry` | gzip request bodies (`FORGE_TELEMETRY_OTLP_COMPRESSION=gzip`) | defaults disabled; `rust_backend` (pure-Rust `miniz_oxide`, no zlib or C) |
| `miniz_oxide`, `adler2`, `crc32fast`, `simd-adler32` | `0.9.1`, `2.0.1`, `1.5.2`, `0.3.10` | via `flate2` | DEFLATE and checksums | as `flate2` requires |
| `want`, `try-lock` | `0.3.2`, `0.2.5` | via `hyper` `client` | hyper's client-side dispatch | hyper's `client` feature only (`http1` already enabled) |

Directly declared crates that were already in the graph: `rustls` 0.23
(defaults disabled; `ring`, `std`, `tls12`, as SQLx's
`tls-rustls-ring-webpki` already enabled), `webpki-roots` 1.0 (the Mozilla
root store SQLx uses; its CDLA-Permissive-2.0 exception in `deny.toml` now
covers this consumer too), `httpdate` 1.0 (hyper's own; `Retry-After`
dates), `getrandom` 0.4 (trace and span ids from the OS CSPRNG), and
`tracing-subscriber` (the log export layer).

Admission review:

- `tokio-rustls` is maintained by the rustls organization alongside
  `rustls`, MIT OR Apache-2.0, no build script, proc macro or native code of
  its own, no RustSec advisories; it is a thin async adapter and its types
  stay private to the exporter. Replacement: a hand-written adapter over
  `rustls::ClientConnection`.
- `flate2` is maintained under the rust-lang organization, MIT OR
  Apache-2.0; with `rust_backend` it compiles no C. `miniz_oxide` and
  `adler2` are also dependencies of the Rust standard library's backtrace
  support; `crc32fast` and `simd-adler32` use `unsafe` for SIMD checksums
  (reviewed by role: they checksum Forge's own request bodies, never hostile
  input). Only compression is used; Forge never decompresses. Replacement:
  dropping gzip support (the variable would then refuse `gzip`).
- `want` and `try-lock` are hyper's own small client-side crates (same
  maintainer as hyper), MIT.
- No new type appears in a public signature except `http::HeaderMap` in
  `OtlpClient::new`, already a public Forge type. Public telemetry contracts
  (`Telemetry`, `Registry`, `Span`, `SpanContext`) are Forge-owned.
- Graph impact for generated applications: `ring` was already in every
  application's graph before this change (through `forge-core` → `sqlx-core`,
  database-free applications included), so telemetry adds no C build step;
  the new build cost is the `flate2` family and `tokio-rustls` (about 15
  packages). Of the new crates only `crc32fast` has a build script, and it
  only runs `$RUSTC --version` to emit `cfg` flags.
- `cargo deny check` was run with these crates in the graph.

## Mail dependency decision

[ADR 0035](adr/0035-mail.md) adds the workspace crate `forge-mail` and **no
new package to `Cargo.lock`**. It declares only crates already admitted:
`rustls` and `tokio-rustls` (ring provider, TLS 1.2/1.3; STARTTLS and implicit
TLS), `webpki-roots` (Mozilla roots, under the existing CDLA-Permissive-2.0
exception), `rustls::pki_types` PEM parsing for `FORGE_MAIL_TLS_CA_FILE`,
`askama` (templates and the preview page), `base64` (AUTH, encoded words,
attachments), `getrandom` (MIME boundaries), `uuid` (UUIDv7 `Message-ID`s),
`serde`, `async-trait`, `thiserror`, `tracing` and `tokio` (it names the
`io-util` feature, which `sqlx-core` already enables). Dev-dependencies:
`tempfile`, `serde_json`, `tracing-subscriber`.

`lettre` was evaluated and not admitted: with the needed features it would
add seven packages (`lettre`, `nom`, `mime`, `quoted_printable`,
`email-encoding`, `email_address`, a second `base64` 0.23), a MIME builder
whose output differs from Forge-Go's, and a message type that would leak into
Forge's API, while Forge would still wrap it to enforce its TLS rules. The
Forge-owned SMTP client speaks only the submission subset, has no unsafe
code, bounds every reply, and is covered by fake-server tests. No rustls or
askama type appears in a stable `forge-mail` signature except askama's
`DynTemplate` in `Templates`, deliberately: templates are the views' engine.
The test CA and key in `crates/forge-mail/tests/fixtures/tls` are test
material, not secrets.

## Internationalization dependency decision

[ADR 0037](adr/0037-internationalization.md) adds the workspace crate
`forge-i18n` and **no new package to `Cargo.lock`**. It declares `serde` and
`serde_json` (the translation files are parsed by a serde visitor of Forge's
own, which refuses the duplicate keys `serde_json` would silently overwrite),
`thiserror` and `tracing`; dev-dependencies `tempfile` and
`tracing-subscriber`. CLDR plural rules are code, not data, so no CLDR or ICU
package (`icu_*`, `fluent`, `rust-i18n`) was admitted: they would add data
crates or macros whose behavior differs from Forge-Go's, which reads the same
files. Dates are a Forge-owned `DateTime`, so the core needs no date library.
`forge-config` (to validate `FORGE_I18N_DEFAULT_LOCALE`), `forge-views` (the
request layer and template helpers, feature `runtime`) and `forge-cli`
(`forge i18n check`, which also adds `serde` for its JSON report) depend on
`forge-i18n`; the `forge` facade exposes it behind the feature `i18n`.

## Two-factor authentication dependency decision

[ADR 0040](adr/0040-totp-multi-factor-authentication.md) adds TOTP MFA and
**no new package to `Cargo.lock`**. `forge-auth` newly declares, all from the
existing lock and the RustCrypto/ring stacks already reviewed:

| Crate | Version | Used by | Purpose | Feature policy |
| --- | --- | --- | --- | --- |
| `hmac` | `0.13.0` | `forge-auth` | HMAC of RFC 6238 codes | defaults disabled (admitted for `forge-storage`) |
| `sha1` | `0.11.0` | `forge-auth` | the SHA-1 of HMAC-SHA-1, the hash authenticator apps implement | defaults disabled; already locked through `sqlx-mysql` |
| `ring` | `0.17.14` | `forge-auth` | AES-256-GCM of TOTP seeds at rest (`LessSafeKey`, random 96-bit nonces) | defaults disabled; already in every application's graph through rustls |
| `subtle`, `base64`, `getrandom`, `sha2` | as admitted | `forge-auth` | constant-time digest comparison, the key and challenge encodings, CSPRNG, digests | unchanged workspace entries |

Admission review:

- No TOTP crate (`totp-rs`, `totp-lite`, `google-authenticator`) was
  admitted: RFC 6238 is a dynamic truncation over an HMAC, which Forge-Go
  also writes over its standard library; the HMAC and hash stay in RustCrypto.
  The codes are checked against the RFC 6238 SHA-1 vectors.
- `aes-gcm` (RustCrypto) would have added `aes`, `cipher`, `ctr`, `ghash`,
  `polyval`, `aead` and `universal-hash`; `ring`'s AES-GCM is already linked
  into every application for TLS, audited and constant-time on the supported
  targets, so it adds no code. Its C/assembly build step and `unsafe` were
  already accepted for rustls (see the telemetry decision). The stored form,
  `nonce || ciphertext || tag`, is byte-compatible with Forge-Go's
  `NewAESGCMSecretCipher`, checked against a vector produced by Go's
  `crypto/cipher`.
- Base32 (RFC 4648) of the seed and backup codes is a 30-line encoder for
  whole 5-byte groups in `forge-auth` (no padding case exists), tested
  against RFC 4648 and Go's `encoding/base32`; it is an encoding, not
  cryptography.
- No new type appears in a public signature: `forge_auth::mfa` exposes
  Forge-owned types (`TotpSecret`, `SecretCipher`, `Aes256GcmSecretCipher`,
  digests, `MfaBackupCode`, `MfaChallengeToken`).
- `forge-tenancy` gains only a `testing` feature (the in-memory MFA store)
  and `tokio` as a dev-dependency; `forge-cli` gains `forge-auth` as a
  dev-dependency for the generated-application end-to-end test.
- `cargo deny check` was run with these declarations.

## Credentials and attribute encryption dependency decision

[ADR 0041](adr/0041-encrypted-credentials-and-attribute-encryption.md) adds
the `forge-encryption` and `forge-credentials` crates and **no new package to
`Cargo.lock`**:

| Crate | Version | Used by | Purpose | Feature policy |
| --- | --- | --- | --- | --- |
| `ring` | `0.17.14` | `forge-encryption` | AES-256-GCM, HKDF-SHA256, HMAC-SHA256 | defaults disabled; already in every application through rustls and `forge-auth` |
| `zeroize` | `1.9.1` | `forge-encryption`, `forge-credentials`, `forge-cli`, `forge-auth` | wipes key material, plaintexts and key text on drop (in `forge-auth`: TOTP seeds, provisioning links, backup codes and challenge tokens) | `alloc` only, no `zeroize_derive`; already locked through rustls |
| `rustix` | `1.1.5` | `forge-credentials` (Unix) | `O_NOFOLLOW` and `ELOOP` constants for key files, no `unsafe` | the workspace `fs`, `std` entry |
| `base64`, `getrandom`, `serde`, `serde_json`, `thiserror`, `async-trait`, `uuid`, `sqlx` | as admitted | both crates | encodings, CSPRNG, documents, errors, the `Rewriter` port, UUID keys, the SQLx adapters (optional, backend features) | unchanged workspace entries |

Admission review:

- RustCrypto `hkdf` and `hmac` are locked too, but `aes-gcm` is not; `ring`
  provides all three primitives in one audited, constant-time library that
  is already linked, so one implementation backs the credentials, the
  attribute values and the MFA seeds.
- Forge-Go uses only AES-256-GCM, so no ChaCha20-Poly1305 crate is needed.
- No crate type appears in a public signature: keys, rings, ciphers and
  errors are Forge-owned types; the SQLx adapters take `forge_db::Database`.
- `forge-cli` gains `forge-credentials`, `forge-encryption` and `zeroize` as
  normal dependencies (path crates and an already-locked crate).

## Deployment dependency decision

[ADR 0042](adr/0042-forge-deploy.md) adds `forge deploy`, `forge logs`,
`forge app` and the `forge-proxy` crate with **no new package in
`Cargo.lock`** (the lock gains only the `forge-proxy` workspace entry and the
new edges below). Newly declared, all already in the graph:

| Crate | Version | Used by | Purpose | Feature policy |
| --- | --- | --- | --- | --- |
| `regex-automata` | `0.4.18` | `forge-cli` | Forge-Go's validation patterns (RE2 syntax) and `forge logs --grep` | default features; already locked through `tracing-subscriber` with its `aho-corasick`, `memchr` and `regex-syntax` |
| `getrandom` | as admitted | `forge-cli`, `forge-proxy` | container-name suffixes, ssh control directory and state file names | unchanged workspace entry |
| `hyper` `client` | as admitted | `forge-proxy` | health probes, forwarding and the ACME client | the feature `forge-telemetry` already enables |
| `rustls`, `tokio-rustls`, `ring`, `webpki-roots` | as admitted | `forge-proxy` | HTTPS serving (TLS 1.2/1.3), the ACME client's verified HTTPS, ES256 JWS and P-256 keys | `ring` provider only, as for SQLx and telemetry |
| `url`, `base64`, `tokio-util` | as admitted | `forge-proxy` | ACME URLs, JWS and PEM encodings, task ownership | unchanged workspace entries |

Admission review:

- No YAML crate was admitted. `serde_yaml` is deprecated, `serde_yml` carries
  an unsoundness advisory, `yaml-rust` is unmaintained and the maintained
  forks (`serde_norway`, `saphyr`, `yaml-rust2`) are new packages with large
  parsers for a file whose subset is small. `forge-cli` reads that subset
  with a strict reader and refuses the rest with a line number.
- No ACME crate was admitted. `instant-acme` and `rustls-acme` would add new
  packages (certificate-request generation such as `rcgen` and its encoders,
  and their own HTTP client integration) to the lock; the RFC 8555 flow over
  the existing hyper, rustls and ring is a few hundred lines. The PKCS#10 request is DER encoding
  only; every key, signature and digest comes from `ring`. The client is
  integration-tested against an in-test ACME server that verifies the JWS
  signatures and nonces and has `openssl` verify and sign the requests.
- No HTTP/2 (`h2`) and no `hyper-util` client-legacy (`tower-service`):
  forge-proxy speaks HTTP/1.1 and opens one connection per forwarded
  request.
- The `ssh` and `docker` binaries are driven as subprocesses from argument
  vectors, as in Forge-Go; no SSH or Docker library is linked.
- `forge-cli` gains a test-support example (`tests/support/fake_docker.rs`),
  built by `cargo test` and never installed. Neither crate has a build
  script, procedural macro or `unsafe` code; forge-proxy's network calls are
  the configured targets, the ACME directory and nothing else.
- `cargo deny check` was run with these declarations.

## CLI lifecycle dependencies

[ADR 0036](adr/0036-application-lifecycle-commands.md) adds **no package to
`Cargo.lock`** (`cargo deny check` is unchanged: the diff is one line, `rustix`
in the `forge-cli` entry). `forge-cli` now depends, through the workspace, on:

| Dependency | Why | Notes |
| --- | --- | --- |
| `forge-config` (path) | `forge doctor` loads the same `AppConfig` the application loads, and describes database URLs with `forge_config::database_url` | no secret leaves it: `SecretString` stays redacted |
| `url` (via `forge-config`) | reads database URLs with the parser SQLx uses, so `forge doctor` and the guards mean what the driver means | already in the lock through `sqlx-core`; the workspace entry is `2.5.8` with default features, the ones SQLx enables |
| `serde_json` | Cargo's `--message-format=json` (the built binary's path) and `doctor --json` | previously a dev-dependency; admitted workspace entry |
| `tokio` | one current-thread runtime on one thread that installs the SIGINT/SIGTERM handlers and sets a flag | `rt` and `signal`, already unified in the workspace features; no network or task spawning |
| `rustix` with `process` | SIGTERM/SIGKILL to the application's process group and to a build, with no `unsafe` and no shell | the workspace entry gains the `process` feature (it had `fs`, `std`); no new transitive package |

File watching is standard-library polling: `notify` (inotify/FSEvents,
`mio`, `walkdir`, `filetime`, ...) was not admitted because the loop must behave
the same on bind mounts and network file systems, a poll of a few hundred files
every 500 ms is negligible, and polling is what Forge-Go does. The CLI contains
no `unsafe`, no build script and no procedural macro of its own, makes no
network call other than the TCP connection `forge doctor` opens to the
configured database host, and starts only `cargo`, `rustc --version` and the
binary `cargo build` produced.

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

## Fuzzing and supply-chain tooling (ADR 0043)

[ADR 0043](adr/0043-continuous-fuzzing-sbom-signed-images.md) adds coverage-guided
fuzzing, SBOMs and signed images. It adds **no package to the framework
workspace or its `Cargo.lock`**; everything below is test or release tooling that
is never linked into a released artifact.

The fuzz targets live in `fuzz/`, a standalone package with its own `[workspace]`
and committed `Cargo.lock` (excluded from the framework workspace like
`examples/`), so the framework's dependency graph, features and lockfile are
unchanged. The package depends on the framework crates by path and on one
third-party crate:

| Crate | Version | Role | Notes |
| --- | --- | --- | --- |
| `libfuzzer-sys` | `0.4.13` | the libFuzzer entry point of every `fuzz_target!` | Maintained by the Rust Fuzz project (rust-fuzz); 0.4.13 is 4 months old at admission. Build script compiling LLVM's libFuzzer C++ sources through `cc` (a C++ compiler is needed to fuzz); `unsafe` FFI glue by design; no network access at build time. License `(MIT OR Apache-2.0) AND NCSA`: the NCSA part (the University of Illinois/NCSA license, permissive and BSD-style) is allowed for this crate only by a scoped, expiring exception in `deny.toml`. Exit: the targets are plain `&[u8]` functions, so any other harness (`bolero`, AFL) replaces it by changing the macro. |
| `arbitrary` | `1.5.0` | transitive of `libfuzzer-sys` (not used directly: targets split `&[u8]` themselves) | rust-fuzz; no build script, no proc macro (the `derive` feature is off). |
| `jobserver` | `0.1.35` | transitive of `cc` (already in the framework lockfile) for parallel C++ builds | Cargo's own crate. |

The rest of `fuzz/Cargo.lock` is the framework's `Cargo.lock` pruned to the crates
the targets use, at the same versions. `cargo deny check` runs on
`fuzz/Cargo.toml` with the same `deny.toml` in CI (job `cargo deny` of
`supply-chain.yml`), and Dependabot proposes updates for `fuzz/` separately.

Fuzzing needs a nightly toolchain (`-Zsanitizer` for AddressSanitizer and
coverage instrumentation). It is pinned by date in `scripts/fuzz.sh`, installed
only by the fuzz jobs, and the framework itself builds on the stable toolchain of
`rust-toolchain.toml`. `cargo-fuzz` (`0.13.2`) and `cargo-auditable` (`0.7.7`) are
developer and CI tools installed with `cargo install --locked` at exact versions
that are at least a week old; they are not dependencies of any crate. Syft
(SBOMs) and cosign (signing) are not Rust dependencies either: Syft runs from an
image pinned by digest, cosign comes from a SHA-pinned action. Release tooling
needs security-owner review (see Admission review): the pins, the permission
rules and the tag-only trigger are checked by `crates/forge/tests/supply_chain.rs`,
and [supply chain](supply-chain.md) lists every pin and how it was verified.

## Exceptions and removal

Exceptions are checked-in records containing scope, reason, risk, compensating
controls, owner, approval date, and expiry. Expired exceptions fail CI. Removing a
dependency also removes features, configuration, documentation, and transitive
allowlist entries made solely for it.

See [ADR 0001](adr/0001-incremental-modular-workspace.md),
[ADR 0006](adr/0006-tokio-and-supervised-concurrency.md), and
[ADR 0007](adr/0007-sqlx-postgresql-first.md), and
[ADR 0008](adr/0008-docker-by-default.md).
