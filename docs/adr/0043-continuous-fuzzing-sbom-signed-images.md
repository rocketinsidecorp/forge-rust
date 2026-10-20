# ADR 0043: Continuous fuzzing, SBOMs and signed images

- **Status:** Accepted
- **Date:** 2026-10-20
- **Port of:** Forge-Go ADR 0044 (continuous fuzzing, SBOMs and signed images)

The guide is [docs/supply-chain.md](../supply-chain.md). The decision is
language-neutral and matches Forge-Go's; this record maps it to Rust tooling.

> Numbering: 0042 is `forge deploy`, the highest on `main` when this record was
> written. A pull request in flight may also claim 0043; whichever merges second
> renumbers (this file, its entry in the ADR index and the references in
> `README.md`, `docs/`, `fuzz/Cargo.toml`, the workspace `Cargo.toml` and
> `crates/forge/tests/supply_chain.rs`, found with `grep -rn "ADR 0043"`).

## Context

Forge parses hostile or operator-supplied text in many places: database and
mail URLs, `config/deploy.yml`, the credentials file and keys,
`Accept-Language` and translation catalogs, mail addresses and headers,
response bodies checked against an OpenAPI document, trace headers, and the
certificates, timestamps and deploy requests forge-proxy reads. Fuzzing was
named in the [test strategy](../test-strategy.md) and the
[threat model](../threat-model.md) but no fuzz target existed.

Nothing described what the `forge` CLI or forge-proxy contain, and nothing
proved where an image came from: the threat model listed "SBOM and signed
images" as planned, operators had to build forge-proxy themselves, and CI
referenced every action by a mutable major-version tag, which the
[dependency policy](../dependency-policy.md) already required to change
before the first release. Constraints: no new crate in the framework's
dependency graph, no long-lived signing key or other secret in the
repository, and nothing may publish except a deliberate release.

## Decision

