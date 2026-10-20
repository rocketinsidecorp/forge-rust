# Supply chain: fuzzing, SBOM, signed images

How Forge-Rust checks its parsers continuously, describes what it ships, and
signs what it publishes
([ADR 0043](adr/0043-continuous-fuzzing-sbom-signed-images.md)). It is the
port of Forge-Go's guide, with the same workflows, names, pins and commands;
the Rust tooling is `cargo-fuzz` (libFuzzer), `cargo-auditable` with Syft, and
the same cosign signing.

## What runs where

| When | Workflow | What |
| --- | --- | --- |
| Pull request, push to `main` | `ci.yml` job `fuzz` | Every fuzz target, discovered automatically, 10 s each after replaying its seeds. |
| Pull request, push to `main` | `ci.yml` job `sbom` | CycloneDX and SPDX SBOMs of the `forge` and `forge-proxy` binaries and of `Cargo.lock` (artifact `sbom`). |
| Pull request, push to `main` | `ci.yml` jobs `test`, `clippy`, `fmt`, `e2e`, `scaffold` | Includes `crates/forge/tests/supply_chain.rs`, the pin and permission gate. |
| Pull request, push to `main`, daily 06:17 UTC | `supply-chain.yml` | Locked graph, `cargo deny` (advisories; bans, licenses, sources) for the workspace and for `fuzz/`. |
| Nightly 03:17 UTC, or by hand | `fuzz.yml` | Each fuzz target on its own runner for 10 minutes (input `fuzztime`), with its corpus cached between nights. |
| Weekly, Monday 04:41 UTC, or by hand | `scorecard.yml` | OpenSSF Scorecard; results stay in the run (artifact `scorecard`, summary). |
| Push of a `v*` tag, only | `release.yml` | Builds, pushes and signs `ghcr.io/rocketinsidecorp/forge-proxy`; binary SBOMs. |
| Weekly | Dependabot | Cargo (workspace, `examples/hello-forge`, `fuzz/`), GitHub Actions (SHA pins included) and forge-proxy's Docker base images. |

Nothing publishes outside a tag push: no pull request, branch, schedule or
manual run can reach `release.yml`, and it refuses a tag that is not a
version (`v1.2.3`, `v1.2.3-rc.1`) or that points at a commit outside the
default branch.

## Fuzzing

