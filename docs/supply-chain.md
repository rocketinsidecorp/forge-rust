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
| Pull request, push to `main` | `ci.yml` jobs `test`, `clippy`, `fmt`, `e2e`, `scaffold`, `database`, `coverage` | Includes `crates/forge/tests/supply_chain.rs`, the pin and permission gate; `database` runs the ignored database tests, `coverage` gates line coverage (docs/test-strategy.md). |
| Pull request, push to `main`, daily 06:17 UTC | `supply-chain.yml` | Locked graph, `cargo deny` (advisories; bans, licenses, sources) for the workspace and for `fuzz/`. |
| Nightly 03:17 UTC, or by hand | `fuzz.yml` | Each fuzz target on its own runner for 10 minutes (input `fuzztime`), with its corpus cached between nights. |
| Weekly, Monday 04:41 UTC, or by hand | `scorecard.yml` | OpenSSF Scorecard; results stay in the run (artifact `scorecard`, summary). |
| Push of a `v*` tag, only | `release.yml` | Builds, pushes and signs `ghcr.io/rocketinsidecorp/forge-proxy`; binary SBOMs. |
| Weekly | Dependabot | Cargo (workspace, `examples/hello-forge`, `fuzz/`), GitHub Actions (SHA pins included) and forge-proxy's Docker base images. |

Nothing publishes outside a tag push: no pull request, branch, schedule or
manual run can reach `release.yml`, and it refuses a tag that is not a
version (`v1.2.3`, `v1.2.3-rc.1`), that differs from the workspace version
(`v` + `version` of `[workspace.package]` in `Cargo.toml`), or that points at
a commit outside the default branch.

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
| `http_query` | query strings (`forge_http::Request::query`) | The strict grammar written out a second time agrees with the decoder on every input (accepted or not, each name and value, which refusal comes first); a decoded query survives re-encoding; into `()` only the empty query is accepted; a typed target with `deny_unknown_fields` accepts exactly what its field types accept. |
| `http_form` | form bodies (`forge_http::Request::form` and `form_value`) | The same differential check as `http_query` plus the form rules: the `Content-Type` check, at most 1000 pairs, repeated names only for sequences, checkbox and blank-optional values, and `csrf_token` and `_method` never reaching the application's type. |
| `http_router` | route patterns and path matching (`forge_http::Router`) | The pattern grammar (whole-segment `{name}` and last-segment `{name...}`), conflicts, the precedence of static text over `{name}` over `{name...}` and the captures agree with a second implementation; a path with a dot segment, an encoded separator or a backslash never reaches a handler; a capture never holds a `.` or `..` segment or a backslash, and a catch-all never starts with `/` or holds `//`. |
| `i18n_accept_language` | `Accept-Language` and locale matching (`forge_i18n`) | At most 32 well-formed ranges of letters, digits and `-`; negotiation always returns an available locale. |
| `i18n_canonical_locale` | locale and translation file names | Canonicalization only changes case, is stable, never yields a path or file-name metacharacter, and names its own file. |
| `i18n_catalog` | translation catalogs (`Translator::from_files`) | A file either fails to load or every key translates; interpolated values are HTML-escaped. |
| `mail_message` | `Address::parse` and `forge_mail::encode` | An address prints and parses back to itself; an encoded message is 7-bit clean with CRLF line ends, names only the headers the encoder writes or the allow-list admits (no injected `Bcc`) and its envelope holds the validated recipients. |
| `openapi_validate_response` | the contract check (`Document::validate_response`) | Every response is valid or a `ContractError`, deterministically; a valid `200` for the post route is a JSON object with the schema's `id` and `title` strings. |
| `proxy_certificate_time` | certificate `notAfter` (`forge_proxy::der::not_after`) | No panic or out-of-bounds read; an accepted time lies in the range of the two ASN.1 time types. |
| `proxy_timestamps` | RFC 3339 and `docker ps` timestamps (`forge_proxy::clock`) | A parsed instant prints and parses back to the same instant. |
| `proxy_duration` | Go durations (`forge_proxy::GoDuration`) | No overflow; a parsed duration prints and parses back to the same value. |
| `proxy_deploy_request` | forge-proxy's control-socket request (`DeployRequest::normalize`) | A normalized request has a valid service, a real target port, lowercase unique valid hosts, an absolute health path and bounded durations; normalizing again changes nothing. |
| `storage_key` | storage keys (`forge_storage::StorageKey::parse` and `unique`) | The accepted keys are exactly the documented grammar; a key is relative, has no empty, `.`, `..` or hidden segment, no backslash, NUL or percent escape, and only descends when joined to a directory; every refusal is the same error. |
| `storage_multipart` | uploads (`forge_storage::receive`) | A refused upload leaves the store empty; an accepted one stores at most `max_files` non-empty files within the size bound, under UUID keys below the prefix, as a type that is allowed and agrees with the bytes, as slices of the request body, with a sanitized file name; the text fields stay within their bounds; the same body gets the same verdict twice. |
| `telemetry_trace_context` | `traceparent` and `tracestate` (`forge_telemetry::context`) | A parsed `traceparent` is valid and prints canonically; a normalized `tracestate` is bounded and idempotent. |

