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
| API-only application | `rails new --api` | Done (`forge new --api`: JSON handlers, no views, assets, translations or browser middleware; `forge generate resource` and `controller --api` write JSON layers wired `DenyAll`; the page generators refuse it; Forge-Go [QG11](https://github.com/rocketinsidecorp/forge-go/pull/74)) | Done (same command and flag, refusals and wording; `fn compose` in `src/bootstrap/mod.rs`, so `forge generate resource` and `controller --api` wire `DenyAll` routes in it, [generators](generators.md#in-an-api-only-application)) |
| Server-rendered HTML, layouts, partials, forms, flash, CSRF | Action View | Done | Done |
| HTML + JSON CRUD generator | `rails g scaffold` | Done | Done (HTML pages); In review (the JSON API under `/v1/<plural>` behind bearer tokens that `forge generate scaffold` now writes next to its pages, [ADR 0045](adr/0045-json-resources-and-bearer-api-authentication.md)) |
| Authentication pages, password reset | `rails g authentication` | Done | Done |
| First tenant and owner, seeds | `db:seed` | Done (`forge setup`, `forge seed`) | In review (`forge setup`, `forge seed`; the application commands `setup` and `seed` are Done) |
| Multi-tenancy with database enforcement | (gems) | Done (PostgreSQL RLS; `app_migrator`/`app_runtime` roles, staging and production refuse a serving role that could bypass RLS, including one holding `TRUNCATE`/`REFERENCES`/`TRIGGER` on an RLS table or a privilege on an unprotected partition, every pool checked unless `AllowRowSecurityBypass()`, `PGOPTIONS` stripped from the serving pool, tests run as the runtime role; Forge-Go ADR 0046, QG9) | Done (PostgreSQL RLS; `app_migrator`/`app_runtime` roles, [ADR 0011](adr/0011-postgresql-runtime-migration-roles-and-rls-context.md); production refuses a serving role that could bypass RLS, generated repository tests run as a per-test runtime role and prove it cannot run DDL, [ADR 0044](adr/0044-refused-row-level-security-bypass-and-runtime-role-tests.md); the same `TRUNCATE`/`REFERENCES`/`TRIGGER` and partition rules, every PostgreSQL pool checked unless `PoolSettings::allow_row_security_bypass()`, no environment fails closed, lazy pools check each connection, and no pool but the migrator's opens while `PGOPTIONS` is set, [forge-rust#85](https://github.com/rocketinsidecorp/forge-rust/issues/85); see follow-ups) |
| TOTP MFA | (gems) | Done | Done (`FORGE_AUTH_MFA_KEY`, `/session/mfa`, `/mfa`, [ADR 0040](adr/0040-totp-multi-factor-authentication.md), [MFA](mfa.md)) |
| Persistent jobs, cron, outbox | Solid Queue | Done | Done (framework crates); In review (`forge new` wires the job queue into PostgreSQL applications: `jobs` feature, `.jobs(WorkerMode::InServe)`, `jobs_configure`) |
| Batch claims, `LISTEN/NOTIFY` wake-up, one replica per scheduled tick | Solid Queue (`batch_size`, `polling_interval`, recurring tasks run once) | Done (`WorkerOptions.BatchSize` 10, `queuepostgres.WithNotify`, `DispatcherOptions.BatchSize` 10, `jobs.SchedulerOptions.Ticks` with `forge_job_schedules`; Forge-Go ADR 0047) | Done (ticks recorded once in `forge_job_schedules`); In review (`WorkerSettings::batch` 10, never above the free slots; `App::jobs_notify(true)` or `FORGE_JOBS_NOTIFY=true` (`PgJobStore::notify` in the library) announces due jobs on `forge_jobs` and `Jobs::run` listens; `DispatcherSettings::batch` 10; [ADR 0046](adr/0046-job-wake-up-and-claim-batches.md), [forge-rust#80](https://github.com/rocketinsidecorp/forge-rust/issues/80); see follow-ups) |
| Mailer with previews | Action Mailer | Done | Done (`forge::mail`, password-reset emails, `/_mail` previews, [ADR 0035](adr/0035-mail.md); `forge generate mailer`, [ADR 0038](adr/0038-generators-and-destroy.md)) |
| Rate limiting | `rate_limit` | Done | Done |
| Sign-in, reset and MFA throttles shared by replicas | `rate_limit` with a shared cache store | Done (`auth/postgres` `LoginThrottler`, `forge_login_throttle`) | In review (`forge::auth::PgLoginThrottle`, same table and transaction, wired in generated PostgreSQL applications; see follow-ups) |
| Session idle timeout | (gems, e.g. Devise `timeoutable`) | Done ([forge-go#62](https://github.com/rocketinsidecorp/forge-go/pull/62): `FORGE_AUTH_SESSION_IDLE_TIMEOUT`, default 2h; `last_seen_at` written at most once a minute) | Done (`FORGE_AUTH_SESSION_IDLE_TIMEOUT`, default 2h, `5m` to `720h`; `forge_sessions.last_seen_at` written at most once a minute, migration `00000000000000000006_forge_session_idle_timeout`, [forge-rust#75](https://github.com/rocketinsidecorp/forge-rust/issues/75); see follow-ups) |
| CSRF token masked per render (BREACH) | `masked_authenticity_token` | Done ([forge-go#62](https://github.com/rocketinsidecorp/forge-go/pull/62): `csrf.Token`, `csrf.MaskToken`) | Done (`csrf_meta`, `csrf_field`, `forge::views::mask_csrf_token`/`unmask_csrf_token`; same encoding) |
| Production warning for a per-process attempt throttle | — | Done ([forge-go#62](https://github.com/rocketinsidecorp/forge-go/pull/62): `auth.MemoryLoginThrottler` in `NewLoginService`, `NewMFAService`, `NewRecoveryService`) | Done (`MemoryLoginThrottle` passed to the generated sign-in, reset or two-factor wiring, `Composition::warn_if_process_local_throttle`) |
| Stream signing key rotation without breaking open pages | `MessageVerifier#rotate` | Done ([forge-go#64](https://github.com/rocketinsidecorp/forge-go/pull/64): `FORGE_STREAMS_PREVIOUS_KEYS`, at most 3, redacted, `broadcast.DecodePreviousKeys`) | Done (`FORGE_STREAMS_PREVIOUS_KEYS`, same name, separator, limit and redaction; `Signer::from_config` reads it for every generated application, [forge-rust#77](https://github.com/rocketinsidecorp/forge-rust/issues/77), [views](views.md#rotating-the-stream-signing-key)) |
| forge-proxy bounds on request headers and connections | nginx `large_client_header_buffers`, `worker_connections` | Done ([forge-go#64](https://github.com/rocketinsidecorp/forge-go/pull/64): `-max-header-bytes` 65536, `431` above; `-max-connections` 10000 per listener) | Done (`-max-header-bytes` and `-max-connections`, same names, defaults and range, [forge-rust#77](https://github.com/rocketinsidecorp/forge-rust/issues/77), [deployment](deployment.md#forge-deploy)) |
| Inline stylesheets allowed by hash, not `'unsafe-inline'` | `content_security_policy` | Done ([forge-go#64](https://github.com/rocketinsidecorp/forge-go/pull/64): the welcome page, with a test that recomputes the hash) | Done (mail preview pages, with a test that recomputes the hash; the generated welcome page has no inline style; two cases stay, see follow-ups) |
| Re-vendor after `forge generate` | — | Done ([forge-go#64](https://github.com/rocketinsidecorp/forge-go/pull/64): `go mod vendor` in an application with `vendor/`, never on `--dry-run`) | Not applicable (`forge new` sets up no vendoring and no generator changes `Cargo.toml` or `Cargo.lock`; a test runs every generator against a vendored application, [cli](cli.md#forge-generate-modelresourcecontrollermailerjob-and-forge-destroy)) |
| Bounded memory-hard password hashing | — | Not needed (PBKDF2, no per-hash memory) | In review (`forge::auth::PasswordWorkLimiter`: one Argon2id hash per CPU, at most 16, then `503`) |
| Client address behind trusted proxies | `request.remote_ip`, `trusted_proxies` | Done (`FORGE_HTTP_TRUSTED_PROXIES`, ADR 0037) | Done (`FORGE_HTTP_TRUSTED_PROXIES`, ADR 0032; `forge deploy` sets it for the proxied role, [ADR 0042](adr/0042-forge-deploy.md)) |
| `HEAD` on every `GET` route | `HEAD` routed to `GET` | Done (`http.ServeMux`) | In review (the router serves `HEAD` with the `GET` route unless a `HEAD` route exists, the server drops the body, `Allow` lists `HEAD`) |
| One name per setting and command (naming decision D2) | — | Done (`FORGE_HTTP_*`, `FORGE_DATABASE_*`, `FORGE_CACHE_DEFAULT_TTL`, `FORGE_STORAGE_LINK_TTL`; `forge uuid [--count N]`) | In review (the same names and duration formats; `FORGE_BIND`, `FORGE_MAX_BODY_BYTES`, `FORGE_SHUTDOWN_GRACE_SECS`, `FORGE_CACHE_DEFAULT_TTL_SECS`, `FORGE_STORAGE_LINK_TTL_SECS` and `forge id` are deprecated aliases until the next minor version: a one-line warning, and different values under both spellings stop start-up; [deployment](deployment.md#renamed-variables), [cli](cli.md#forge-uuid)) |
| Development environment files | `dotenv` gems, `.env.development` | Done (`.env.development` then `.env.local`, process environment wins, `forge dev\|test\|db\|migrate\|rollback\|doctor`) | In review (same files, format, precedence and line-only errors; `forge dev`, `test`, `doctor` and every lifecycle command; `forge new` writes `.env.development`; differences: no `FORGE_TEST_DATABASE_URL` or generated `.env.local`, [cli](cli.md#development-environment-files)) |
| Catch-all route segment | `get "files/*path"` | Done (`GET /files/{key...}`, `http.ServeMux`) | In review (`{name...}` as the last segment: the rest of the path, possibly empty; static and `{name}` routes win; like Forge-Go's router, every path holding `%2F`, `%5C`, a raw backslash or a `.`/`..` segment (spelled or `%2e`-encoded) answers `400` before routing, and so does a catch-all value starting with `/` or holding `//` (which `ServeMux` cleans); the OpenAPI document names it `{name}` like Forge-Go's; a path that stops before the catch-all, `/files`, is `404` where `ServeMux` redirects to `/files/`) |
| Handler panic answered with `500` | `ShowExceptions` | Done (`recoverer`: JSON error body with the request id) | In review (text body `internal server error`, `x-request-id`, logged with the request id) |
| HSTS behind TLS | `config.force_ssl` | Done (`Strict-Transport-Security` unless `FORGE_HTTP_TRANSPORT=plain`) | In review (same header and rule, `FORGE_HTTP_TRANSPORT=plain\|trusted-proxy`, see follow-ups) |
| Verified database TLS in production, pool limits | `sslmode` in `database.yml`, `pool` | Done (`sslmode=verify-full\|verify-ca`, MySQL `tls=true`, `FORGE_DATABASE_ALLOW_PLAINTEXT`, `FORGE_DATABASE_MAX_CONNS\|MIN_CONNS\|MAX_CONN_LIFETIME\|MAX_CONN_IDLE_TIME\|CONNECT_TIMEOUT\|STATEMENT_TIMEOUT`) | Done (stricter TLS rule: `sslmode=verify-full`, SQLx's MySQL spelling `ssl-mode=VERIFY_IDENTITY`; `verify-ca` refused, see follow-ups; `FORGE_DATABASE_ALLOW_PLAINTEXT`, `FORGE_DATABASE_MAX_CONNS\|MIN_CONNS\|MAX_CONN_LIFETIME\|MAX_CONN_IDLE_TIME\|CONNECT_TIMEOUT\|STATEMENT_TIMEOUT`, the pool keys since [forge-rust#85](https://github.com/rocketinsidecorp/forge-rust/issues/85); see follow-ups) |
| Bound on buffered request bodies | — | Not needed (bodies are streamed through `MaxBytesReader`) | In review (`FORGE_MAX_IN_FLIGHT_BODY_BYTES`, 256 MiB for all bodies at once, `503` with `Retry-After`) |
| Cache | Solid Cache | Done | Done |
| File storage with signed URLs | Active Storage | Done (disk) | Done (disk) |
| Import maps, Turbo, Stimulus | Hotwire | In review | Done |
| Turbo Streams over SSE | Action Cable | In review | Done (`scaffold --broadcast`, ADR 0031) |
| Traces, metrics, log correlation | (gems) | Done (OpenTelemetry, Prometheus) | Done (OpenTelemetry, Prometheus; ADR 0033) |
| Preconfigured Prometheus, Grafana, Tempo, Loki | — | Done (`compose.observability.yaml`, Forge-Go ADR 0038) | Done (`compose.observability.yaml`, [ADR 0034](adr/0034-local-observability-stack.md)) |
| Zero-downtime deploys, rollback, remote logs | Kamal | Done (`forge deploy`, `forge logs`, `forge-proxy`) | In review (`forge deploy`, `forge logs`, `forge app`, `forge-proxy` with ACME TLS, same `config/deploy.yml`, [ADR 0042](adr/0042-forge-deploy.md), [deployment](deployment.md#forge-deploy)) |
| Migration credential reaches the migrate step alone | — | Done (`migrate.env` in `config/deploy.yml`, [forge-go#57](https://github.com/rocketinsidecorp/forge-go/issues/57)) | In review (`migrate.env`, same schema, messages and dry-run plan, [forge-rust#73](https://github.com/rocketinsidecorp/forge-rust/issues/73), [deployment](deployment.md#the-migration-credential)) |
| Live reload and database lifecycle | `bin/dev`, `db:*` | In review (`forge dev`, `forge db`) | In review (`forge dev`, `forge db create\|drop\|reset\|setup\|prepare\|migrate\|rollback\|status\|seed`, `forge migrate\|rollback\|setup\|seed`, `forge doctor`, `forge version`, [ADR 0036](adr/0036-application-lifecycle-commands.md)) |
| Encrypted credentials, attribute encryption | `credentials:edit`, AR Encryption | Done (`forge credentials`, `encryption`) | Done (`forge credentials init\|edit\|show\|rotate-key`, `forge::credentials`, `forge::encryption`, [ADR 0041](adr/0041-encrypted-credentials-and-attribute-encryption.md), [credentials](credentials.md), [encryption](encryption.md)) |
| i18n | I18n | Done (`config/locales/*.json`, `t`, CLDR plurals, `forge new --locale`, `forge i18n check`, Forge-Go ADR 0042) | Done (`config/locales/*.json`, `layout.i18n.t`, CLDR plurals, `forge new --locale`, `forge i18n check`, [ADR 0037](adr/0037-internationalization.md), [i18n](i18n.md)) |
| OpenAPI from typed routes | — | Done (Forge-Go ADR 0043) | Done (`#[derive(OpenApiSchema)]`, `forge::openapi::typed`/`describe`, `GET /openapi.json`, `FORGE_OPENAPI_ENABLED`, `forge openapi`, [ADR 0039](adr/0039-openapi-from-typed-routes.md), [OpenAPI](openapi.md)) |
| List the application's routes | `rails routes` | Done (`forge routes [NAME]`, runs the composed application since [forge-go#66](https://github.com/rocketinsidecorp/forge-go/pull/66): every HTML and JSON route, the `/v1/<plural>` routes of resources included, with its resource action and source line; a source scan only when the application cannot be built or configured) | Done (`forge routes [NAME]`, runs the composed application: every HTML and JSON route with its source line, `NAME` the binary (`--bin NAME`) like `forge dev [NAME]`, [CLI](cli.md#application-lifecycle-commands), [forge-rust#85](https://github.com/rocketinsidecorp/forge-rust/issues/85)) |
| Controller, model, mailer and job generators | `rails g` | Done (Forge-Go ADR 0041) | Done (`forge generate model\|controller\|mailer\|job --queue`, [ADR 0038](adr/0038-generators-and-destroy.md), [generators](generators.md)) |
| One page generator | — | Done (`forge generate view [DIR/]NAME`, `forge destroy view`) | In review (same command and arguments: `templates/DIR/NAME.html`, plus the Askama view model and a rendering test; refuses `--api` applications instead of adding a views base, [generators](generators.md#view)) |
| Model plus JSON API generator | `rails g resource` | Done (`forge generate resource`, `forge destroy resource`: model + `/v1/<plural>` JSON routes behind bearer tokens) | In review (`forge generate resource`, `forge destroy resource`: the model + `/v1/<plural>` JSON CRUD behind bearer tokens, `<plural>:read`/`<plural>:write`, OpenAPI `bearerAuth`, generated `401`/`403`/cross-tenant/validation tests, [generators](generators.md#resource); see follow-ups) |
| JSON sign-in for API clients (bearer tokens) | (gems) | Done (`forge new --api-auth`, `forge generate authentication --api-auth`: `/auth/login`, `/auth/logout`, `/auth/mfa/complete`, `/auth/recovery/*`, `/auth/cookie/*`, `GET /v1/auth/session`) | In review (`forge new --api-auth`, `forge generate authentication --api-auth`: `POST /auth/login`, `POST /auth/logout`, `GET /v1/auth/session`; tenant-bound tokens stored as SHA-256 digests in `forge_bearer_sessions`; the MFA, recovery and cookie JSON routes are follow-ups, [forge-rust#82](https://github.com/rocketinsidecorp/forge-rust/issues/82), before v0.1.0) |
| Undo a generator | `rails d` | Done (`forge destroy`, Forge-Go ADR 0041) | Done (`forge destroy`, [ADR 0038](adr/0038-generators-and-destroy.md)) |
| Continuous fuzzing, SBOM, signed images | — | Done (`scripts/fuzz.sh`, `scripts/sbom.sh`, `release.yml`, Forge-Go ADR 0044) | Done (`scripts/fuzz.sh`, `scripts/sbom.sh`, signed forge-proxy image, SHA/digest pins enforced by `crates/forge/tests/supply_chain.rs`, [ADR 0043](adr/0043-continuous-fuzzing-sbom-signed-images.md), [supply chain](supply-chain.md)) |
| Published benchmarks | — | Done (`go test -bench`, `scripts/bench.sh`, [BENCHMARKS.md](https://github.com/rocketinsidecorp/forge-go/blob/main/docs/BENCHMARKS.md)) | Done (`cargo bench -p forge-bench`, `scripts/bench.sh`, [benchmarks](benchmarks.md)) |
| Getting Started guide | Rails Guides | Done (Forge-Go docs/GETTING_STARTED.md) | Done ([getting started](getting-started.md)) |
| HTTP timeouts and request head bound | Puma `first_data_timeout`, `persistent_timeout` | Done (`FORGE_HTTP_READ_HEADER_TIMEOUT`, `FORGE_HTTP_READ_TIMEOUT`, `FORGE_HTTP_WRITE_TIMEOUT`, `FORGE_HTTP_IDLE_TIMEOUT`, `FORGE_HTTP_MAX_HEADER_BYTES`) | Done (same names, defaults and duration syntax; `FORGE_REQUEST_TIMEOUT_SECS` is the deprecated spelling of `FORGE_HTTP_WRITE_TIMEOUT`; the servers differ where Go's net/http and hyper do, see [deployment](deployment.md#renamed-variables); [forge-rust#85](https://github.com/rocketinsidecorp/forge-rust/issues/85)) |
| Format check for CI | `rubocop` | Done (`forge format --check`: lists files, writes nothing, exits 1) | Done (`forge format --check`: `cargo fmt --all -- --check --files-with-diff`, [forge-rust#85](https://github.com/rocketinsidecorp/forge-rust/issues/85)) |
| Metrics listener on a kernel-chosen port | — | Done (`FORGE_TELEMETRY_METRICS_ADDR=127.0.0.1:0`, bound address logged) | Done (same; [forge-rust#85](https://github.com/rocketinsidecorp/forge-rust/issues/85)) |
| Stale scheduler ticks skipped, not replayed | Solid Queue recurring tasks | Done (`SchedulerOptions.MaxLateness`, default one interval, skipped ticks reported) | Done (`Schedule::max_lateness`, default one interval: the gap to the next tick; skipped ticks logged as a warning; decision D14, [forge-rust#85](https://github.com/rocketinsidecorp/forge-rust/issues/85)) |
| Undecodable cached values counted and logged | — | Done (`cache.DecodeFailures()`, warn once per key with an HMAC-SHA256 key prefix) | Done (`forge_cache::decode_failures()`, the same log, [forge-rust#85](https://github.com/rocketinsidecorp/forge-rust/issues/85)) |
| Generated Dockerfile pinned to the framework's own images | — | Done (`TestDockerfilesPinBaseImagesByDigest`) | Done (the supply-chain gate requires forge-proxy's exact references, [forge-rust#85](https://github.com/rocketinsidecorp/forge-rust/issues/85)) |

## Parity follow-ups

- Job wake-up and batches ([forge-rust#80](https://github.com/rocketinsidecorp/forge-rust/issues/80),
  [ADR 0046](adr/0046-job-wake-up-and-claim-batches.md)): both frameworks
  announce due jobs with `pg_notify('forge_jobs', queue)` at enqueue, ignore
  payloads that are not queue names, hold one `LISTEN` connection per worker
  outside the pool, reconnect after a jittered 0.5 s to 1 s doubling to 15 s
  to 30 s while polling continues, claim each queue once per
  (re)subscription, merge wake-ups in a one-slot signal per queue with 10 ms
  between woken claims, bound claims by a batch of 10 (at most 100, never
  above the free slots) and claim 10 outbox events per transaction.
  Differences: (1) both switches are typed (Go's store option `WithNotify`,
  Rust's `PgJobStore::notify(true)`), and Rust's facade adds
  `App::jobs_notify(true)` and `FORGE_JOBS_NOTIFY`, which also set the
  session setting `forge.jobs_notify` on the application's pool, because
  Rust's free enqueue functions take only a transaction; Rust announces with
  a trigger that reads that setting, so schedule ticks are announced too.
  Forge-Go adopts `FORGE_JOBS_NOTIFY` with the same meaning (tracked in
  [forge-go#72](https://github.com/rocketinsidecorp/forge-go/issues/72), next
  Go jobs package, before v0.1.0).
  (2) Go exposes `Worker.Listening()`; Rust logs the subscription instead.
  (3) Go's idle poll doubles to `MaxPollInterval` (10 s); Rust polls every
  `poll_interval` (1 s).
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
  `MAX_CONN_IDLE_TIME` have Forge-Go's names, defaults (0, 30m, 5m) and
  bounds since [forge-rust#85](https://github.com/rocketinsidecorp/forge-rust/issues/85),
  as SQLx's `min_connections`, `max_lifetime` and `idle_timeout`; the
  `routes`/`openapi` pool keeps no connection open, as Forge-Go's lazy pool
  does.

- RLS and environment hardening ([forge-rust#85](https://github.com/rocketinsidecorp/forge-rust/issues/85),
  2026-10-26, port of Forge-Go QG9): the role query has Forge-Go's two new
  predicates and reason texts (`TRUNCATE`, `REFERENCES` on the table or a
  column, or `TRIGGER` on a table with row-level security, by any
  membership, `PUBLIC` included; any privilege on a partition without
  row-level security of such a table). Every PostgreSQL pool is checked
  unless its `PoolSettings` name development or test (no environment fails
  closed, as Forge-Go's empty `Database.Environment`), with
  `PoolSettings::allow_row_security_bypass()` as the only opt-out
  (`AllowRowSecurityBypass()`); an eager pool checks once, a lazy pool
  every connection (`postgres.Lazy`). Differences: (1) SQLx cannot drop
  `PGOPTIONS` once read (and prepends it to the URL's `options`), so where
  Forge-Go strips it from the serving pool Forge-Rust refuses to open any
  pool but the migrator's while it is set; `PGAPPNAME` is replaced by the
  URL's `application_name`, else `forge`, as in Forge-Go; SQLx never reads
  `PGTZ`. (2) Forge-Go's `seed` and `setup` open the runtime pool and are
  checked; Forge-Rust's application commands (every `App::command`, `setup`
  and `seed` included) connect with
  `FORGE_MIGRATION_DATABASE_URL` (ADR 0026: `setup` creates principals the
  runtime role may only read), whose role owns the schema, so they opt out
  like the migrator. (3) A refused lazy connection surfaces as a pool
  timeout in Forge-Rust (SQLx retries `after_connect` errors), with the
  reason logged; `require_row_security` reports it directly.

- RLS defense in depth (forge-rust#70, 2026-10-26, port of Forge-Go QG1
  and its ADR 0046): `PgDatabase::require_row_security` is Forge-Go's
  `postgres.RequireRowSecurity`, with the same rules (login and acting role;
  superuser, `BYPASSRLS`, `CREATEROLE`, membership in a superuser or
  `BYPASSRLS` role; a role switch through the URL or `ALTER ROLE ... SET
  role`), the same pinned `search_path`, qualified catalog names and
  fail-closed result, the same environments ("every environment except
  development and test") and no override variable, plus three rules
  Forge-Go does not have yet, found by the independent review of
  forge-rust#70: Forge-Rust also refuses a serving role that owns, or can
  `SET ROLE` to the owner of, a table with row-level security (the owner can
  `ALTER TABLE ... NO FORCE ROW LEVEL SECURITY`); `REPLICATION`, or
  membership in a `REPLICATION` role (logical decoding reads every tenant's
  changes); and membership in `pg_execute_server_program`,
  `pg_write_server_files` or `pg_read_server_files`. **Forge-Go follow-up:**
  port these three rules to `postgres.RequireRowSecurity` (being ported by
  the maintainers, 2026-10-26). Forge-Rust also runs the check before any
  composition hook or telemetry, as Forge-Go does inside `Open`;
  `forge::db::testing::RlsTestDatabase` is `postgrestest.NewRLS`, and every
  PostgreSQL scaffold gets Forge-Go's DDL-refusal test. Differences: (1)
  Forge-Rust's `FORGE_ENV` has no `staging`, so the check runs in
  `production`; (2) Forge-Go's generated bootstrap passes the option to
  `postgres.Open`, Forge-Rust's `forge::App` lifecycle runs it for `serve`,
  `jobs work` and the `jobs` operator commands, so there is no line to
  remove from a generated bootstrap; (3) the generated sign-in tests
  (`tests/authentication_postgres.rs`) still run as the test server's user,
  where Forge-Go's run as `NewRLS`'s runtime role: some of them read
  `pg_stat_activity` and lock rows across sessions as the administrator, so
  moving them is a follow-up; (4) migrations that grant to `app_runtime` by
  name (the generated identity tables) do not reach the per-test runtime
  role, which keeps the broader default privileges there; (5) the
  development `init.sql` grants sequences `USAGE, SELECT, UPDATE`, where
  Forge-Go grants `USAGE, SELECT`; (6) Forge-Go's SQLSTATE 42883 `uuidv7()`
  explainer is not needed: Forge-Rust generates no SQL UUID default on any
  backend (identifiers are UUIDv7 from Rust). `docker/postgres/init.sql` is
  now `0644` under `0755` directories whatever the umask, as Forge-Go's.

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
  change it authorizes. Forge-Rust's JSON login (`--api-auth`, ADR 0045)
  refuses a two-factor account (`403 mfa_required`) instead of completing it
  at `/auth/mfa/complete`, which is not ported, nor is `PruneMFA` yet. Its
  sign-in, reset and MFA counters are in PostgreSQL since QR2 (below).

- JSON resources and bearer tokens (forge-rust QR3b, 2026-10-26,
  [ADR 0045](adr/0045-json-resources-and-bearer-api-authentication.md)):
  `forge generate resource`, the scaffold's JSON API and `forge new
  --api-auth` follow Forge-Go's commands, routes, error codes
  (`credentials_required`, `credentials_invalid`, `permission_denied`,
  `login_throttled`), `bearerAuth` scheme and `PATCH` semantics. Differences:
  (1) Forge-Rust has no permission table, so no migration registers
  `PLURAL:read`/`PLURAL:write` (the use cases declare them and the printed
  `RbacPolicy` grants them, ADR 0038); (2) tokens are 64 hex digits in their
  own `forge_bearer_sessions` table, not 43 base64url characters in the
  sessions table, and they end when the password changes, the account is
  disabled or turns two-factor on; (3) a two-factor account gets
  `403 mfa_required` (no `/auth/mfa/complete`), and the JSON recovery and
  cookie routes are not ported; (4) `GET /v1/auth/session` lists `roles`, not
  permissions; (5) global JSON resources are `DenyAll` for reads too, where
  Forge-Go lets tenant roles read them; (6) responses have no
  `created_at`/`updated_at`, pages are in UUIDv7 order without `limit`, and
  undecodable bodies are `422` (Forge-Go `400`); (7) `--api` applications
  get resources wired `DenyAll`, as in Forge-Go (they have no identity, so
  no bearer tokens; see the `rails new --api` row). **Forge-Go
  follow-up:** its `docs/AUTHENTICATION.md` and `RAILS_PARITY.md` still say
  Forge-Rust has no JSON login.

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

- Security defaults (forge-rust#75, port of Forge-Go QG4a, audit findings
  S-3, S-7 and PF-3; Forge-Go's side is
  [forge-go#62](https://github.com/rocketinsidecorp/forge-go/pull/62)):
  (1) the session idle timeout: the same variable, default, bounds (`5m` to
  `720h`, no way to turn it off) and error, the same conditional `UPDATE`
  (at most once a minute per session, re-checking that the session is live
  and not idle, decided by the database clock), sessions open at the
  upgrade counted as used then. Differences: the column comes from a
  generated application migration (`00000000000000000006_forge_session_idle_timeout`)
  in the generated `PostgresSessionStore`, like Forge-Rust's other identity
  tables, where Forge-Go has a framework migration and repository option;
  the store re-checks `expires_at` too and still returns a revoked session
  to `Session::authenticate`, which refuses it, so an idle and a revoked
  session both end as `RequestAuthError::InvalidSession`. (2) Masked CSRF
  tokens: the same layout (32-byte pad, then token XOR pad, unpadded
  base64url, 86 characters); Forge-Rust's unmasked token is its canonical
  64 lowercase hex digits where Forge-Go's is 43 base64url characters, so
  each framework accepts its own unmasked form; the masked forms of the same
  32 bytes are identical. `csrf_token` still returns the raw token (the
  masking happens in `csrf_meta`, `csrf_field` and `mask_csrf_token`),
  where Forge-Go's `csrf.Token` returns it masked. The unmasked token stays
  accepted in the header for good and in the form field until v0.2.0.
  (3) The production warning has Forge-Go's wording with Forge-Rust's type
  names; Forge-Rust has no framework sign-in or recovery service, so the
  generated `authentication::routes` asks `Composition` to check the three
  throttles it receives. The JSON login of `--api-auth` (ADR 0045) counts
  in the pages' `PgLoginThrottle`, which that check already covers.

- Benchmarks ([benchmarks](benchmarks.md), rocketinsidecorp/forge-rust#41): the
  scenario names, variants and output formats are Forge-Go's, and results are
  read side by side. One scenario is not run yet and not reported: the
  end-to-end `show_json`. Generated applications now have the JSON scaffold
  and bearer-token login it needs (`forge new --api-auth`, ADR 0045), but
  the load generator still names `show_json` and refuses it until it signs
  in for a token and drives `GET /v1/<plural>/{id}` (follow-up). `router_match/wildcard/*` runs since the router has
  Forge-Go's catch-all `{name...}` segment. Differences that are not gaps: `health` is
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
  proxy needs a reviewed Rust port with the same CSP rules). The
  `.env.development`/`.env.local` files are read by `forge dev`, `test`,
  `doctor` and the lifecycle commands with Forge-Go's names, format,
  precedence and redaction, and `forge new` generates `.env.development`
  ([cli](cli.md#development-environment-files)).
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

- Stream signing key rotation (ADR 0031, rocketinsidecorp/forge-rust#14 and
  #77): both frameworks read `FORGE_STREAMS_PREVIOUS_KEYS` (comma-separated
  base64 keys in the format of `FORGE_STREAMS_KEY`, at most 3, redacted,
  errors name an entry's position and never a key; previous keys without a
  current key are refused). Previous keys only verify, the current key signs.
  Differences: (1) Forge-Go reads the variable in the generated
  `htmlstreams.NewSigner`, which an application generated earlier must update
  by hand (`streams.go` is written once); Forge-Rust reads it in
  `Signer::from_config` and `forge_config::StreamsConfig`, so applications
  generated earlier need no code change. (2) Forge-Rust validates the list
  when the configuration is loaded (a bad entry stops every command, as a bad
  `FORGE_STREAMS_KEY` does) and refuses previous keys without a current key
  in `Signer::from_config`; Forge-Go does both in `NewSigner`, so only the
  application's `serve` sees them. Procedure:
  [deployment](deployment.md#rotating-the-stream-signing-key).

- forge-proxy limits (forge-go QG4b, audit S-8, forge-rust#77): both proxies
  have `-max-header-bytes` (default 65536, 4096 to 1048576, `431` above it)
  and `-max-connections` (default 10000 per listener, at least 1); neither is
  in `config/deploy.yml`. Differences: Forge-Rust answers `431` through hyper
  (its response has no body; Go's has a one-line text body) and closes the
  connection in both; the head limit is the exact byte count of the request
  line and headers, where `net/http` adds 4 KiB of read-buffer slack.

- Inline styles (forge-go QG4b S-10, forge-rust#77): Forge-Go's welcome page
  allows its one stylesheet by SHA-256 hash. Forge-Rust's generated welcome
  page has no inline style (its policy is `style-src 'self'`, asserted by a
  generated test), so the hash applies to the mail preview pages, which
  compute it from the page served. Two uses cannot be hashed and keep
  `style-src 'unsafe-inline'`: the HTML part of a previewed message (the
  mailer's own `style` attributes and `<style>` blocks, shown in a sandboxed
  frame that runs no script and loads nothing remote) and SVG assets
  (`default-src 'none'; style-src 'unsafe-inline'; sandbox`, whose styles can
  load nothing). Forge-Go has no mail preview page.

- Re-vendoring after `forge generate` (forge-go QG4b D-9): Forge-Go runs `go
  mod vendor` in an application with `vendor/`. Not applicable to Forge-Rust:
  `forge new` sets up no vendoring and generators never change the
  dependency set ([cli](cli.md#forge-generate-modelresourcecontrollermailerjob-and-forge-destroy)).

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
  sorting and footer in both frameworks, and since
  [forge-go#66](https://github.com/rocketinsidecorp/forge-go/pull/66) both
  list the routes of the composed application (Forge-Rust:
  `Router::routes()` and a built-in `routes` command, like `openapi`), so
  both list the `/v1/<plural>` JSON routes of generated resources and
  scaffolds. Both need the configuration `serve` needs (a database URL for
  database applications, from the environment or `.env.development`) and build
  the application first. Differences: when the application cannot be built or
  configured, Forge-Go says why and falls back to a scan of the source
  (literal paths only), where Forge-Rust fails with the reason; Forge-Rust's
  `ACTION` column is always `-` because it has no `Resources` router (Forge-Go
  fills it for `Resources` routes; porting `Resources` to Rust is a separate
  item). `forge routes [NAME]` takes the binary to run when an application
  has several, with Forge-Go's meaning (Forge-Go's `cmd/NAME`, Forge-Rust's
  Cargo `--bin NAME`, like its `forge dev [NAME]` and `forge build [NAME]`;
  [forge-rust#85](https://github.com/rocketinsidecorp/forge-rust/issues/85)).

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
  standalone document with an inline stylesheet (allowed by SHA-256 hash since
  forge-go#64, [follow-ups](#parity-follow-ups)); Forge-Rust renders it in the application layout, so it
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
