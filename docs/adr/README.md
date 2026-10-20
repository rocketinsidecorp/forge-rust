# Architecture Decision Records

ADRs record decisions whose reversal would affect public contracts, security, or
multiple crates. `Accepted` means the design is approved for implementation; it
does not mean implementation exists.

| ADR | Decision | Status |
| --- | --- | --- |
| [0001](0001-incremental-modular-workspace.md) | Incremental modular Cargo workspace | Accepted |
| [0002](0002-hexagonal-boundaries.md) | Enforced hexagonal dependency boundaries | Accepted |
| [0003](0003-uuidv7-default-identifiers.md) | UUIDv7 typed identifiers by default | Accepted |
| [0004](0004-tenancy-and-rls.md) | Explicit tenant context plus PostgreSQL RLS | Accepted |
| [0005](0005-ai-provider-and-execution-boundary.md) | Separate AI generation from controlled execution | Accepted |
| [0006](0006-tokio-and-supervised-concurrency.md) | Tokio with supervised structured lifecycles | Accepted |
| [0007](0007-sqlx-postgresql-first.md) | SQLx and PostgreSQL-first persistence | Accepted |
| [0008](0008-docker-by-default.md) | Secure Docker packaging by default | Accepted (refined by 0009) |
| [0009](0009-rails-style-generated-delivery-assets.md) | Rails-style generated delivery assets (distroless, self-probe, CI) | Accepted |
| [0010](0010-session-and-tenant-authorization-context.md) | Server-side sessions and tenant-scoped authorization context | Accepted |
| [0011](0011-postgresql-runtime-migration-roles-and-rls-context.md) | Separate PostgreSQL migration/runtime roles and transaction-local RLS context | Accepted |
| [0012](0012-structured-append-only-audit-evidence.md) | Structured append-only security audit evidence | Accepted |
| [0013](0013-persistent-principals-sessions-and-memberships.md) | Persistent principals, credential-digest sessions, and memberships | Accepted |
| [0014](0014-argon2-session-cookie-and-csrf.md) | Argon2id password authentication, opaque session cookies, and CSRF | Accepted |
| [0021](0021-selectable-database-backends.md) | Selectable database backends with `forge new -d` | Accepted |
| [0022](0022-server-rendered-views.md) | Server-rendered views, Hotwire-compatible, full-stack by default | Accepted |
| [0023](0023-form-csrf-flash-and-method-override.md) | Form CSRF (host-only double-submit cookie), cookie flash and method override for HTML applications | Accepted |
| [0024](0024-scaffold-generator.md) | `forge generate scaffold`: printed wiring and deny-by-default access | Accepted |
| [0025](0025-durable-jobs-scheduler-and-outbox.md) | Durable PostgreSQL jobs, cron schedules, transactional outbox and idempotent consumers | Accepted |
| [0026](0026-authentication-pages-and-setup.md) | Authentication pages, sign-in redirects, first-owner setup and seeds | Accepted |
| [0027](0027-rate-limiting.md) | Fixed-window rate limiting: bounded memory limiter, shared stores, fail-closed middleware | Accepted |
| [0028](0028-cache.md) | Application cache: bounded memory and shared stores, single-flight `fetch`, hashed keys | Accepted |
| [0029](0029-file-storage.md) | File storage: restricted keys, `openat` disk adapter, signed links, strict uploads | Accepted |
| [0030](0030-framework-migration-ownership.md) | Framework migration ownership: application `rollback` never reverts framework migrations; disabled features are tolerated | Accepted |
| [0031](0031-live-updates-over-server-sent-events.md) | Live updates: Turbo Streams over Server-Sent Events, signed stream names, PostgreSQL `LISTEN/NOTIFY` fanout, `scaffold --broadcast` | Accepted |
| [0032](0032-trusted-proxies-and-client-ip.md) | Trusted proxies and the client address: `FORGE_HTTP_TRUSTED_PROXIES`, right-to-left `X-Forwarded-For`, `Request::client_ip` | Accepted |
| [0033](0033-telemetry.md) | Forge-owned telemetry over OpenTelemetry protocols: `FORGE_TELEMETRY_*`, W3C propagation, OTLP/HTTP JSON, Prometheus listener, correlated log export | Accepted |
| [0034](0034-local-observability-stack.md) | Preconfigured local observability stack: `compose.observability.yaml` with the Collector, Prometheus, Tempo, Loki and Grafana | Accepted |
| [0035](0035-mail.md) | Mail: validated MIME, a Forge-owned TLS-enforcing SMTP client, development and test senders, askama templates, previews, password reset emails | Accepted |
| [0036](0036-application-lifecycle-commands.md) | Application lifecycle commands: `forge dev`, `migrate`, `rollback`, `setup`, `seed`, `db`, `doctor`, `version` delegate to the application's own subcommands; destructive-command protection; process ownership | Accepted |
| [0037](0037-internationalization.md) | Internationalization: Rails-style JSON catalogs in `config/locales`, CLDR plural rules, locale negotiation, `t`/`l`/`number`/`locale` helpers, `forge new --locale`, `forge i18n check` (port of Forge-Go ADR 0042) | Accepted |

New ADRs use the next four-digit number. A changed decision receives a new ADR
that marks the prior record `Superseded`; accepted records are not rewritten to
hide history. A decision that is extended without being reversed receives a
new ADR that refines the prior one; the prior record stays `Accepted` and gains
only a `Refined by` note.
