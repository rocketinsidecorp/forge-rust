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
| TOTP MFA | (gems) | Done | Planned |
| Persistent jobs, cron, outbox | Solid Queue | Done | Done (framework crates; `forge new` wiring planned) |
| Mailer with previews | Action Mailer | Done | Done (`forge::mail`, password-reset emails, `/_mail` previews, [ADR 0035](adr/0035-mail.md); `forge generate mailer` with the generators in [forge-rust#15](https://github.com/rocketinsidecorp/forge-rust/issues/15)) |
| Rate limiting | `rate_limit` | Done | Done |
| Client address behind trusted proxies | `request.remote_ip`, `trusted_proxies` | Done (`FORGE_HTTP_TRUSTED_PROXIES`, ADR 0037) | Done (`FORGE_HTTP_TRUSTED_PROXIES`, ADR 0032; `forge deploy` sets it once ported) |
| Cache | Solid Cache | Done | Done |
| File storage with signed URLs | Active Storage | Done (disk) | Done (disk) |
| Import maps, Turbo, Stimulus | Hotwire | In review | Done |
| Turbo Streams over SSE | Action Cable | In review | Done (`scaffold --broadcast`, ADR 0031) |
| Traces, metrics, log correlation | (gems) | Done (OpenTelemetry, Prometheus) | Done (OpenTelemetry, Prometheus; ADR 0033) |
| Preconfigured Prometheus, Grafana, Tempo, Loki | — | Done (`compose.observability.yaml`, Forge-Go ADR 0038) | Done (`compose.observability.yaml`, [ADR 0034](adr/0034-local-observability-stack.md)) |
| Zero-downtime deploys, rollback, remote logs | Kamal | In review (`forge deploy`) | Planned |
| Live reload and database lifecycle | `bin/dev`, `db:*` | In review (`forge dev`, `forge db`) | In review (`forge dev`, `forge db create\|drop\|reset\|setup\|prepare\|migrate\|rollback\|status\|seed`, `forge migrate\|rollback\|setup\|seed`, `forge doctor`, `forge version`, [ADR 0036](adr/0036-application-lifecycle-commands.md)) |
| Encrypted credentials, attribute encryption | `credentials:edit`, AR Encryption | In review | Planned |
| i18n | I18n | Planned | Planned |
| OpenAPI from typed routes | — | Planned | Planned |
| Controller, model and mailer generators | `rails g` | Planned | Planned |
| Continuous fuzzing, SBOM, signed images | — | Planned | Planned |
| Published benchmarks | — | Planned | Planned |
| Getting Started guide | Rails Guides | Planned | Planned |

## Parity follow-ups

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
  `forge routes` (a `Router::routes()` accessor in `forge-http` and a built-in
  `routes` command that composes the application without serving it; Forge-Go
  scans the source, which cannot see Rust's code-built routes), `db
  schema:dump|schema:load|console`, browser reload in `forge dev` (the Go
  proxy needs a reviewed Rust port with the same CSP rules), and
  `.env.development`/`.env.local` files in `forge dev` and the lifecycle
  commands together with generating them. Planned right after the generators
  ([forge-rust#15](https://github.com/rocketinsidecorp/forge-rust/issues/15)).
  `forge inspect`, `destroy`, `deploy`, `logs`, `app`, `credentials` and
  `i18n` stay "Planned" in the table above. Forge-Go reads the target
  database of `db create|drop` from `FORGE_DATABASE_URL` and the credentials
  from `FORGE_MIGRATION_DATABASE_URL`; Forge-Rust uses
  `FORGE_MIGRATION_DATABASE_URL` for both (see
  [cli](cli.md#differences-from-forge-go)); Forge-Go adopts that, or Rust
  adds the split, when the second framework's `db create` is next touched.

- Stream signing key rotation (ADR 0031, rocketinsidecorp/forge-rust#14):
  both libraries verify previous keys (`Signer::new(key, previous)`,
  `SignerOptions.PreviousKeys`), but generated applications of both read only
  `FORGE_STREAMS_KEY`, so rotating it stops live updates on open pages until
  they are reloaded. Add `FORGE_STREAMS_PREVIOUS_KEYS` (comma-separated,
  validated and redacted like `FORGE_STREAMS_KEY`, at most 3) to both
  frameworks in one change, Forge-Go first since its live updates are still in
  review; until then the rotation procedure is in
  [deployment](deployment.md#rotating-the-stream-signing-key).

## Review gate

Every pull request passes the Claude Code Review before it merges to `main`,
including a screen for malicious or exfiltrating code: unexpected network
calls, access to credentials, wallets or crypto keys, obfuscated code, build
scripts and new dependencies.
