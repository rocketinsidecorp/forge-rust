# ADR 0042: `forge deploy` and forge-proxy

- **Status:** Accepted
- **Date:** 2026-10-20
- **Port of:** Forge-Go ADR 0030 (`forge deploy` and forge-proxy)

The guide is [docs/deployment.md](../deployment.md#forge-deploy); the threat
analysis is the [deployment review](../threat-model.md#deployment-review).

## Context

Generated applications ship a production image, health endpoints, a
`migrate` subcommand and graceful shutdown (ADRs 0008, 0009, 0011), but
getting that image onto servers was left to the operator. Rails 8 answers
with Kamal; Forge-Go answers with `forge deploy`, `forge logs`, `forge app`
and its own reverse proxy, forge-proxy (Forge-Go ADR 0030), which closes
Kamal's weak spots: no command substitution in secrets, an all-or-nothing
switch across hosts, a reviewable proxy in the framework repository, and
stable exit codes for lock contention, failed health checks, failed
migrations, failed hooks and partially reverted states.

Feature parity is an owner rule: the commands, names, flags, configuration
file and behaviour must match, so a project can switch frameworks without
rewriting `config/deploy.yml`. Forge-Rust adds its own rules: hexagonal rings
(deployment is tooling, never domain or application code), bounded and owned
concurrency, no custom cryptography, no shell strings with interpolated
configuration, and no new crate unless strictly necessary.

## Decision

1. **Same contract as Forge-Go.** `forge deploy [init|setup|rollback
   VERSION|prune|details|lock status|acquire|release|maintenance on|off]`,
   `forge logs` and `forge app exec|shell`, with Forge-Go's flags, defaults,
   messages, `--json` reports and exit codes (0, 1, 3 lock held, 4 unhealthy
   and reverted, 5 migration failed, 6 hook failed, 7 inconsistent). The
   remote layout (`~/.forge/SERVICE/{env,lock,deploys.log}`), labels,
   container names and the lock and deploy-log JSON are Forge-Go's: both
   frameworks can operate on the same hosts. The `--dry-run` plans of
   deploy, rollback, prune and setup are byte-for-byte Forge-Go's golden
   plans (tests compare them with the files copied from Forge-Go).

2. **Where it lives.** The orchestration is a module of the CLI ring
   (`forge-cli`, `src/deploy`); forge-proxy is a new outer-ring crate and
   binary (`crates/forge-proxy`). Neither is a dependency of `forge` or of
   generated applications. The CLI uses forge-proxy's Go-compatible
   durations and timestamps so the flags it passes, the lock records and the
   logs round-trip exactly.

3. **Transport without a local shell.** The system `ssh` client runs from an
   argument vector (`std::process::Command`), so `~/.ssh/config`, the agent,
   `known_hosts` and `ProxyJump` apply; the host follows `--`, host keys are
   always verified and `BatchMode=yes` prevents prompts; one `ControlMaster`
   connection per host is shared and closed at the end. The remote side
   necessarily interprets a command line: it is `sh -c` over a script made of
   fixed control flow and configuration values each passed through one
   POSIX quoting function, after strict validation of hosts, images,
   service, role, volume and environment names (the patterns are Forge-Go's,
   compiled with `regex-automata`). Secrets travel on stdin, never in argv,
   never in `ps`. Local `git`, `docker build`/`push` and hooks run the same
   way, from argument vectors.

4. **Configuration.** `config/deploy.yml` keeps Forge-Go's schema, defaults
   and validation messages, decoded strictly (unknown and duplicate keys,
   second documents, non-string durations, tags in `image`, inline
   passwords, option-like hosts, two proxied roles are errors; yaml.v3's
   scalar typing is reproduced). No YAML crate is in the dependency graph,
   so a strict reader in `forge-cli` parses the subset deploy files use —
   block and flow collections, plain and quoted scalars, comments — and
   refuses anchors, aliases, tags, block scalars, complex keys and
   multi-line scalars with a line number rather than guessing.

5. **Concurrency.** Hosts are handled by scoped threads, at most
   `parallelism` at once (sequentially in a dry run), for every deploy
   operation and for `forge logs` without `-f`. `forge logs -f` opens every
   host at once instead: a followed stream never ends, so a host queued
   behind the cap would never be shown. Every child process is waited for
   or killed; a signal (SIGINT/SIGTERM) cancels remote work, and cleanup
   (lock release, revert) runs with its own deadline. `forge logs` closes
   each stream's stdin on Ctrl-C so the remote watchdog stops `docker
   logs`.

6. **forge-proxy in Rust.** hyper (HTTP/1.1) server and per-request client
   connections; one target per service swapped under a short lock; in-flight
   leases counted per target so a switch drains the previous target and no
   request reaches a retired one. Edge header handling, body limits, the
   buffered/streamed body policy, response timeouts, upgrade tunnels,
   maintenance page, JSON access log, Unix control socket (0700/0600), state
   file (atomic, 0600, Forge-Go's format) and command line (Go `flag` syntax)
   are Forge-Go's. Connections, control requests and tunnels are bounded and
   owned (`JoinSet`, `TaskTracker`) and stop within the shutdown budget.
   `-idle-timeout` closes a keep-alive connection waiting for its next
   request (the next request's headers then have `-read-header-timeout`
   from their first byte, Go's `IdleTimeout`/`ReadHeaderTimeout` split;
   before 2026-10-25 hyper's header timer cut that wait at 10 s), a response write
   that makes no progress and an upgraded connection silent in both
   directions, so a client that stops reading cannot hold a connection, a
   tunnel or a drain open. `-max-header-bytes` (default 65536, 4096 to
   1048576, `431` above it; hyper's `max_header_size`, which unlike
   `max_buf_size` accepts values below 8 KiB; before 2026-10-26 the bound was
   hyper's 1 MiB buffer) and `-max-connections` (default 10000 per listener,
   at least 1) are Forge-Go's flags with Forge-Go's names, defaults and
   range. Admission happens before accept: one permit is taken per accept and
   belongs to the connection it returns, a failed accept keeps it for the
   retry, and shutdown ends the loop whether it waits for a permit or for a
   client. The cache and control directories are kept 0700
   (existing ones restricted at start, links refused), and state and cache
   files are written atomically, synced, with the directory synced after
   the rename.

7. **TLS without new dependencies.** Certificates come from an ACME CA (Let's
   Encrypt by default) through an RFC 8555 client written on hyper, rustls
   and ring, all already in the graph: JWS ES256 with ring's ECDSA, a DER
   encoder for the PKCS#10 request (encoding only; every primitive is ring's),
   HTTP-01 challenges answered on port 80. Certificates are obtained ahead of
   time when a TLS service is deployed and renewed 30 days before expiry by
   one owned loop with exponential backoff, whose cache file operations run
   on the blocking pool; a challenge answer is withdrawn however its
   issuance ends. Handshakes never wait for the CA and fail for hosts that
   are not TLS services or have no certificate yet; a certificate is used
   only when its key matches and its subject alternative names cover the
   host.
   rustls serves TLS 1.2 and 1.3. The cache keeps autocert's layout so
   Forge-Go's proxy image can reuse it. A plain-HTTP ACME directory is
   refused except on loopback.

8. **`forge new` is unchanged.** As in Forge-Go, `forge deploy init` writes
   the configuration (with `app_port: 3000`, Forge-Rust's port, and
   `FORGE_MIGRATION_DATABASE_URL`, which `migrate` needs, under `migrate.env`
   so no serving container receives it) instead of every new
   application carrying hosts and registry placeholders.

## Dependencies

No package is added to `Cargo.lock`. Newly direct dependencies, all already
in the graph: `regex-automata` (forge-cli; RE2-compatible patterns and
`forge logs --grep`), `getrandom` (forge-cli), and for forge-proxy hyper's
`client` feature (enabled by forge-telemetry), `rustls`, `tokio-rustls`,
`ring`, `webpki-roots`, `url` and `base64`.

## Consequences

- A project's `config/deploy.yml`, `.forge/secrets` and hooks work with both
  frameworks; the hosts' state and the proxy volume are shared formats.
- The reviewed surface grows by an ACME client and a DER encoder. They are
  integration-tested against an in-test RFC 8555 server that verifies every
  JWS signature and nonce and has its CSRs verified and signed by `openssl`,
  and against Pebble, Let's Encrypt's test CA, when
  `FORGE_TEST_PEBBLE_DIRECTORY` is set (`crates/forge-proxy/tests/tls.rs`
  shows the setup; CI does not start Pebble yet), not against Let's Encrypt
  itself.
- `forge-proxy` is HTTP/1.1 only and opens one application connection per
  request (no pool outlives a switch). HTTP/2 would need the `h2` crate.
- `migrate` runs with the migrate role's env file plus `migrate.env`, as in
  Forge-Go (its issue #57, here #73): the schema owner's
  `FORGE_MIGRATION_DATABASE_URL` goes to the one-off container alone, in its
  own `0600` file on the migrate host that is removed when the step ends,
  and a name in `migrate.env` may not appear in `env` or `servers.*.env`.
  Without `migrate.env` the container gets the role's env file, which has no
  migration credential, and `migrate` requires one.
- The YAML reader refuses valid YAML features Forge-Go's decoder accepts
  (anchors, block scalars); such files must be rewritten in the common
  forms. Floats in integer fields are truncated as yaml.v3 does.
- The orchestration is tested with a recording runner, against Forge-Go's
  golden plans, and over real `sshd` with a docker stand-in
  (`crates/forge-cli/tests/support/fake_docker.rs`); the Docker CLI
  semantics the stand-in models are an assumption checked against Docker's
  documentation, not a daemon, in CI. The forge-proxy image's Dockerfile is
  not built in CI.