`deploy_yaml` compiles the reader's own source file (it needs only `std`)
instead of widening `forge-cli`'s private API for tests. The proxy's ACME and
HTTP response parsing is private to `forge-proxy` and is not fuzzed yet; the
certificate and time parsing it feeds are.

## Findings

The first runs of the targets (30 s each) found no panic, overflow or
unbounded recursion in any parser. They found one gap, fixed in the same
change: database URLs with port `0` (`postgres://u:p@host:0/db`, or
`?port=0`) were well formed and had an endpoint, where Forge-Go refuses ports
outside 1-65535. They are now malformed, with no endpoint; the
`config_database_url` seed `port-zero` keeps it covered and the target asserts
the endpoint port is never 0. The mail URL, which already refused port 0, is
asserted too.
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
  (`cargo-fuzz 0.13.2`, `cargo-auditable 0.7.7`, `cargo-llvm-cov 0.9.1`) and
  the nightly date in `scripts/fuzz.sh`.
- CI installs those tools through the local action
  `.github/actions/cargo-tool` (`tool:` and an exact `version:`), which
  installs into a directory named after the version, caches it under a key
  that holds the version and puts it first on `PATH`: bumping a version
  always installs it, where the former `command -v tool || cargo install`
  kept whatever rust-cache restored. The release job installs without a
  cache.
- Only allowlisted actions run (`ALLOWED_ACTIONS` in
  `crates/forge/tests/gate/rules.rs`, compared without case):
  `actions/checkout`, `actions/cache`, `actions/upload-artifact`,
  `Swatinem/rust-cache`, `EmbarkStudios/cargo-deny-action`,
  `docker/setup-buildx-action`, `docker/login-action`,
  `docker/build-push-action`, `ossf/scorecard-action` and
  `sigstore/cosign-installer`, plus local `./` actions. A SHA pin is not
  enough: an installer action (`taiki-e/install-action`,
  `baptiste0928/cargo-install`, ...) is refused however it is pinned, and
  `docker://` actions are refused. To add an action, review its code (and
  what it downloads), pin it by SHA with its version comment, and add it to
  `ALLOWED_ACTIONS` and to the pin table below in the same reviewed change;
  the gate also fails when an allowlisted action is no longer used. A
  sub-path cannot leave the repository (no empty, `.` or `..` segment).
- The generated application's workflows are gated as `forge new` writes
  them, not as the template reads: `crates/forge-cli/src/generate/supply_chain.rs`
  generates an application for every option (each `-d` backend and none,
  with and without Docker, full-stack and `--api`, two names) and runs every
  rule below on every file under its `.github/`. A `%%if%%` branch cannot hide
  a step: a line that is `run: |` text in one render and a step in another is
  read as both. The framework test also reads the template with its
  `%%...%%` markers blanked line-aligned, all branches at once; that merged
  reading is an extra check, not the guarantee, since no render produces it.
