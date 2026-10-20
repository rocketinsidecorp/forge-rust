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
| First tenant and owner, seeds | `db:seed` | Done (`forge setup`, `forge seed`) | Done (`setup`, `seed`) |
| Multi-tenancy with database enforcement | (gems) | Done (PostgreSQL RLS) | Done (PostgreSQL RLS) |
| TOTP MFA | (gems) | Done | Planned |
| Persistent jobs, cron, outbox | Solid Queue | Done | Done (framework crates; `forge new` wiring planned) |
| Mailer with previews | Action Mailer | Done | Planned |
| Rate limiting | `rate_limit` | Done | Done |
| Cache | Solid Cache | Done | Done |
| File storage with signed URLs | Active Storage | Done (disk) | Done (disk) |
| Import maps, Turbo, Stimulus | Hotwire | In review | Done |
| Turbo Streams over SSE | Action Cable | In review | Planned |
| Traces, metrics, log correlation | (gems) | Done (OpenTelemetry, Prometheus) | Planned |
| Preconfigured Prometheus, Grafana, Tempo, Loki | — | Planned | Planned |
| Zero-downtime deploys, rollback, remote logs | Kamal | In review (`forge deploy`) | Planned |
| Live reload and database lifecycle | `bin/dev`, `db:*` | In review (`forge dev`, `forge db`) | Planned |
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

## Review gate

Every pull request passes the Claude Code Review before it merges to `main`,
including a screen for malicious or exfiltrating code: unexpected network
calls, access to credentials, wallets or crypto keys, obfuscated code, build
scripts and new dependencies.