The targets are the parsers that read untrusted or operator-supplied text.
They live in `fuzz/`, a standalone [cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz)
package with its own workspace and `Cargo.lock`, excluded from the framework
workspace: `libfuzzer-sys` (and the C++ libFuzzer it builds) never enters the
framework's dependency graph, its lockfile or a released binary
([dependency policy](dependency-policy.md#fuzzing-and-supply-chain-tooling-adr-0043)).
cargo-fuzz needs a nightly compiler and a C++ toolchain; the nightly is pinned
in `scripts/fuzz.sh` and used only for fuzzing, the rest of the repository
builds on the stable toolchain of `rust-toolchain.toml`.

`scripts/fuzz.sh` reads the `[[bin]]` tables of `fuzz/Cargo.toml` and fuzzes
each target in turn, so a new target is fuzzed in CI without editing a list
(the gate in `crates/forge/tests/supply_chain.rs` fails when a file in
`fuzz/fuzz_targets/` has no entry, no seed corpus or no row below). It
continues after a failure and exits 1 if any target failed. Its options are
Forge-Go's.

```sh
rustup toolchain install "$(scripts/fuzz.sh -T)" --profile minimal   # the pinned nightly
cargo install cargo-fuzz --version 0.13.2 --locked

scripts/fuzz.sh -l                                # list the targets
scripts/fuzz.sh                                   # every target, 10s each (what CI runs)
scripts/fuzz.sh -t 2m config proxy                # targets whose name contains config or proxy, 2 minutes each
scripts/fuzz.sh -t 10m -r '^proxy_duration$'      # one target
scripts/fuzz.sh -t 100000x -r '^deploy_yaml$'     # an iteration count instead of a time
```

Each target replays its committed seeds (`fuzz/seeds/<target>/`) and writes
what it learns to `fuzz/corpus/<target>/` (git-ignored; CI caches it between
nights). A failing input is written to `fuzz/artifacts/<target>/`. It is a
real bug: fix it and move the input to `fuzz/seeds/<target>/` with the fix,
where every later run replays it as a regression test. CI uploads new failing
inputs as the artifact `fuzz-failures` (pull requests) or
`fuzz-failures-<target>` (nightly); to reproduce one:

```sh
gh run download <run-id> --name fuzz-failures-config_database_url --dir /tmp/failure
cargo "+$(scripts/fuzz.sh -T)" fuzz run config_database_url /tmp/failure/fuzz/artifacts/config_database_url/crash-<hash>
cp /tmp/failure/fuzz/artifacts/config_database_url/crash-<hash> fuzz/seeds/config_database_url/   # with the fix
```

Seeds are plain files (NUL separates the fields of the targets that take
several) and are committed on purpose: they cover the grammar of each input
and every malformed shape reviewed so far.

Targets and what each one asserts besides "never panics":

| Target | Parser | Property |
| --- | --- | --- |
| `config_database_url` | `FORGE_DATABASE_URL` and `forge_config::database_url` | Errors, `describe`, `Debug` and serialization never repeat the password; a URL with an endpoint is well formed and has a host without delimiters (the port is not asserted yet, see Findings). |
| `config_mail_url` | `FORGE_MAIL_URL` | Errors, `Debug` and serialization never repeat the password; an accepted SMTP URL has one host without delimiters and a port in 1-65535. |
| `config_trusted_proxies` | `FORGE_HTTP_TRUSTED_PROXIES` and `TrustedProxies::resolve` | The resolved client is the peer or an entry of `X-Forwarded-For`; an untrusted peer is always its own client. |
| `deploy_yaml` | the `config/deploy.yml` reader (`crates/forge-cli/src/deploy/yaml.rs`, ADR 0042) | No panic or stack overflow on any text (depth and size are bounded); parsing is deterministic. |
| `credentials_envelope` | the credentials file and key text (`forge_credentials`, ADR 0041) | Nothing but the sealed seeds opens under the fixed key; a file that opens re-seals and opens again to the same document; key errors never echo the key. |
| `i18n_accept_language` | `Accept-Language` and locale matching (`forge_i18n`) | At most 32 well-formed ranges of letters, digits and `-`; negotiation always returns an available locale. |
| `i18n_canonical_locale` | locale and translation file names | Canonicalization only changes case, is stable, never yields a path or file-name metacharacter, and names its own file. |
| `i18n_catalog` | translation catalogs (`Translator::from_files`) | A file either fails to load or every key translates; interpolated values are HTML-escaped. |
| `mail_message` | `Address::parse` and `forge_mail::encode` | An address prints and parses back to itself; an encoded message is 7-bit clean with CRLF line ends, names only the headers the encoder writes or the allow-list admits (no injected `Bcc`) and its envelope holds the validated recipients. |
| `openapi_validate_response` | the contract check (`Document::validate_response`) | Every response is valid or a `ContractError`, deterministically; a valid `200` for the post route is a JSON object with the schema's `id` and `title` strings. |
| `proxy_certificate_time` | certificate `notAfter` (`forge_proxy::der::not_after`) | No panic or out-of-bounds read; an accepted time lies in the range of the two ASN.1 time types. |
| `proxy_timestamps` | RFC 3339 and `docker ps` timestamps (`forge_proxy::clock`) | A parsed instant prints and parses back to the same instant. |
| `proxy_duration` | Go durations (`forge_proxy::GoDuration`) | No overflow; a parsed duration prints and parses back to the same value. |
| `proxy_deploy_request` | forge-proxy's control-socket request (`DeployRequest::normalize`) | A normalized request has a valid service, a real target port, lowercase unique valid hosts, an absolute health path and bounded durations; normalizing again changes nothing. |
| `telemetry_trace_context` | `traceparent` and `tracestate` (`forge_telemetry::context`) | A parsed `traceparent` is valid and prints canonically; a normalized `tracestate` is bounded and idempotent. |

`deploy_yaml` compiles the reader's own source file (it needs only `std`)
instead of widening `forge-cli`'s private API for tests. The proxy's ACME and
HTTP response parsing is private to `forge-proxy` and is not fuzzed yet; the
certificate and time parsing it feeds are.

## Findings

The first runs of the targets (30 s each) found no panic, overflow or
unbounded recursion in any parser. They found one gap, left for a follow-up
because it is in `forge-config`, outside this change: `FORGE_DATABASE_URL`
and `database_url::endpoint` accept port `0` (`postgres://u:p@host:0/db`),
where Forge-Go refuses ports outside 1-65535. The `config_database_url` seed
`port-zero` covers it; when the fix lands, restore the `port != 0` assertion
in the target. The mail URL, which already refuses port 0, is asserted.
(Four other early failures were wrong properties in the new targets, fixed
before they were committed: an echoed substring of generic error text, a
SQLite file name taken for a credential, the RFC 2047 wire form of a
non-ASCII display name, and the year range of an ASN.1 `GeneralizedTime`.)

## SBOMs

Two formats, from the tools that know each artifact best, none of them a
dependency of the framework:

- **Binaries** (`forge`, `forge-proxy`): CycloneDX JSON and SPDX JSON from
  Syft, read from the dependency list [`cargo auditable`](https://github.com/rust-secure-code/cargo-auditable)
  embeds in each binary, so they list exactly the crates linked and their
  versions. `scripts/sbom.sh` builds the binaries like a release (`--locked`,
  the release profile) and runs Syft from its image, pinned by digest, with
  `--network none`.
- **The workspace**: a CycloneDX and SPDX pair of `Cargo.lock`, which lists
  every package any feature can pull in (a superset of any one binary).
- **Published images**: an SPDX SBOM attestation that BuildKit's Syft
  scanner (pinned by digest) attaches to the image index, with SLSA
  provenance (`mode=max`), during the release build.

```sh
# binaries and Cargo.lock, locally (Docker, or a local Syft with SYFT=/path/to/syft)
cargo install cargo-auditable --version 0.7.7 --locked
scripts/sbom.sh dist/sbom
jq -r '.components[] | "\(.name) \(.version)"' dist/sbom/forge-proxy.cdx.json
jq -r '.packages[] | "\(.name) \(.versionInfo)"' dist/sbom/forge-proxy.spdx.json

# from CI or a release
gh run download <run-id> --name sbom --dir dist/sbom

# a published image: SBOM and provenance of the linux/amd64 variant
docker buildx imagetools inspect ghcr.io/rocketinsidecorp/forge-proxy:v0.1.0 \
  --format '{{ json (index .SBOM "linux/amd64").SPDX }}' | jq -r '.packages[] | "\(.name) \(.versionInfo)"'
docker buildx imagetools inspect ghcr.io/rocketinsidecorp/forge-proxy:v0.1.0 \
  --format '{{ json (index .Provenance "linux/amd64").SLSA }}'
```

## Signed images

`release.yml` runs only when a `v*` tag is pushed. The forge-proxy image is
built for `linux/amd64`, pushed to ghcr.io with provenance and SBOM, and its
index is signed by digest with Sigstore cosign. Signing is keyless: the job's
GitHub OIDC token gets a short-lived Fulcio certificate that names this
repository, the workflow file and the tag, and the signature is logged in
Rekor. There is no signing key or secret in the repository. The job verifies
its own signature before it ends.

```sh
git tag v0.1.0 && git push origin v0.1.0   # a maintainer cuts a release
```

Verify before you deploy (cosign v3 or later):

```sh
cosign verify ghcr.io/rocketinsidecorp/forge-proxy:v0.1.0 \
  --certificate-identity-regexp '^https://github\.com/rocketinsidecorp/forge-rust/\.github/workflows/release\.yml@refs/tags/v' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com

# pin exactly what you verified, by digest, in config/deploy.yml (proxy.image)
docker buildx imagetools inspect ghcr.io/rocketinsidecorp/forge-proxy:v0.1.0 --format '{{ .Manifest.Digest }}'
```

To accept exactly one release, replace the regular expression with
`--certificate-identity https://github.com/rocketinsidecorp/forge-rust/.github/workflows/release.yml@refs/tags/v0.1.0`.
A signature made by any other repository, workflow, branch or pull request
does not match either form.

## Pinning policy

- Every third-party action is pinned to a full commit SHA with the version in
  a comment (`uses: actions/checkout@3d3c42e5… # v7.0.1`). Each SHA was
  checked against the tag in the action's repository (`git ls-remote`,
  dereferencing annotated tags). Where Forge-Go uses the same action, the pin
  is Forge-Go's.
- Every container a workflow runs (Syft, BuildKit's scanner, the CI database
  service) and the base images of `crates/forge-proxy/Dockerfile` are pinned
  by the digest of their multi-architecture index.
- New pins use releases at least a week old, so a compromised release has
  time to be noticed and withdrawn before Forge runs it. The same holds for
  the tool versions installed with `cargo install --locked`
  (`cargo-fuzz 0.13.2`, `cargo-auditable 0.7.7`) and the nightly date in
  `scripts/fuzz.sh`.
- `crates/forge/tests/supply_chain.rs` fails `cargo test --workspace` when an
  action is not pinned by SHA with a version comment, a workflow container or
  service image or a Dockerfile base image is not pinned by digest, copies of
  the Syft pins differ, forge-proxy's Rust tag differs from
  `rust-toolchain.toml`, a checkout keeps its credentials, a workflow lacks a
  top-level `permissions:`, uses `pull_request_target`, asks for write access
  to the contents, reads a secret other than the release job's token, or holds
  `packages: write` or `id-token: write` outside `release.yml`; when the
  release workflow gains a trigger other than `v*` tags or loses its tag
  guard, signing or provenance; and when a fuzz target is not declared,
  seeded and documented, or `fuzz/` stops being a standalone workspace.
- Permissions are read-only by default (`contents: read`, or `{}` in
  `release.yml` and `scorecard.yml`); only the release image job holds
  `packages: write` and `id-token: write`. No workflow that runs on a pull
  request has a secret.

The pins in use:

| Pin | Version | Verified against |
| --- | --- | --- |
| `actions/checkout` | v7.0.1 | tag in the action's repository |
| `actions/upload-artifact` | v7.0.2 | tag |
| `actions/cache` | v6.1.0 | tag |
| `Swatinem/rust-cache` | v2.9.2 | tag (v2.9.3 was under a week old) |
| `EmbarkStudios/cargo-deny-action` | v2.1.1 | tag |
| `docker/setup-buildx-action` | v4.4.1 | tag |
| `docker/login-action` | v4.6.0 | tag |
| `docker/build-push-action` | v7.4.0 | tag |
| `sigstore/cosign-installer` | v4.1.2 | tag |
| `ossf/scorecard-action` | v2.5.0 | tag |
| `ghcr.io/anchore/syft` | v1.54.1 | registry digest of the tag |
| `docker.io/docker/buildkit-syft-scanner` | stable-1 | registry digest of the tag |
| `postgres` (CI service) | 18.6-trixie | registry digest of the tag |
| `rust` (forge-proxy build) | 1.98.1-slim-trixie | registry digest of the tag |
| `gcr.io/distroless/cc-debian13` (forge-proxy runtime) | nonroot | registry digest of the tag |

To update a pin by hand:

```sh
git ls-remote https://github.com/actions/checkout 'refs/tags/v7.0.1' 'refs/tags/v7.0.1^{}'   # the ^{} line, if any, is the commit
docker buildx imagetools inspect ghcr.io/anchore/syft:v1.54.1 --format '{{ .Manifest.Digest }}'
docker buildx imagetools inspect rust:1.98.1-slim-trixie --format '{{ .Manifest.Digest }}'
```

Dependabot proposes new SHAs for actions and digests for the forge-proxy
Dockerfile. It does not update the digests inside workflow `run:` steps or
scripts (Syft), `sbom: generator=` (BuildKit's scanner) or the CI service
image; refresh those by hand, all copies at once (the test requires the Syft
copies to agree).

## Residual risks

- `ossf/scorecard-action` runs its image by tag (its `action.yaml` names a
  `ghcr.io/ossf/scorecard-action` tag); the job is read-only and publishes
  nothing. `EmbarkStudios/cargo-deny-action` builds a Docker image from its
  own Dockerfile and downloads a release of `cargo-deny` in it; the jobs hold
  read permissions only.
- The `sigstore/cosign-installer` pin chooses the cosign version and verifies
  its checksum; Syft inside BuildKit's scanner image is whatever that digest
  holds.
- `cargo install --locked` of `cargo-fuzz` and `cargo-auditable` builds the
  crate versions their own lockfiles name, downloaded from crates.io and
  checked against the index checksums; the versions are pinned, the crates'
  dependency trees are not part of the framework's `cargo deny` gate.
- The nightly toolchain is downloaded by rustup from static.rust-lang.org,
  pinned by date, and runs only in the fuzz jobs.
- A maintainer who can push `v*` tags can publish a signed image from any
  commit: the default-branch guard runs from the tagged commit's own
  `release.yml`, so a tag on a commit that removed it skips it. Protect `v*`
  tags with a repository ruleset, and when verifying an image also check that
  the certificate's source commit is on `main`.
- Signatures prove which workflow built an image from which commit, not that
  the commit is benign: review still guards `main`.
- Binary SBOMs are workflow artifacts (90 days for releases); they are not
  attached to a GitHub release.
- The generated application's Dockerfile, compose database images, CI and
  release templates are not covered by these pins yet (follow-up in the
  [parity matrix](rails-parity.md#parity-follow-ups)); the forge-proxy image is
  `linux/amd64` only.