- Gated files are written in a simple YAML subset. The gate reads the
  workflows, local actions and rendered generated-app files line by line, and
  refuses whatever such a reader could misread rather than guess:
  - a carriage return not followed by a line feed, U+0085, U+2028 or U+2029,
    and a byte-order mark anywhere but at the start;
  - a YAML directive (`%YAML`), content after a document marker (`---`,
    `...`), and a document root that does not start at column 0;
  - outside block scalars (`run: |`), an anchor (`&`), alias (`*`), tag (`!`,
    `!!`), explicit key (`?`) or merge key (`<<`) where any key or value
    starts (`${{ !cancelled() }}`, quoted strings and `run: |` text stay
    allowed);
  - a plain scalar continued on a deeper line: write a multi-line expression
    as a `>-` block scalar;
  - a flow collection (`[...]`, `{...}`) or quoted scalar that does not close
    on its line, a flow indicator or a `:` without a blank inside a flow
    collection's plain scalar, a quoted key whose `:` is not followed by a
    blank;
  - a backslash in any double-quoted scalar, key or value: an escape spells
    text the rules would not read (`"u\x73es"` is `uses`, `"c\x61rgo
    install"` is `cargo install`, `"CARGO_\x48OME"` is `CARGO_HOME`). Plain
    and single-quoted scalars and block text have no escapes, so the text
    the rules read is the text YAML yields.
  A `#` inside a quoted scalar is text, not a comment.
- Every gated file is also parsed with a YAML parser (`yaml-rust2`, a
  test-only dependency, docs/dependency-policy.md): the parser and the line
  reader must find the same `uses` keys, the same values on the same lines
  (block-scalar text is not read for keys, so a decoy there cannot stand in
  for a key the reader misses), and the checkout (`with:
  persist-credentials: false` in the step itself) and `permissions` rules
  read the parsed tree, not the text. No mapping repeats a key, compared
  without case (`persist-credentials` and `Persist-Credentials`, a second
  `uses`): readers that keep the first and the last value would disagree.
- Closed allowlists decide what a gated file may set, write, select and
  pass (see Threat model and residual risk below). As a second, independent
  check, no gated file names a variable that redirects where cargo or rustup
  fetch from or what they run (`CARGO_HOME`, `CARGO_REGISTRIES_*`,
  `CARGO_REGISTRY_*`, `CARGO_SOURCE*`, `CARGO_INSTALL_ROOT`, `CARGO_NET_*`,
  `CARGO_CONFIG*`, `CARGO_HTTP_*`, `RUSTC_WRAPPER`,
  `RUSTC_WORKSPACE_WRAPPER`, `RUSTUP_DIST_SERVER`, `RUSTUP_DIST_ROOT`,
  `RUSTUP_UPDATE_ROOT`, `XDG_CONFIG_HOME`, `ACTIONS_ALLOW_UNSECURE_COMMANDS`)
  anywhere, in any case; `HOME` and `RUSTC` are refused where set; and
  `$GITHUB_ENV`, `::set-env`, `::add-path`, the runner's command-file paths
  and cargo configuration file names (`.cargo/config`, `cargo/config.toml`)
  are refused anywhere.
- The install rule reads `run` text as text: with and without YAML comments
  removed, continued lines joined with and without a space, quotes and
  backslashes ignored. It refuses ANSI-C quoting (`$'...'`), any command
  whose program is a variable (`c=cargo; $c install`) or a path to cargo
  when it installs, and every `.cargo/config` or `.cargo/config.toml` in the
  repository, symbolic links included (cargo reads them from every parent
  directory; one could redirect crate sources or alias `install`).
- What text scanning cannot see: the shell inside `run` is not parsed (a
  script the workflow runs, `eval` of computed text, a command or variable
  name assembled at run time), and an allowlisted action or a reviewed
  script can still fetch a tool. Those are covered by review of every script
  and allowlisted action, not by the gate (see Threat model and residual
  risk below).
- Cargo tools are installed only through that action, plus one reviewed
  exception: the `Install cargo-auditable` step of `release.yml`, which
  installs exactly `cargo-auditable`. Both use one exact `--version` and
  `--locked`, from crates.io. The gate refuses any
  other install in a workflow or action, however it is spelled (a path to
  cargo, `$CARGO`, options before `install`, `binstall`, `cargo-binstall`,
  `CARGO_ALIAS_*`, continued or folded lines), and any probe that would skip
  an install because some binary is already there.
