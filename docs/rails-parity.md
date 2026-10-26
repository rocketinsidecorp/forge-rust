# Rails parity

Forge exists in two frameworks, Forge-Go and Forge-Rust, that ship the same
capabilities with the same commands, names and conventions (owner decision,
2026-10-20). This matrix tracks Ruby on Rails 8 capabilities and the places
where Forge intends to go further. A row is **Done** only when code, tests and
README commands exist on `main`; **In review** means a pull request is open or
the work is finished and waiting for its review; **Planned** means not started.

| Capability | Rails 8 | Forge-Go | Forge-Rust |
| --- | --- | --- | --- |
| New application with Docker and CI | `rails new` | Done | Done |
| Database selection | `rails new -d` | Done (PostgreSQL, MySQL, MariaDB, SQLite) | Done (PostgreSQL, MySQL, MariaDB, SQLite) |
| Server-rendered HTML, layouts, partials, forms, flash, CSRF | Action View | Done | Done |
| HTML + JSON CRUD generator | `rails g scaffold` | Done | Done |
| Authentication pages, password reset | `rails g authentication` | Done | Done |
| First tenant and owner, seeds | `db:seed` | Done (`forge setup`, `forge seed`) | In review (`forge setup`, `forge seed`; the application commands `setup` and `seed` are Done) |
| Multi-tenancy with database enforcement | (gems) | Done (PostgreSQL RLS) | Done (PostgreSQL RLS) |
| TOTP MFA | (gems) | Done | Done (`FORGE_AUTH_MFA_KEY`, `/session/mfa`, `/mfa`, [ADR 0040](adr/0040-totp-multi-factor-authentication.md), [MFA](mfa.md)) |
| Persistent jobs, cron, outbox | Solid Queue | Done | Done (framework crates); In review (`forge new` wires the job queue into PostgreSQL applications: `jobs` feature, `.jobs(WorkerMode::InServe)`, `jobs_configure`) |
| Mailer with previews | Action Mailer | Done | Done (`forge::mail`, password-reset emails, `/_mail` previews, [ADR 0035](adr/0035-mail.md); `forge generate mailer`, [ADR 0038](adr/0038-generators-and-destroy.md)) |
| Rate limiting | `rate_limit` | Done | Done |
| Sign-in, reset and MFA throttles shared by replicas | `rate_limit` with a shared cache store | Done (`auth/postgres` `LoginThrottler`, `forge_login_throttle`) | In review (`forge::auth::PgLoginThrottle`, same table and transaction, wired in generated PostgreSQL applications; see follow-ups) |
| Bounded memory-hard password hashing | — | Not needed (PBKDF2, no per-hash memory) | In review (`forge::auth::PasswordWorkLimiter`: one Argon2id hash per CPU, at most 16, then `503`) |
| Client address behind trusted proxies | `request.remote_ip`, `trusted_proxies` | Done (`FORGE_HTTP_TRUSTED_PROXIES`, ADR 0037) | Done (`FORGE_HTTP_TRUSTED_PROXIES`, ADR 0032; `forge deploy` sets it for the proxied role, [ADR 0042](adr/0042-forge-deploy.md)) |
| `HEAD` on every `GET` route | `HEAD` routed to `GET` | Done (`http.ServeMux`) | In review (the router serves `HEAD` with the `GET` route unless a `HEAD` route exists, the server drops the body, `Allow` lists `HEAD`) |
| Handler panic answered with `500` | `ShowExceptions` | Done (`recoverer`: JSON error body with the request id) | In review (text body `internal server error`, `x-request-id`, logged with the request id) |
| HSTS behind TLS | `config.force_ssl` | Done (`Strict-Transport-Security` unless `FORGE_HTTP_TRANSPORT=plain`) | In review (same header and rule, `FORGE_HTTP_TRANSPORT=plain\|trusted-proxy`, see follow-ups) |
| Verified database TLS in production, pool limits | `sslmode` in `database.yml`, `pool` | Done (`sslmode=verify-full\|verify-ca`, MySQL `tls=true`, `FORGE_DATABASE_ALLOW_PLAINTEXT`, `FORGE_DATABASE_MAX_CONNS\|MIN_CONNS\|MAX_CONN_LIFETIME\|MAX_CONN_IDLE_TIME\|CONNECT_TIMEOUT\|STATEMENT_TIMEOUT`) | In review (stricter TLS rule: `sslmode=verify-full`, SQLx's MySQL spelling `ssl-mode=VERIFY_IDENTITY`; `verify-ca` refused, see follow-ups; `FORGE_DATABASE_ALLOW_PLAINTEXT`, `FORGE_DATABASE_MAX_CONNS\|CONNECT_TIMEOUT\|STATEMENT_TIMEOUT`; see follow-ups) |
| Bound on buffered request bodies | — | Not needed (bodies are streamed through `MaxBytesReader`) | In review (`FORGE_MAX_IN_FLIGHT_BODY_BYTES`, 256 MiB for all bodies at once, `503` with `Retry-After`) |
| Cache | Solid Cache | Done | Done |
| File storage with signed URLs | Active Storage | Done (disk) | Done (disk) |
| Import maps, Turbo, Stimulus | Hotwire | In review | Done |
| Turbo Streams over SSE | Action Cable | In review | Done (`scaffold --broadcast`, ADR 0031) |
| Traces, metrics, log correlation | (gems) | Done (OpenTelemetry, Prometheus) | Done (OpenTelemetry, Prometheus; ADR 0033) |
| Preconfigured Prometheus, Grafana, Tempo, Loki | — | Done (`compose.observability.yaml`, Forge-Go ADR 0038) | Done (`compose.observability.yaml`, [ADR 0034](adr/0034-local-observability-stack.md)) |
| Zero-downtime deploys, rollback, remote logs | Kamal | Done (`forge deploy`, `forge logs`, `forge-proxy`) | In review (`forge deploy`, `forge logs`, `forge app`, `forge-proxy` with ACME TLS, same `config/deploy.yml`, [ADR 0042](adr/0042-forge-deploy.md), [deployment](deployment.md#forge-deploy)) |
| Live reload and database lifecycle | `bin/dev`, `db:*` | In review (`forge dev`, `forge db`) | In review (`forge dev`, `forge db create\|drop\|reset\|setup\|prepare\|migrate\|rollback\|status\|seed`, `forge migrate\|rollback\|setup\|seed`, `forge doctor`, `forge version`, [ADR 0036](adr/0036-application-lifecycle-commands.md)) |
| Encrypted credentials, attribute encryption | `credentials:edit`, AR Encryption | Done (`forge credentials`, `encryption`) | Done (`forge credentials init\|edit\|show\|rotate-key`, `forge::credentials`, `forge::encryption`, [ADR 0041](adr/0041-encrypted-credentials-and-attribute-encryption.md), [credentials](credentials.md), [encryption](encryption.md)) |
| i18n | I18n | Done (`config/locales/*.json`, `t`, CLDR plurals, `forge new --locale`, `forge i18n check`, Forge-Go ADR 0042) | Done (`config/locales/*.json`, `layout.i18n.t`, CLDR plurals, `forge new --locale`, `forge i18n check`, [ADR 0037](adr/0037-internationalization.md), [i18n](i18n.md)) |
| OpenAPI from typed routes | — | Done (Forge-Go ADR 0043) | Done (`#[derive(OpenApiSchema)]`, `forge::openapi::typed`/`describe`, `GET /openapi.json`, `FORGE_OPENAPI_ENABLED`, `forge openapi`, [ADR 0039](adr/0039-openapi-from-typed-routes.md), [OpenAPI](openapi.md)) |
| List the application's routes | `rails routes` | Done (`forge routes`, a source scan: literal `Handle`/`Resources` paths only, so the `/v1/<plural>` JSON routes of resources are missing) | Done (`forge routes`, runs the composed application: every HTML and JSON route with its source line, [CLI](cli.md#application-lifecycle-commands)) |
| Controller, model, mailer and job generators | `rails g` | Done (Forge-Go ADR 0041) | Done (`forge generate model\|controller\|mailer\|job --queue`, [ADR 0038](adr/0038-generators-and-destroy.md), [generators](generators.md)) |
| Undo a generator | `rails d` | Done (`forge destroy`, Forge-Go ADR 0041) | Done (`forge destroy`, [ADR 0038](adr/0038-generators-and-destroy.md)) |
| Continuous fuzzing, SBOM, signed images | — | Done (`scripts/fuzz.sh`, `scripts/sbom.sh`, `release.yml`, Forge-Go ADR 0044) | Done (`scripts/fuzz.sh`, `scripts/sbom.sh`, signed forge-proxy image, SHA/digest pins enforced by `crates/forge/tests/supply_chain.rs`, [ADR 0043](adr/0043-continuous-fuzzing-sbom-signed-images.md), [supply chain](supply-chain.md)) |
| Published benchmarks | — | Done (`go test -bench`, `scripts/bench.sh`, [BENCHMARKS.md](https://github.com/rocketinsidecorp/forge-go/blob/main/docs/BENCHMARKS.md)) | Done (`cargo bench -p forge-bench`, `scripts/bench.sh`, [benchmarks](benchmarks.md)) |
| Getting Started guide | Rails Guides | Done (Forge-Go docs/GETTING_STARTED.md) | Done ([getting started](getting-started.md)) |

## Parity follow-ups

- HTTP core (forge-rust QR1, 2026-10-25): `HEAD`, the panic `500` and HSTS
  follow Forge-Go's rules. Differences: (1) Forge-Go answers a panic with
  its JSON error body (`{"error":{"code":"internal_error",...}}`), Forge-Rust
  with `text/plain` `internal server error`, like its other framework
  refusals; both carry the request id. (2) `FORGE_HTTP_TRANSPORT`: Forge-Rust
  has no `tls` mode (it never terminates TLS) and refuses it; unset, it is
  `trusted-proxy` in production, where Forge-Go defaults to `plain` and
  refuses to start, so its generated Dockerfile sets `trusted-proxy`.
  Neither forge-proxy adds HSTS; the application's header passes through.
  (3) `FORGE_MAX_IN_FLIGHT_BODY_BYTES` is Rust-only: Forge-Go streams
  request bodies, Forge-Rust buffers them before dispatch. (4) A path guard
  registered before the locale negotiation can no longer be bypassed with a
  locale prefix in Forge-Rust (the prefix is removed in a rewrite step
  before every layer); Forge-Go still relies on registering path-sensitive
  middleware after its i18n middleware, and should strip the prefix before
  the application middleware the same way (tracked in
  [forge-go#56](https://github.com/rocketinsidecorp/forge-go/issues/56)). (5) forge-proxy `-idle-timeout`
  now means what it means in Forge-Go: the wait between requests.

- Database TLS and pool (forge-rust QR2, 2026-10-26): production refuses a
  database URL that does not verify the server certificate, with Forge-Go's
  key `FORGE_DATABASE_ALLOW_PLAINTEXT` as the opt-out, and the serving pool
  takes Forge-Go's `FORGE_DATABASE_MAX_CONNS` (1-1000, 10),
  `FORGE_DATABASE_CONNECT_TIMEOUT` (5s) and `FORGE_DATABASE_STATEMENT_TIMEOUT`
  (30s, at least 1ms; migrations run without it) with the same duration
  syntax. Differences: (1) MySQL/MariaDB TLS is spelled as SQLx spells it,
  `ssl-mode=VERIFY_IDENTITY` (Forge-Go's driver takes `tls=true`, which
  Rust refuses as ignored), and Rust refuses CA-only verification
  (`verify-ca`, `VERIFY_CA`) that Forge-Go accepts: SQLx's rustls backend
  adds the given CA to the public web roots and skips the host name check,
  so any publicly trusted certificate would pass, where pgx trusts only
  `sslrootcert`. (2) Rust
  accepts a Unix socket named in the URL (`host=/dir`, `socket=`) without
  TLS; Forge-Go's URLs need a TCP host. (3) The connect timeout is SQLx's
  acquire timeout (waiting for a free connection and opening one), where
  pgx bounds each dial; it does not set SQLite's `busy_timeout` (fixed at
  5s). (4) `FORGE_DATABASE_MIN_CONNS`, `MAX_CONN_LIFETIME` and
  `MAX_CONN_IDLE_TIME` are not ported yet (SQLx's defaults apply: no
  minimum, 30 min lifetime, 10 min idle); add them when a deployment needs
  them.

- Database URLs without a host (tracked 2026-10-25, forge-rust#24): the Rust
  production guard of `db drop` treats `postgres:///shop` as a local socket
  (what SQLx resolves it to: `PGHOST=/dir`, or the first default socket
  directory with a server, else `localhost`). Forge-Go's `forge db` refuses
  such a URL as ambiguous, because its checks need an explicit host. Both fail
  closed for a remote host; the difference is that Rust accepts the URL. Decide
  once whether Go should resolve the same way or Rust should refuse it, and
  change the other framework to match.

- TOTP MFA ([ADR 0040](adr/0040-totp-multi-factor-authentication.md)): the
  primitives, storage format, key, throttle limits, audit names (including
  `auth.mfa.backup_codes_regenerated`) and the HTML enrollment, rotation,
  backup-code and disable pages behind password and code step-up match
  Forge-Go (its ADR 0045). Both use a step-up backup code up only with the
  change it authorizes. Forge-Rust has no JSON login API, so it has no
  `/auth/mfa/complete` bearer route and does not port `PruneMFA` yet. Its
  sign-in, reset and MFA counters are in PostgreSQL since QR2 (below).

- Shared attempt throttle (forge-rust QR2, 2026-10-26): `PgLoginThrottle`
  ports Forge-Go's `auth/postgres` `LoginThrottler`: the same
  `forge_login_throttle` table (scope 1 account, 2 source, 3 pair; 32-byte
  key digest; count; `reset_at`; primary key `(scope, key_hash)`, no
  surrogate id), the same transaction (create empty, lock in order, refuse
  without counting, count, prune up to 8 expired rows), the database clock,
  and success clearing the account and pair. Differences: (1) the table is a
  generated application migration (`00000000000000000005_forge_login_throttle`)
  like Forge-Rust's other identity tables, where Forge-Go embeds it in its
  framework migrations; (2) the digests are Forge-Rust's `ThrottleKey`
  digests, so the two frameworks' rows are not interchangeable (no database
  is shared between them); (3) Forge-Rust authentication is PostgreSQL-only,
  so no MySQL/MariaDB/SQLite store exists; `MemoryLoginThrottle` remains for
  tests and custom wiring, counting per process.

- Benchmarks ([benchmarks](benchmarks.md), rocketinsidecorp/forge-rust#41): the
  scenario names, variants and output formats are Forge-Go's, and results are
  read side by side. Forge-Rust has no equivalent of two scenarios, so they are
  not run and not reported: `router_match/wildcard/*` (its router has no
  catch-all `{name...}` segment) and the end-to-end `show_json` (a generated
  application has no JSON scaffold and no bearer-token login: `forge generate
  scaffold` is HTML-only, [CLI](cli.md#refusals)). Adding a JSON
  scaffold with bearer-token login to Forge-Rust, or a catch-all to its router,
  makes the scenario available; the load generator already names `show_json`
  and refuses it with that reason. Differences that are not gaps: `health` is
  `GET /health/ready` in Forge-Rust (the probe that pings the database; its
  `/health` never does), `password_verify` times Argon2id where Forge-Go's is
  PBKDF2, `middleware_chain` and `http_telemetry` drive `Router::dispatch`
  with the server's telemetry steps because the server's request handler is
  private, and the rig creates a migration role and a runtime role where
  Forge-Go creates one. The load generator is a Rust port
  (`crates/forge-loadgen`), because Forge-Go's `internal/loadgen` speaks to a
  Forge-Go application only (other form field, session and login routes).

- Framework migration ownership ([ADR 0030](adr/0030-framework-migration-ownership.md),
  rocketinsidecorp/forge-rust#10): Forge-Rust's `rollback` never reverts the
  rate-limit or cache migrations merged into the application's table and
  `migrate` tolerates them once their feature is off. Forge-Go merges its
  framework sets into `forge_schema_migrations`, reverts the most recently
  applied migration of any set and refuses versions this build does not
  contain; it adopts the same rule in rocketinsidecorp/forge-go#13 (in
  progress, next Forge-Go pull request after the observability stack).

- Telemetry (ADR 0033 and 0034, rocketinsidecorp/forge-rust#13): part 1
  ports Forge-Go's telemetry (same `FORGE_TELEMETRY_*` variables, metric
  names and OTLP/Prometheus formats); part 2 ports the generated local
  observability stack with the same files, services, image digests and
  commands. Grafana's default host port is 3001 instead of Forge-Go's 3000
  because Forge-Rust's generated app already publishes 3000, and the overlay
  also sets `FORGE_TELEMETRY_OTLP_ALLOW_PLAINTEXT=true` because the image runs
  in production; the dashboard keeps only panels for metrics Forge-Rust
  exports. Still open in the same issue: metrics Forge-Rust cannot record
  yet: `db.client.operation.duration` and per-query spans (SQLx has no
  query hook), `forge.db.pool.waits`/`forge.db.pool.wait.duration` (the SQLx
  pool exposes no wait counters), `http.client.request.duration` (no
  instrumented client). See
  [observability](observability.md#differences-from-forge-go).

- Application lifecycle ([ADR 0036](adr/0036-application-lifecycle-commands.md)):
  the Rust CLI now has `version`, `doctor`, `dev`, `migrate`, `rollback`,
  `setup`, `seed` and `db create|drop|reset|setup|prepare|migrate|rollback|status|seed`
  with Forge-Go's names, flags and refusals. Still open, in this order, each
  an application subcommand so the CLI keeps holding no credentials:
  `db schema:dump|schema:load` (`db console` exists since P5: it runs the
  client with the URL's credentials in the environment, like Forge-Go), browser reload in `forge dev` (the Go
  proxy needs a reviewed Rust port with the same CSP rules), and
  `.env.development`/`.env.local` files in `forge dev` and the lifecycle
  commands together with generating them. Planned right after the generators
  ([forge-rust#15](https://github.com/rocketinsidecorp/forge-rust/issues/15)).
  `forge inspect` stays "Planned" in the table above. Forge-Go reads the target
  database of `db create|drop` from `FORGE_DATABASE_URL` and the credentials
  from `FORGE_MIGRATION_DATABASE_URL`; Forge-Rust uses
  `FORGE_MIGRATION_DATABASE_URL` for both (see
  [cli](cli.md#differences-from-forge-go)); Forge-Go adopts that, or Rust
  adds the split, when the second framework's `db create` is next touched.

- Credentials and attribute encryption
  ([ADR 0041](adr/0041-encrypted-credentials-and-attribute-encryption.md)):
  file format, keys, key ids, ciphertexts and blind indexes are
  byte-compatible with Forge-Go (Go-written fixtures are tested). Neither
  framework has `forge generate scaffold ... field:string:encrypted` or loads
  credentials in generated bootstrap; both stay on the roadmap of both
  frameworks, to be added to Forge-Go first. Forge-Rust refuses symbolic-link
  key files, which Forge-Go accepts inside the directory
  ([credentials](credentials.md#compatibility-with-forge-go)).

- Stream signing key rotation (ADR 0031, rocketinsidecorp/forge-rust#14):
  both libraries verify previous keys (`Signer::new(key, previous)`,
  `SignerOptions.PreviousKeys`), but generated applications of both read only
  `FORGE_STREAMS_KEY`, so rotating it stops live updates on open pages until
  they are reloaded. Add `FORGE_STREAMS_PREVIOUS_KEYS` (comma-separated,
  validated and redacted like `FORGE_STREAMS_KEY`, at most 3) to both
  frameworks in one change, Forge-Go first since its live updates are still in
  review; until then the rotation procedure is in
  [deployment](deployment.md#rotating-the-stream-signing-key).

- Deployment ([ADR 0042](adr/0042-forge-deploy.md)): commands, flags,
  `config/deploy.yml`, `.forge/secrets`, hooks, remote layout, lock and
  deploy-log records, exit codes and the forge-proxy command line, control
  protocol and state file match Forge-Go, and the dry-run plans equal
  Forge-Go's golden files. Differences, each a follow-up: (1) both frameworks
  run `migrate` with the migrate role's env file, so Forge-Rust's separate
  `FORGE_MIGRATION_DATABASE_URL` reaches that role's long-running containers;
  a per-command environment (for example `migrate.env`) needs the same schema
  change in both, Forge-Go first since its schema is the reference. (2)
  Forge-Rust's forge-proxy obtains certificates ahead of time over HTTP-01
  instead of autocert's on-demand TLS-ALPN-01/HTTP-01 at the first handshake;
  the cache layout is shared. It speaks HTTP/1.1 only (no `h2` crate) and
  opens one application connection per request. (3) The YAML reader refuses
  anchors, aliases, tags, block scalars and multi-line scalars, which
  yaml.v3 accepts. (4) clap reports usage errors with exit code 2 where
  Forge-Go's `flag` uses 1, and single-dash long flags (`-dry-run`) are not
  accepted by `forge` (forge-proxy accepts both, like Go). (5) As in
  Forge-Go, `forge new` does not write `config/deploy.yml`; `forge deploy
  init` does.

- `forge routes`: same command, columns (`METHOD`, `PATH`, `ACTION`, `SOURCE`),
  sorting and footer in both frameworks, but Forge-Go scans the source for
  literal `Handle`, `HandleFunc` and `Resources` paths and Forge-Rust lists the
  routes of the composed application (`Router::routes()` and a built-in `routes`
  command, like `openapi`), because Rust routes are registered by code.
  **Forge-Go follow-up:** its table does not list the `/v1/<plural>` JSON
  routes of generated resources and scaffolds (`generate resource|scaffold`),
  because their patterns are built from a package constant and are not a string
  literal at the call (`"GET " + postsPath`, with `postsPath = "/v1/posts"` in
  `app/adapters/httpapi`); the literal `GET /v1/status` and `--api` controller
  routes are listed. Forge-Go should either resolve package-level string
  constants and concatenations of them in `discoverRoutes`, or run the
  application's own route registration as Forge-Rust does, so that `forge
  routes` lists every route. Forge-Rust lists every HTML and JSON route. Its
  `ACTION` column is always `-` because it has no `Resources` router (Forge-Go
  fills it for `Resources` routes); porting `Resources` to Rust is a separate
  item.
  Forge-Rust's command needs the configuration `serve` needs (a database URL
  for database applications) and builds the application first; Forge-Go's
  never executes application code.

- Supply chain ([ADR 0043](adr/0043-continuous-fuzzing-sbom-signed-images.md),
  [supply chain](supply-chain.md)): the workflows, pins, tag guard,
  verification command and `scripts/fuzz.sh` interface match Forge-Go. Known
  differences, each a tracked follow-up: (1) the release publishes only the
  forge-proxy image for `linux/amd64`: hello-forge's path dependencies leave
  its Docker context (the generated application's own Dockerfile is the
  supported route), and an arm64 build under emulation takes hours, so a
  cross-compiling Dockerfile or a native arm64 runner is needed first; (2)
  binary SBOMs come from `cargo auditable` plus Syft for `linux/amd64`
  only; (3) the generated application's CI and `release.yml` template, with
  the `FORGE_RELEASE_IMAGES` opt-in and the CI SBOM artifact, are not ported
  yet and `forge new` still writes unpinned actions (the template files are
  edited by other pull requests; Forge-Go ported them in the same pull
  request as the rest); (4) Forge-Go's fuzz targets for session tokens, pagination cursors,
  UUID text and the `forge db` target are not ported; candidates in Rust are
  `forge_storage` key parsing and signed links, `forge_broadcast` stream
  tokens, `forge_core::Timestamp::parse` and the MFA key and backup-code
  parsers. The Rust-only targets are `credentials_envelope`, `deploy_yaml`,
  `proxy_*` and `mail_message`; (5) fuzzing found that database URLs accepted
  port 0 where Forge-Go refuses it; fixed in the same change
  ([supply chain](supply-chain.md#findings)).

- Jobs in generated applications: both `forge new`s wire background work
  into the application lifecycle with a `JobsConfigure`/`jobs_configure`
  hook that registers no job, runs at most 4 jobs at once and stops with the
  process, and `forge generate job` prints the line to add there. Forge-Go's
  hook starts its in-process recurring scheduler (`jobs.Scheduler`, on
  PostgreSQL, MySQL and MariaDB) and its persistent queue (`forge generate
  job --queue`) is wired by hand; Forge-Rust's starts the persistent
  PostgreSQL queue (`forge::jobs`, whose cron `Schedule`s replace the
  scheduler), so MySQL, MariaDB and SQLite applications get no job wiring,
  and its limit of 4 is per queue. `forge generate job` without `--queue` (an
  in-process scheduled job) stays Forge-Go only. Forge-Rust's generated
  README documents the `jobs work|status|dead|retry|discard` commands, which
  Forge-Go's lacks.

- Welcome page and README logo: the generated welcome page has Forge-Go's
  layout, sections, colors and text (masthead, logo, call-to-action buttons,
  health endpoints, footer) with the Forge Rust logo. Forge-Go serves it as a
  standalone document with an inline stylesheet (`style-src
  'unsafe-inline'`); Forge-Rust renders it in the application layout, so it
  keeps the strict Content-Security-Policy, the flash messages and Turbo, and
  its styles are the `welcome-page` section of `assets/application.css`. The
  generated README starts with the logo from `assets/` like Forge-Go's; a
  `--api` application has no `assets/`, so its README links the logo in the
  Forge-Rust repository instead.

## Review gate

Every pull request passes the Claude Code Review before it merges to `main`,
including a screen for malicious or exfiltrating code: unexpected network
calls, access to credentials, wallets or crypto keys, obfuscated code, build
scripts and new dependencies.