1. **Every fuzz target runs, discovered, not listed.** `scripts/fuzz.sh` (the
   options of Forge-Go's script) reads the `[[bin]]` tables of
   `fuzz/Cargo.toml` and fuzzes each target for a fixed time: 10 s per target
   on every pull request, 10 minutes per target nightly on its own runner
   (`fuzz.yml`) with the corpus cached between nights and failing inputs
   uploaded as artifacts. A failing input is a bug: it is fixed and committed
   as a seed.
2. **cargo-fuzz in a standalone package.** The targets use `cargo-fuzz` and
   `libfuzzer-sys` in `fuzz/`, which has its own `[workspace]` and
   `Cargo.lock` and is excluded from the framework workspace like
   `examples/`. cargo-fuzz needs a nightly compiler: it is pinned by date in
   `scripts/fuzz.sh`, installed only by the fuzz jobs, and the rest of the
   repository stays on the stable toolchain. `libfuzzer-sys` is admitted
   under the [dependency policy](../dependency-policy.md#fuzzing-and-supply-chain-tooling-adr-0043)
   as test tooling that is never linked into a released artifact; `cargo
   deny` runs on `fuzz/Cargo.toml` too.
3. **Targets with properties, not just "no panic":** database and mail URLs
   (errors and diagnostics never repeat the password; accepted URLs are
   unambiguous), trusted proxies and client address resolution, the deploy
   YAML reader, the credentials envelope and key text (nothing but a sealed
   file opens; a file that opens re-seals), `Accept-Language`, locale and
   catalog parsing (HTML escaping), mail addresses and encoded messages (no
   injected header), the OpenAPI contract validator, trace context, and
   forge-proxy's certificate `notAfter`, timestamps, durations and deploy
   requests. The reader of `config/deploy.yml` is private to `forge-cli`, so
   its target compiles the same source file instead of widening the CLI's API.
4. **SBOMs from the tools that know each artifact, outside the framework's
   graph.** Binaries: `cargo auditable` embeds the dependency list, Syft reads
   it and writes CycloneDX and SPDX (`scripts/sbom.sh`, CI job `sbom`); a
   second pair describes `Cargo.lock`. Published images: BuildKit attaches an
   SPDX SBOM and SLSA provenance (`mode=max`) to the image index. Syft and
   BuildKit's scanner run from images pinned by digest, Syft with
   `--network none`.
5. **Signed images from tags only.** `release.yml` triggers on `v*` tag pushes
   and nothing else, refuses a tag that is not a version or not on the default
   branch, builds the forge-proxy image (`crates/forge-proxy/Dockerfile`),
   pushes it to ghcr.io and signs the index by digest with cosign, keyless,
   through the job's GitHub OIDC token. Users verify with `cosign verify`
   pinned to this repository's workflow identity and GitHub's issuer. Only that
   job holds `packages: write` and `id-token: write`; every workflow defaults
   to read-only and no workflow that runs on a pull request has a secret.
6. **Pin by SHA and digest, enforce with a test.** Actions are pinned to a
   verified commit SHA with the version in a comment (Forge-Go's pins where it
   uses the same action), workflow containers and forge-proxy's base images by
   index digest; new pins use releases at least a week old.
   `crates/forge/tests/supply_chain.rs` fails `cargo test` when a pin, a
   permission rule, the tag-only trigger or `persist-credentials: false` is
   lost, when copies of the Syft pins disagree, and when a fuzz target is
   undeclared, unseeded or undocumented. Dependabot updates actions, the
   forge-proxy base images and `fuzz/`; OpenSSF Scorecard runs weekly without
   publishing.

## Consequences

- A new `fuzz_targets/*.rs` file is fuzzed on the next pull request and
  nightly without workflow changes, once it has a `[[bin]]` entry, a seed and
  a documentation row (the gate says which is missing). Every pull request
  spends about four minutes fuzzing, plus building the fuzz package.
- Operators can pull a signed forge-proxy instead of building it, and pin the
  digest they verified in `config/deploy.yml`.
- Pins must be refreshed deliberately: Dependabot does not see the digests in
  `run:` steps and scripts, `sbom: generator=` or service images.
- `cargo install --locked` of `cargo-fuzz` and `cargo-auditable` runs the
  crates' own dependency trees, which the framework's `cargo deny` does not
  cover; their versions are pinned and the jobs hold read permissions only.
- The signature proves which workflow built an image from which commit, not
  that the commit is benign; whoever can push a `v*` tag on the default branch
  can publish. A tag ruleset is recommended.

## Differences from Forge-Go

Same contract, with these deliberate differences (tracked in the
[parity matrix](../rails-parity.md#parity-follow-ups)):

- The release publishes forge-proxy for `linux/amd64` only; Forge-Go also
  publishes its example and `linux/arm64`. hello-forge's path dependencies
  leave its Docker context, and an arm64 build under emulation takes hours.
- Binary SBOMs are for `linux/amd64` and also come as SPDX.
- The generated application's CI and release templates (`FORGE_RELEASE_IMAGES`
  opt-in, SBOM artifact) are not ported in this record's pull request.
- Forge-Go's targets for tokens, cursors, UUIDs and `forge db` have no Rust
  counterpart yet; the Rust targets also cover the credentials envelope, the
  deploy YAML reader, mail encoding and forge-proxy's parsers.

## Follow-ups

- Generated applications: pinned actions, a release workflow and a CI SBOM
  artifact in the `forge new` templates, then pin their Dockerfile and compose
  images by digest.
- An arm64 forge-proxy image (a cross-compiling Dockerfile or a native runner)
  and a published example image.
- Attach binary SBOMs to a GitHub release (needs `contents: write` in a
  separate job) instead of keeping them as 90-day artifacts.
- Fuzz targets for storage keys and signed links, stream tokens, timestamps and
  the MFA key and backup-code parsers; forge-proxy's ACME response parsing.
- Publish signed `forge` CLI binaries with SLSA provenance.