- `crates/forge/tests/supply_chain.rs` fails `cargo test --workspace` when an
  action is not pinned by SHA with a version comment, a workflow container or
  service image or a Dockerfile base image is not pinned by digest, copies of
  the Syft pins differ, forge-proxy's Rust tag differs from
  `rust-toolchain.toml`, a checkout keeps its credentials, a workflow lacks a
  top-level `permissions:`, uses `pull_request_target`, asks for write access
  to the contents, reads a secret other than the release job's token, or holds
  `packages: write` or `id-token: write` outside `release.yml`; when the
  release workflow gains a trigger other than `v*` tags or loses its tag,
  workspace-version or default-branch guard, signing or provenance; when a
  fuzz target is not declared, seeded and documented, or `fuzz/` stops being
  a standalone workspace; when a `deny.toml` exception is past its
  `expires` date (the libfuzzer-sys NCSA exception must carry one); when a
  cargo tool is installed anywhere but the cargo-tool action and the
  reviewed release step, or there without one exact `--version` and
  `--locked`; when a
  workspace crate does not take `version.workspace = true` (so the release
  tag check covers every crate); and when the coverage job or
  `.coverage-floor` goes missing.
  Each rule is a function with negative tests on hand-written snippets, and
  the rules see through the spellings that once slipped past: flow style
  (`- { uses: ... }`, `permissions: { contents: write }`), anchors and
  aliases, `docker://` actions, a local `./` action's `action.yml`,
  `Actions/Checkout` in any case (checked per step, not counted),
  `SECRETS.X`, `secrets:  inherit`, `join(secrets, ...)`, comments after a
  tab. An `image:` is a container only under `services:` or `container:`; an
  action input under `with:` is not.
- The generated application's CI (`templates/app/github/workflows/ci.yml.tmpl`,
  gated as rendered for every option),
  `Dockerfile.tmpl` (and `examples/hello-forge/Dockerfile`) and
  `compose.yaml.tmpl` follow the same rules: actions pinned by SHA,
  checkouts without credentials, both base images by the digests of
  `crates/forge-proxy/Dockerfile`, and the PostgreSQL, MySQL and MariaDB
  images by the same digests as `.github/workflows/ci.yml`.
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
| `postgres` (`database` job) | 16.14-trixie | registry digest of the tag (Docker Hub, pushed 2026-08-05) |
| `mysql`, `mariadb` (`database`, `coverage` jobs) | 8.4, 11.8 | Forge-Go's pins |
| `cargo-llvm-cov` | 0.9.1 | crates.io, 2026-09-06, not yanked |
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

## Threat model and residual risk

Engineering decisions D6 and D8 (2026-10-25). The supply-chain gate is
defense in depth against unreviewed drift and casual injection in the
workflows, local actions and generated-app files. It is not a shell sandbox:
review of every change is the primary control. Since D8 the gate's core is a
set of closed allowlists in `crates/forge/tests/gate/rules.rs`, so it
converges instead of chasing new redirects; growing a list is a reviewed
change to that file.

- Variables (`ALLOWED_VARIABLES`): a gated file sets only listed names, in an
  `env:` key at any level (workflow, job, step, container, service; plain,
  quoted or flow) or in `run` text (`NAME=`, `NAME+=`, `NAME[i]=`, `export`,
  `env`, `sudo`, `declare`, `typeset`, `readonly`, `local`, `read`,
  `mapfile`, `getopts`, `let`, `printf -v`, `for`/`select`/`coproc NAME`,
  `${NAME:=...}`, `{NAME}>`, arithmetic). `run` text is read in every
  reading bash could give it (`shell_lines`): physical lines, lines joined
  as bash joins a backslash-newline continuation, joined with a space, each
  with and without comments, so a target split from its `=` by a
  continuation is still read. Names a tool reads as flags or a
  program (`CARGO_INCREMENTAL`, `CARGO_TERM_COLOR`, `RUST_BACKTRACE`,
  `FORGE_TEST_FAKE_DOCKER`, ...) take one exact value. `HOME`, `PATH`,
  `BASH_ENV`, `LD_PRELOAD`, `RUSTUP_*`, `RUSTFLAGS` and every other unlisted
  name are refused as assignment targets; reading `$HOME` stays allowed. An
  `env:` or `with:` that is a whole expression is refused, and `shell:` is
  bash only.
- Runner files (`RUNNER_FILE_LINES`): `$GITHUB_ENV`, `$GITHUB_PATH` and
  `$GITHUB_OUTPUT` are written only on the listed lines (the fuzz matrix and
  release image name outputs, the cargo-tool action's `bin/` on `PATH`),
  each a complete logical line: a listed line continued from or into
  another is refused, and so is every other line that names a runner file,
  in any reading, a heredoc or a `$( )` included;
  `::set-env`, `::add-path`, `ACTIONS_ALLOW_UNSECURE_COMMANDS` and the
  runner's command-file paths are refused.
- Expressions (`EXPRESSION_LINES`): `format`, `fromJSON`, `join` and `toJSON`
  appear only on the listed line (the fuzz matrix).
- Toolchains and flags (`ALLOWED_TOOLCHAINS`, empty; `ALLOWED_RUSTUP`):
  no `cargo`/`rustc`/`rustdoc +toolchain`, no cargo `--config`, no `-Z`
  flag, rustup only as `toolchain install` and `component add`, and no
  `linker`, `link-arg`, `codegen-backend` or `runtool` anywhere.
- Action inputs: allowlisted actions get exact versions (`go-version` X.Y.Z
  or `go-version-file: go.mod`, golangci-lint `version` vX.Y.Z,
  `cosign-release` and setup-buildx `version` vX.Y.Z when set), no
  `check-latest`, `driver-opts` images by digest, and no `latest` anywhere.
- Job containers and services (`CONTAINER_KEYS`,
  `ALLOWED_CONTAINER_OPTIONS`): only `image`, `env` (under
  `ALLOWED_VARIABLES`), numeric `ports` and `options`; no `volumes`,
  `credentials`, `entrypoint` or `command`. `options:` is exactly one of the
  listed database health checks, and `-e`, `--env`, `--env-file`, `-v`,
  `--volume`, `--mount`, `--entrypoint`, `--privileged`, `--network` and
  `--cap-add` are refused in it.
- Tool configuration files (`tool_configuration_files`): none anywhere in
  the repository (templates included) or in any generated application, for
  every `forge new` option: `.cargo/config` and `.cargo/config.toml`, a
  `rust-toolchain` or `rust-toolchain.toml` whose content is not exactly
  one of `ALLOWED_TOOLCHAIN_FILES` (rustup reads quoted and inline-table
  `path` spellings and legacy plain-text names, so content is listed, not
  parsed; a toolchain bump updates the list), `go.work`, `go.work.sum`, `go.env`,
  `go/env`, `.bashrc`, `.profile`, `.bash_profile`, `.envrc`.
- The YAML-level rules still apply underneath: the simple subset (no
  escapes, anchors, tags, hidden line breaks), the parser cross-check,
  duplicate keys, template branches gated as rendered, and the denylist of
  redirecting variables as a second, independent check.

Accepted residual risk, left to review: names the shell builds at run time
(`eval`, a `printf`-built or `$(...)`-built name), the content of the scripts
the workflows call (`scripts/*.sh`), and what an allowlisted action does
internally with its pinned inputs. Reviews of the gate judge it against this
model: a bypass inside the lists' scope is a defect to fix in the gate.

## Residual risks

- `ossf/scorecard-action` runs its image by tag (its `action.yaml` names a
  `ghcr.io/ossf/scorecard-action` tag); the job is read-only and publishes
  nothing. `EmbarkStudios/cargo-deny-action` builds a Docker image from its
  own Dockerfile and downloads a release of `cargo-deny` in it; the jobs hold
  read permissions only.
- The `sigstore/cosign-installer` pin chooses the cosign version and verifies
  its checksum; Syft inside BuildKit's scanner image is whatever that digest
  holds.
- `cargo install --locked` of `cargo-fuzz`, `cargo-auditable` and
  `cargo-llvm-cov` builds the
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
