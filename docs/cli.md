# Forge CLI Reference

Status: Phase 3 in progress. This page documents the `forge` executable
(crate `forge-cli`) and the subcommands built into generated applications.

## Installation

```sh
cargo install --locked --git https://github.com/rocketinsidecorp/forge-rust forge-cli
```

`--locked` installs with the dependency graph from the repository's committed
`Cargo.lock`. Framework contributors can use `cargo run -p forge-cli -- <command>`
from a checkout.

## Command summary

```text
forge id
forge new <name> [-d <DATABASE>] [--api] [--locale en|es|pt-BR] [--skip-docker] [--skip-ci] [--skip-lockfile] [--forge-path <PATH>]
forge generate migration <name>
forge generate authentication [--dry-run]
forge generate scaffold <name> <field:type>... (--global | --tenant-scoped) [--plural <PLURAL>] [--broadcast] [--dry-run]
forge generate model <name> <field:type>... (--global | --tenant-scoped) [--plural <PLURAL>] [--dry-run]
forge generate controller <name> <action>... [--api] [--public] [--dry-run]
forge generate mailer <name> <method[:field,...]>... [--dry-run]
forge generate job <name> [<field:type>...] --queue [--dry-run]
forge destroy model|scaffold|controller|mailer|job <arguments of the generator> [--dry-run] [--force] [--migrations]
forge check
forge test [ARGS]
forge format
forge lint
forge build [NAME]
forge version
forge doctor [--json]
forge dev [NAME] [--no-reload] [--poll-interval D] [--debounce D]
forge migrate [status]
forge rollback [--steps N]
forge setup --tenant SLUG --email EMAIL [--organization NAME] [--password-stdin]
forge seed
forge db create | drop | reset | setup | prepare | migrate | status | seed
forge db rollback [--steps N]
forge i18n check [--json]
```

The quality and build commands run `cargo` directly (no shell) in the current
directory. They are thin wrappers, and the underlying Cargo commands are always
valid to run yourself. A failed Cargo invocation makes `forge` exit with a
non-zero status. The lifecycle commands (`dev`, `migrate`, `rollback`, `setup`,
`seed`, `db`) are described under
[Application lifecycle commands](#application-lifecycle-commands).

## `forge id`

Prints a new UUIDv7 identifier to standard output. UUIDv7 values sort by
creation time and are Forge's default identifier format
([ADR 0003](adr/0003-uuidv7-default-identifiers.md)).

## `forge new <name>`

Creates a new application in `./<name>`. Like `rails new`, the application is
full-stack: server-rendered HTML views with a layout, digested assets, Turbo
and Stimulus through an import map, a strict Content-Security-Policy,
CSRF-protected forms and flash messages ([Views](#views),
[ADR 0022](adr/0022-server-rendered-views.md)). `--api` generates a JSON-only
application instead. `<name>` becomes the Cargo package name
and the binary name. It must be a valid Cargo package name: start with a
lowercase ASCII letter and use only lowercase letters, digits, `-` and `_`.
`forge new` refuses to write into an existing path.

| Flag | Effect |
| --- | --- |
| `--api` | Generate an API-only application, like `rails new --api`: JSON handlers, no `templates/`, `assets/`, view models, Askama dependency or HTML request layers. |
| `--skip-docker` | Do not generate `Dockerfile`, `.dockerignore`, `compose.yaml` or the observability stack (`compose.observability.yaml`, `observability/`). |
| `--skip-ci` | Do not generate `.github/workflows/ci.yml` or `.github/dependabot.yml`. |
| `-d`, `--database <DATABASE>` | `postgresql` (default, alias `postgres`), `mysql`, `mariadb`, `sqlite3` (alias `sqlite`) or `none`. `none` omits SQLx, migrations, database Compose services and bootstrap hooks. See [Database backends](#database-backends). |
| `--skip-lockfile` | Do not run `cargo generate-lockfile` after generation. |
| `--forge-path <PATH>` | Depend on a local `forge` crate by path instead of the Git tag. For framework contributors, similar to `rails new --dev`. Point it at `crates/forge` of a checkout: full-stack applications also take their `forge-views` build dependency from the sibling `crates/forge-views`. |

### The `forge` dependency

By default the generated `Cargo.toml` depends on `forge` from this repository at
Git tag `v<cli version>`, so an application is created against the same
framework version as the CLI that generated it. Full-stack applications enable
the `forge` feature `views` and add `askama` plus a build dependency on
`forge-views` (feature `build` only, from the same tag) for `build.rs`.
Full-stack applications with a database also enable `broadcast` (live
updates, [ADR 0031](adr/0031-live-updates-over-server-sent-events.md)), so
that `forge generate scaffold --broadcast` works without editing
`Cargo.toml`: like `views`, `forge new` chooses features once and the
generators never edit existing files. The feature only compiles
`forge::broadcast` and `Composition::broadcast`: nothing is served, no
connection is opened and no task runs until the wiring printed by a
`--broadcast` scaffold (`Streams::register`) is added. An application that
will never broadcast may remove `"broadcast"` from the features; a later
`--broadcast` scaffold then asks for it back. Until a release tag such as
`v0.1.0` is pushed, that dependency cannot resolve. Use `--forge-path` with a
local checkout in the meantime.

`--forge-path` writes a Cargo path dependency, which Cargo resolves relative to
the generated application's directory. An absolute path avoids confusion. A path
outside the application directory is not visible to `docker build`, because
the build context is the application directory. Vendor the framework into the
application first, as `scripts/e2e-generated-app.sh` does.

### Lockfile

After writing files, `forge new` runs `cargo generate-lockfile` in the new
application unless `--skip-lockfile` is given. The generated Dockerfile builds
with `--locked` and needs a committed `Cargo.lock`. If lockfile generation fails
(for example, offline or before the release tag exists), the application is kept
and `forge new` prints a warning explaining how to create the lockfile before
running `docker build`.

### PostgreSQL and migrations

PostgreSQL is generated by default. The generated package depends on SQLx only
at the outer infrastructure/bootstrap boundary. It includes `build.rs`,
`.gitattributes` with LF-normalized SQL, a `migrations/` directory, and the
framework's append-only `forge_audit_events` migration, identity/session
migration, `infrastructure::audit::PgAuditSink`,
`infrastructure::auth::PostgresSessionStore`,
`infrastructure::tenancy::PgMembershipStore`, and concrete
`infrastructure::security` authentication mechanisms.

Create a reversible migration:

```sh
forge generate migration create_customers
```

Forge creates a versioned `.up.sql` / `.down.sql` pair using a positive,
time-sortable numeric migration version (nanoseconds since the Unix epoch).
A version that is already taken, by a file in `migrations/` or by a
framework migration (`forge_db::FRAMEWORK_MIGRATIONS`, whose versions are
reserved, [ADR 0030](adr/0030-framework-migration-ownership.md)), moves to the
next free second; `forge generate scaffold` follows the same rule. It creates
files with create-new semantics and never overwrites an existing migration.

Generated database applications separate runtime and migration credentials.
Both values are treated as secrets by Forge configuration and are redacted from
ordinary Debug and serialization output.

```sh
export FORGE_MIGRATION_DATABASE_URL='postgres://app_migrator:app_migrator@127.0.0.1:5432/shop'
forge migrate                  # or: cargo run -- migrate
forge rollback                 # or: cargo run -- rollback

export FORGE_DATABASE_URL='postgres://app_runtime:app_runtime@127.0.0.1:5432/shop'
forge dev                      # or: cargo run
```

`migrate` and `rollback` require `FORGE_MIGRATION_DATABASE_URL`; they do
not fall back to the runtime credential. The serving process should receive only
`FORGE_DATABASE_URL`.

`rollback` reverts the application's newest applied migration, one per call.
It never reverts migrations shipped by Forge crates: the job and outbox schemas
have their own bookkeeping tables, and the rate-limit and cache migrations
merged with `combine_migrations` are skipped even though their versions are
higher. When only those remain, `rollback` exits with status 1 and changes
nothing. Removing a store from `combine_migrations` later leaves its table in
place and `migrate` keeps working
([ADR 0030](adr/0030-framework-migration-ownership.md)).

With generated Docker assets, `docker/postgres/init.sql` creates separate
development-only `app_migrator` and `app_runtime` roles. The runtime role is
not schema owner, cannot create databases and has `NOBYPASSRLS`; the migration
role may create databases so that `forge db reset` works against the container. Generated
`src/infrastructure/database/mod.rs` provides `begin_tenant_transaction`,
which installs the authorized tenant and principal as transaction-local
PostgreSQL settings for RLS policies.

The generated security module uses Argon2id for password authentication, issues
independent 256-bit bearer/CSRF secrets from the OS CSPRNG, persists only SHA-256
digests, emits a Secure/HttpOnly/SameSite=Lax `__Host-forge_session` cookie,
and enforces the session-bound CSRF token on unsafe cookie-authenticated
methods: the `x-csrf-token` header, plus the `csrf_token` form field in
full-stack applications. These mechanisms are infrastructure helpers.

Full-stack PostgreSQL applications also get sign-in, sign-out and
password-reset pages, the `setup` command and the authentication migration,
like `rails generate authentication` (see
[`forge generate authentication`](#forge-generate-authentication), which adds
the same files to an older application, and
[ADR 0026](adr/0026-authentication-pages-and-setup.md)). They are generated
application code, so routes stay reviewable and editable; `--api` and
non-PostgreSQL applications do not get them.

### Generated tree

`forge new shop` produces (optional files annotated):

```text
shop/
├── .dockerignore            # omitted with --skip-docker; excludes rust-toolchain.toml
├── .github/                 # omitted with --skip-ci
│   ├── dependabot.yml       #   cargo, docker, github-actions updates
│   └── workflows/
│       └── ci.yml           #   fmt check, clippy -D warnings, tests, image build
├── .gitattributes           # omitted with -d none; SQL migrations use LF
├── .gitignore
├── assets/                  # omitted with --api; digested and embedded in the binary
│   ├── application.css
│   ├── application.js       #   imports Turbo, starts Stimulus
│   └── forge-rust-logo.png  #   shown on the welcome page
├── build.rs                  # embeds assets/ (omitted with --api); reruns on migration changes (omitted with -d none)
├── Cargo.lock               # omitted with --skip-lockfile (or if generation fails)
├── Cargo.toml               # package `shop`, lib `app`, deps: forge, serde, askama, SQLx + auth mechanism crates
├── Dockerfile               # omitted with --skip-docker
├── README.md
├── compose.yaml             # omitted with --skip-docker; 127.0.0.1:3000, stop_grace_period 20s
├── compose.observability.yaml  # omitted with --skip-docker; opt-in overlay: collector, Prometheus, Tempo, Loki, Grafana
├── docker/                   # omitted with --skip-docker
│   └── postgres/
│       └── init.sql          # PostgreSQL only; local least-privilege roles
├── migrations/              # omitted with -d none; Forge + app SQL
│   ├── 00000000000000000001_forge_audit.up.sql
│   ├── 00000000000000000001_forge_audit.down.sql
│   ├── 00000000000000000002_forge_identity.up.sql
│   ├── 00000000000000000002_forge_identity.down.sql
│   ├── 00000000000000000003_forge_authentication.up.sql    # PostgreSQL full-stack: tenants, reset tokens
│   └── 00000000000000000003_forge_authentication.down.sql
├── observability/           # omitted with --skip-docker; read-only config of the stack (ADR 0034)
│   ├── otel-collector.yaml
│   ├── prometheus.yml
│   ├── tempo.yaml
│   ├── loki.yaml
│   └── grafana/             #   data sources and the "Forge application" dashboard
├── rust-toolchain.toml      # pinned to the framework toolchain
├── src/
│   ├── main.rs              # binary `shop`: calls app::bootstrap::run()
│   ├── lib.rs               # library `app`: declares the rings
│   ├── domain/
│   ├── application/
│   ├── adapters/
│   │   └── http/
│   │       ├── mod.rs       # routes; HTML handlers (JSON with --api)
│   │       ├── authentication.rs  # PostgreSQL full-stack: sign-in, sign-out, password reset
│   │       └── views/       # omitted with --api; Site, Layout, view models
│   ├── infrastructure/      # PostgreSQL: audit, auth, tenancy, accounts (tenants, resets, setup)
│   └── bootstrap/
│       ├── mod.rs           # run(): App with migrations, compose and commands
│       ├── seeds.rs         # database applications: `seed`, idempotent, one transaction
│       └── setup.rs         # PostgreSQL full-stack: `setup`, first tenant and owner
├── templates/               # omitted with --api; Askama templates
│   ├── layouts/
│   │   ├── application.html
│   │   └── _flash.html
│   ├── passwords/           # PostgreSQL full-stack: new.html, edit.html
│   ├── sessions/            # PostgreSQL full-stack: new.html, _sign_out.html
│   └── welcome/
│       └── index.html
└── tests/
    ├── architecture.rs      # ring import boundaries
    ├── authentication_http.rs      # PostgreSQL full-stack: the pages with in-memory ports
    ├── authentication_postgres.rs  # PostgreSQL full-stack: setup and reset store (ignored without FORGE_TEST_DATABASE_URL)
    ├── container.rs         # omitted with --skip-docker; Dockerfile RUST_VERSION == toolchain
    └── http.rs              # omitted with --api; layout, CSP, assets, CSRF enforcement
```

[`examples/hello-forge`](../examples/hello-forge) is the committed
database-free, full-stack output of
`forge new hello-forge --skip-ci -d none --skip-lockfile --forge-path ../../crates/forge`
and is the authoritative reference for exact file contents. A golden test in
`forge-cli` fails if generator output drifts from it. Like a real user
application, it is a standalone package outside the framework workspace. Its
`Cargo.lock` is created separately and committed. Framework CI runs format,
Clippy and tests against it.

Notes on the generated package:

- It is a standalone package with no `[workspace]` table.
- `src/main.rs` only calls `app::bootstrap::run()`. The composition root in
  `src/bootstrap/mod.rs` builds `forge::App`, which owns the Tokio runtime, so
  the application never uses `#[tokio::main]`:

  ```rust
  pub fn run() -> ExitCode {
      forge::App::new(env!("CARGO_PKG_NAME"))
          .version(env!("CARGO_PKG_VERSION"))
          .compose(compose)
          .run()
  }

  fn compose(composition: &mut forge::Composition<'_>) -> Result<(), forge::ComposeError> {
      let site = crate::adapters::http::site(composition.router())?;
      crate::adapters::http::routes(composition.router(), &site)?;
      Ok(())
  }
  ```

  The full-stack HTTP adapter registers through `App::compose` because loading
  the asset table can fail at startup, and because resources generated by
  `forge generate scaffold` are wired at the end of `compose` with the
  database the composition provides; `--api` applications use
  `.routes(crate::adapters::http::routes)`.
- Full-stack database applications have a `forge` dev-dependency with the
  `testing` feature, so repository tests can use
  `forge::db::testing::TestDatabase`.

- `tests/architecture.rs` is a real test that scans source imports. It fails
  when `src/domain` imports `application`, `adapters`, `infrastructure`,
  `bootstrap`, `forge::http`, `forge::views`, `askama`, or protocol/runtime
  crates such as `tokio`, `hyper` or `sqlx`, and when `src/application` imports
  `adapters`, `infrastructure`, `bootstrap`, `forge::http`, `forge::views` or
  `askama` ([ADR 0002](adr/0002-hexagonal-boundaries.md)).
- `tests/http.rs` (full-stack only) dispatches requests through the router
  without a network: the welcome page is HTML with the layout, the CSP and a
  CSRF meta tag matching the `__Host-forge_csrf` cookie; Turbo and Stimulus
  are served digested; a form `POST` without the page's token is `403` and
  with it reaches the handler.
- `tests/container.rs` (generated only with the Docker assets) fails when the
  Dockerfile's `ARG RUST_VERSION` differs from the channel in
  `rust-toolchain.toml`. The builder image therefore always uses the same
  toolchain as local builds. `.dockerignore` excludes `rust-toolchain.toml`,
  so the builder image uses its preinstalled toolchain instead of downloading
  components.
- The Docker assets are described in [deployment](deployment.md#container-image).
- With the default database capability, Compose includes PostgreSQL, waits until
  the database and least-privilege roles are initialized, runs a one-shot
  migration container with the migration credential, then starts the app with
  only the runtime credential. `-d none` removes those database pieces.

### Views

Full-stack applications follow [ADR 0022](adr/0022-server-rendered-views.md)
and [ADR 0023](adr/0023-form-csrf-flash-and-method-override.md). The guide,
including a walkthrough that adds a form, is [docs/views.md](views.md). In
short: Askama templates in `templates/` are checked against view models in
`src/adapters/http/views/` at compile time; `build.rs` embeds `assets/` with
content digests; `Site::register` adds the asset routes and the request
layers (strict CSP on every response, CSRF protection with the `_method`
override, flash messages). The CSRF and flash cookies are `__Host-` cookies:
browsers accept them on HTTPS origins and on `http://localhost` and
`http://127.0.0.1`.

### Translations

The pages are translated ([ADR 0037](adr/0037-internationalization.md),
[i18n](i18n.md)): `config/locales/en.json` holds the layout and welcome page
text and the number and date formats, `authentication.en.json` the sign-in
pages (PostgreSQL). `--locale es` or `--locale pt-BR` adds that locale's
files and makes it the default locale (`DEFAULT_LOCALE` in
`src/adapters/http/views/locales.rs`); `FORGE_I18N_DEFAULT_LOCALE` overrides
it per deployment. `--locale` cannot be combined with `--api`.

### Database backends

Like `rails new -d`, `forge new -d <DATABASE>` selects the backend
([ADR 0021](adr/0021-selectable-database-backends.md)). IDs are always UUIDv7
generated in Rust; generated SQL never uses database UUID generators.

| `-d` | Framework call | Compose service | Local `FORGE_DATABASE_URL` |
| --- | --- | --- | --- |
| `postgresql` (default) | `.postgres(sqlx::migrate!())` | `postgres:18` with migrator/runtime roles | `postgres://app_runtime:…@127.0.0.1:5432/<app>` (migrations use `app_migrator`) |
| `mysql` | `.mysql(sqlx::migrate!())` | `mysql:8.4` | `mysql://…@127.0.0.1:3306/<app>` |
| `mariadb` | `.mysql(sqlx::migrate!())` | `mariadb:11.8` | `mariadb://…@127.0.0.1:3306/<app>` |
| `sqlite3` | `.sqlite(sqlx::migrate!())` | none; `/data` volume owned by uid 65532 | `sqlite://storage/development.sqlite3?mode=rwc` |
| `none` | — | — | — |

PostgreSQL-only modules are not generated for `mysql`, `mariadb` or `sqlite3`
yet, and `forge new` prints which ones: row-level security and the
runtime/migrator roles, audit event persistence, identity/session/credential
persistence, and tenant membership persistence. Tenant isolation on those
backends rests on the application layer (bind the tenant predicate from
`TenantContext`); use PostgreSQL when the database must enforce isolation.

## `forge generate authentication`

```sh
forge generate authentication --dry-run
forge generate authentication
```

The `rails generate authentication` of Forge
([ADR 0026](adr/0026-authentication-pages-and-setup.md)). `forge new`
already generates these files for full-stack PostgreSQL applications; run
the generator in the root of an older one to add them. It writes, all or
nothing and byte-identical to `forge new`:

```text
migrations/00000000000000000003_forge_authentication.{up,down}.sql   forge_tenants, forge_password_resets, runtime grants
src/infrastructure/accounts/mod.rs          PgTenantDirectory, PgPasswordResetStore, provision_owner
src/adapters/http/authentication.rs         routes, handlers, AuthenticationPorts, SignInRedirect layer
src/adapters/http/views/authentication.rs   view models
templates/sessions/{new,_sign_out}.html     sign-in page and sign-out button
templates/passwords/{new,edit}.html         reset request and new-password pages
src/bootstrap/setup.rs                      the `setup` command
tests/authentication_http.rs                the pages with in-memory ports
tests/authentication_postgres.rs            setup and reset tokens against TestDatabase
```

It refuses to overwrite any of them (or an existing
`*_forge_authentication.up.sql`), never edits an existing file and prints
the wiring: four `pub mod` lines, the statements to append to `fn compose`
(the stores, Argon2id, two `MemoryLoginThrottle`s, the reset notifier from
`forge::mail::password_reset_notifier`, the `/_mail` reset-email preview and
the audit sink) and the
`.command("setup", setup::SUMMARY, setup::run)` line for `fn run`.
`--dry-run` prints the same without writing. Applications created with
`--api`, `-d none`, MySQL, MariaDB or SQLite are refused with the reason:
Forge persists identities, sessions and memberships only in PostgreSQL today.

| Method | Path | Behavior |
| --- | --- | --- |
| `GET` | `/session/new` | organization (tenant slug, prefilled from `?tenant=`), email, password; hidden validated `return_to`; a sign-out button when already signed in |
| `POST` | `/session` | `303` to the validated return path (default `/`) with "Signed in."; `422` with one message for any wrong field; `429` with `Retry-After` when throttled |
| `DELETE` | `/session` | `_method=delete` form: revoke the session, clear the cookie, rotate the CSRF token, `303` to the sign-in page |
| `GET`, `POST` | `/passwords/new`, `/passwords` | request a reset link: the same `303` and notice whether or not the account exists |
| `GET`, `PATCH` | `/passwords/edit?token=`, `/passwords` | choose a new password (12 characters or more); every session of the account is revoked |

Security behavior, all covered by the generated tests:

- every handler that changes credentials refuses a request the CSRF layer
  did not verify; sign-in issues a new session and CSRF token and revokes
  the session the browser held;
- a wrong organization, email or password, a disabled account and an
  inactive membership all answer the same; unknown logins still cost one
  Argon2id hash; sign-in and reset attempts are throttled in process, per
  15 minutes: 5 per login and source, 50 per login, 300 per source (IPv6
  per /64);
- logins are canonical (`forge::auth::CanonicalLogin`: trimmed, ASCII only,
  lowercased); a non-ASCII address is an unknown account, so a look-alike
  spelling can neither match nor get its own throttle count, and reset
  links go to the login as stored, never as typed;
- reset requests are handled by a bounded background queue owned by the
  server's lifecycle, and the page never answers in less than 100 ms, so
  timing does not tell whether an account exists; a reset submission is
  checked for a known, unused, unexpired token before any Argon2id work;
- reset tokens are 256-bit, single use, valid 15 minutes, bound to the
  account and tenant, stored as SHA-256 digests and never logged; the reset
  pages are `no-store` and `Referrer-Policy: no-referrer`;
- `SignInRedirect` sends a browser that gets `401` from an access policy to
  `/session/new?return_to=<page>`; JSON clients keep the `401`.

Reset links are delivered by a `forge::auth::PasswordResetNotifier`: the
generated composition calls `forge::mail::password_reset_notifier`, which
emails them through the sender `FORGE_MAIL_URL` selects once
`FORGE_MAIL_URL`, `FORGE_MAIL_FROM` and `FORGE_AUTH_PASSWORD_RESET_URL` are
set, and otherwise returns `UndeliverablePasswordResets`, which delivers
nothing and reports `NotConfigured` (logged, never shown); the page answers
the same either way ([mail](mail.md#password-reset-emails)). With
`FORGE_MAIL_PREVIEW=true` in development the email is previewed at
`/_mail/password_reset`. MFA (TOTP) is not provided yet.

## `forge generate scaffold`

```sh
forge generate scaffold post title:string body:text published:boolean --global
forge generate scaffold invoice number:string total_cents:integer due_at:timestamp? --tenant-scoped
```

The `rails generate scaffold` of Forge ([ADR 0022](adr/0022-server-rendered-views.md)
decision 7, [ADR 0024](adr/0024-scaffold-generator.md)): run in the root of a
full-stack database application, it generates a server-rendered HTML CRUD
(index, show, new/create, edit/update, delete) through every ring, for the
application's database backend, with tests. The walkthrough is in
[docs/views.md](views.md#scaffolding-a-resource).

| Argument | Meaning |
| --- | --- |
| `<name>` | Singular resource name, `snake_case` or `PascalCase` (`line_item`, `LineItem`). It names the entity type (`LineItem`), the table, routes and modules use the plural. |
| `<field:type>...` | At least one field. Types: `string` (at most 255 characters), `text` (at most 65,535), `integer` (`i64`; store money as integer minor units, there is no float), `boolean`, `timestamp` (`forge::core::Timestamp`, UTC; the form edits whole seconds and refuses sub-second input), `uuid` (`forge::core::Uuid`, any version). Append `?` for an optional (nullable) field: `due_at:timestamp?`. |
| `--global` | Rows are shared by every tenant. Routes `/<plural>/...`, closed by `DenyAll` until the application decides who may use them. |
| `--tenant-scoped` | Rows belong to a tenant. Routes `/tenants/{tenant_id}/<plural>/...`, `tenant_id` bound from the authorized `TenantContext` in every statement, plus forced row-level security on PostgreSQL. Exactly one of `--global` and `--tenant-scoped` is required. |
| `--plural <PLURAL>` | The plural when the English rules (`-y` to `-ies`; `-s`, `-x`, `-z`, `-ch`, `-sh` take `-es`; otherwise `-s`) get it wrong. A name that already looks plural is refused unless `--plural` is given. |
| `--broadcast` | Live updates ([ADR 0031](adr/0031-live-updates-over-server-sent-events.md)): create, update and delete publish Turbo Streams and the index page subscribes over Server-Sent Events. Writes `src/adapters/http/streams.rs` once and `templates/<plural>/_row.html`; production needs `FORGE_STREAMS_KEY` ([views](views.md#live-updates)). |
| `--dry-run` | Print the files that would be created and the wiring; write nothing. |

Names are validated before anything is read: snake_case ASCII, at most 40
characters, not a Rust keyword or common SQL keyword, not a reserved column
(`id`, `tenant_id`, `version`, `created_at`, `updated_at`) or a name the
generated code already uses (`csrf_token`, `clone`, ...), at most 32 fields
without duplicates, and constraint names within PostgreSQL's 63-byte limit.

### What it generates

For `post title:string body:text published:boolean --global` in a PostgreSQL
application:

```text
migrations/<version>_create_posts.up.sql     table, UUIDv7 key from Rust, version, timestamps, checks, grants
migrations/<version>_create_posts.down.sql   DROP TABLE
src/domain/entities/post.rs                  Post, PostId, PostAttributes, typed validation errors + unit tests
src/application/ports/post_repository.rs     PostRepository port, PostPage, PostStoreError
src/application/use_cases/posts.rs           PostService (list/find/create/update/delete), permissions
src/infrastructure/database/posts.rs         PgPostRepository (SQLx, optimistic locking, cursor pages)
src/adapters/http/access.rs                  access policies (first scaffold only; reused after)
src/adapters/http/posts.rs                   routes and handlers
src/adapters/http/views/posts.rs             Askama view models
templates/posts/{index,show,new,edit,_form,not_found}.html
tests/posts_http.rs                          HTML flow with an in-memory port
tests/posts_repository.rs                    repository against TestDatabase
```

Generated Rust is formatted with the application's pinned `rustfmt` before
anything is written. The generator writes only new files: if any of them, or
a `*_create_<plural>` migration, already exists it refuses and writes nothing;
a failed write removes what was written. Output is deterministic (golden
fixtures under `crates/forge-cli/tests/fixtures/scaffold/`).

| Backend | Migration | Repository | Tenant isolation |
| --- | --- | --- | --- |
| PostgreSQL | `uuid`, `timestamptz`, `bigint`, `boolean`, `text` with length checks; `REVOKE ALL FROM PUBLIC` and grants to `app_runtime` when the role exists | `PgPostRepository` | `tenant_id` predicate in every statement, each operation in `begin_tenant_transaction`, forced RLS policy on `forge.tenant_id` |
| MySQL / MariaDB | `BINARY(16)`, `DATETIME(6)`, `BIGINT`, `BOOLEAN`, `VARCHAR(255)`, `MEDIUMTEXT` | `MySqlPostRepository` | `tenant_id` predicate in every statement (no RLS, ADR 0021) |
| SQLite | `BLOB`, `TEXT`, `INTEGER` with boolean and length checks | `SqlitePostRepository` | `tenant_id` predicate in every statement (no RLS, ADR 0021) |

### Wiring

`forge generate scaffold` never edits an existing file. It prints the module
declarations to add (one `pub mod` line in each ring's `mod.rs`, kept sorted)
and the statements to append to the `compose` function in
`src/bootstrap/mod.rs`, already formatted with full paths:

```rust
    // posts: global. DenyAll refuses every request until you decide who may
    // use them: implement GlobalAccess (src/adapters/http/access.rs).
    let posts = crate::application::use_cases::posts::PostService::new(std::sync::Arc::new(
        crate::infrastructure::database::posts::PgPostRepository::new(
            composition.postgres()?.clone(),
        ),
    ));
    crate::adapters::http::posts::routes(
        composition.router(),
        &site,
        posts,
        std::sync::Arc::new(crate::adapters::http::access::DenyAll),
    )?;
```

With `--broadcast` the first broadcasting scaffold also prints
`pub mod streams;` and, before its routes,

```rust
    let streams =
        crate::adapters::http::streams::Streams::register(composition, forge::Fanout::Postgres)?;
```

(`forge::Fanout::InProcess` on MySQL, MariaDB and SQLite, which have no
`LISTEN/NOTIFY`); every broadcasting resource passes `&streams` as the last
argument of its `routes` call.

### Access

Every route asks its access policy for `<plural>:read` (index, show) or
`<plural>:write` (new, create, edit, update, delete) before doing anything.
Nothing is public by default:

- **Global** resources are wired with `DenyAll` (`403`). Replace it with your
  own `GlobalAccess` implementation in `compose`; that is the explicit
  decision to open the routes. Tenant roles cannot authorize global rows
  because every tenant reads them.
- **Tenant-scoped** resources on PostgreSQL are wired with
  `SessionTenantAccess`: a live session (`__Host-forge_session`), the
  session-bound CSRF token on unsafe methods, an active membership in the
  route's `{tenant_id}` and an RBAC grant. The printed `RbacPolicy` grants
  both permissions to the `owner` role, the role `setup` gives the first
  member ([ADR 0026](adr/0026-authentication-pages-and-setup.md)), and
  nothing to any other role. Permissions match exactly, so a role that edits
  needs both grants (the index and show pages, and the redirect after a
  create or update, need `<plural>:read`):

  ```rust
  let mut policy = forge::security::RbacPolicy::new();
  let owner = forge::security::Role::new("owner").map_err(forge::ComposeError::other)?;
  for permission in [
      crate::application::use_cases::posts::READ_PERMISSION,
      crate::application::use_cases::posts::WRITE_PERMISSION,
  ] {
      let permission =
          forge::security::Permission::new(permission).map_err(forge::ComposeError::other)?;
      policy.allow(owner.clone(), permission);
  }
  ```

  The output also prints an example that grants a read-only `viewer` role.

  The use cases check the same grants (defense in depth): listing needs
  `<plural>:read`, changes need `<plural>:write`, and loading one record
  accepts either because the edit and update pages load the record they
  change under the write grant.

  On MySQL, MariaDB and SQLite, which have no Forge session or membership
  persistence yet, tenant-scoped resources are wired with `DenyAll` until the
  application provides `SessionStore` and `MembershipStore` implementations.

Denials answer `401` (no valid session), `403` or `503` (identity store
unavailable) with a generic body. With the sign-in pages, browsers get a
`303` to `/session/new?return_to=<page>` instead of the `401`.

### Refusals

The scaffold refuses, with an explanation and without writing:

- `forge new --api` applications: the scaffold generates HTML pages; JSON
  scaffolds are not available yet.
- `forge new -d none` applications: there is no database.
- Applications without the `compose` function, the `forge` dev-dependency
  with the `testing` feature or (PostgreSQL tenant resources)
  `begin_tenant_transaction`, all of which `forge new` generates.
- An `src/adapters/http/access.rs` that lacks the generated traits.

## `forge generate model|controller|mailer|job` and `forge destroy`

```sh
forge generate model post title:string 'body:text?' --global          # the scaffold's model files, no pages
forge generate controller pages index about_us --public               # read-only GET pages
forge generate controller reports monthly                             # guarded: asks for reports:read
forge generate controller status_checks index --api                   # JSON under /v1
forge generate mailer user welcome:name,url password_reset            # typed emails, templates, previews
forge generate job send_welcome user_id:uuid 'note:string?' --queue   # persistent PostgreSQL queue job
forge destroy controller pages index about_us --public --dry-run      # what would be removed
forge destroy model post title:string 'body:text?' --global --migrations
```

The `rails g` and `rails d` of Forge, with Forge-Go's commands and rules
([ADR 0038](adr/0038-generators-and-destroy.md)). Every generator writes only
new files, all or nothing, refuses symlinks and prints the module
declarations and `fn compose` lines to add. Guarded controllers ask the
access policy for `NAME:read` and reuse the permission of the model of the
same name; a model or scaffold refuses a permission another module already
declares. `forge destroy` takes the generator's arguments and removes a file
only while it is byte-identical to what the generator writes (`--force`
otherwise); migrations stay unless `--migrations`, translated locale files
are kept, and the shared access, streams and jobs modules are never removed.
Details, files and refusals: [generators](generators.md).

## `forge i18n check`

```sh
forge i18n check          # human report; exit 1 when a locale lacks keys or plural forms
forge i18n check --json   # {"schema_version":1,"locales":[...],"keys":N,"referenced":N,"missing":{...},"plural_forms_missing":{...},"unused":[...]}
```

Loads `config/locales` with the application's own rules (a malformed file,
a duplicate key or a non-canonical file name is an error, exit 1), compares
every locale with the keys of all the others and with the literal keys of
`.t("KEY")` in `*.html` and `*.rs` files and of `translate(LOCALE, "KEY")` /
`translate_html(LOCALE, "KEY")` (the `tests`, `target`, `vendor`,
`node_modules`, `tmp`, `bin`, `storage` and dot directories are skipped),
reports the plural forms each locale's rule needs for 0 to 1000, and lists
keys nothing references (informational; `date.`, `time.` and `number.` are
data). It reads files only. Same report and exit status as Forge-Go
([i18n](i18n.md#forge-i18n-check)).

## `forge check`, `forge test`, `forge format`, `forge lint`

| Command | Runs |
| --- | --- |
| `forge check` | `cargo check --all-targets --all-features` |
| `forge test` | `cargo test --all-targets --all-features` |
| `forge test ARGS...` | `cargo test ARGS...`: arguments replace the defaults, like `forge test ARGS` replaces `./...` in Forge-Go (`forge test --lib orders -- --nocapture`) |
| `forge format` | `cargo fmt --all` |
| `forge lint` | `cargo clippy --all-targets --all-features -- -D warnings` |

## `forge build`

Runs `cargo build --release --locked`, producing `target/release/<name>`. This
is the same release build the Dockerfile performs. It requires a `Cargo.lock`
that is up to date with `Cargo.toml`. `forge build NAME` builds one binary
(`--bin NAME`) of a package that has several.

## Application lifecycle commands

`forge version`, `forge doctor`, `forge dev`, `forge migrate`, `forge rollback`,
`forge setup`, `forge seed` and `forge db ...` are the same commands, with the
same names, flags and messages, that the Forge-Go CLI has
([ADR 0036](adr/0036-application-lifecycle-commands.md)). `dev`, `migrate`,
`rollback`, `setup`, `seed` and `db` run inside a generated application (a Cargo
package that depends on `forge`) and refuse other directories.

```sh
forge version                      # forge 0.1.0 (also forge --version)
forge doctor                       # toolchain, FORGE_* settings, database server reachable?
forge doctor --json                # {"schema_version":1,"ok":true,"environment":...,"diagnostics":[]}
forge dev                          # http://127.0.0.1:3000, rebuilds and restarts on save
forge dev --poll-interval 1s       # scan less often (large trees, slow disks)
forge dev --no-reload              # build and run once
forge migrate                      # apply pending migrations
forge migrate status               # applied / pending / modified / disabled / missing
forge rollback --steps 2           # your newest two migrations; never a Forge-owned table
forge setup --tenant acme --email owner@example.com --organization "Acme Inc"
forge seed
forge db setup                     # create + migrate + seed
forge db prepare                   # setup if the database is missing, else migrate (safe to repeat)
forge db reset                     # drop + setup: a clean development database
forge db status
forge db drop
FORGE_ALLOW_DESTRUCTIVE=true forge db drop --force-production --yes
```

### How they run

Each command builds the application once with `cargo build --locked
--message-format=json-render-diagnostics`, finds the binary Cargo produced in
its messages and runs it as a direct child with your terminal, working
directory and environment. Cargo's diagnostics are printed as usual; the
application's output is yours. Nothing is reimplemented in the CLI: `migrate`
is `<app> migrate`, `db create` is `<app> db create`, and so on, so development
and production use one implementation. A failing application command makes
`forge` exit with status 1 and `application command "migrate" failed with
status 78` (the application's own code is in the message). Ctrl-C and SIGTERM
reach the application (SIGTERM, then a kill after 35 seconds, or at once on a
second Ctrl-C); a build is stopped with its whole process group, and nothing the
CLI started outlives it.

| `forge` | Runs | Notes |
| --- | --- | --- |
| `migrate` | `<app> migrate` | Needs `FORGE_MIGRATION_DATABASE_URL`. |
| `migrate status` | `<app> migrate status` | One line per migration, `state version_name`: `applied`, `pending`, `modified` (file changed after it was applied; `migrate` refuses), `disabled` (a Forge feature you no longer compose) or `missing` (unknown to the application; `migrate` fails closed). Only reads: without the bookkeeping table nothing is applied (the output starts with `no migrations applied`) and the table is not created. |
| `rollback [--steps N]` | `<app> rollback [--steps N]` | N from 1 to 1000, default 1. Forge-owned migrations are never reverted ([ADR 0030](adr/0030-framework-migration-ownership.md)). Not guarded, like Forge-Go; `db rollback` is. |
| `seed` | `<app> seed` | One transaction, idempotent. |
| `setup --tenant SLUG --email EMAIL [--organization NAME] [--password-stdin]` | `<app> setup ...` | The password is prompted for with echo off (standard input stays your terminal), piped with `--password-stdin` or in `FORGE_SETUP_PASSWORD`; never an argument. |
| `db create` | `<app> db create` | Prints `created ...` or `... already exists`. Uses `FORGE_MIGRATION_DATABASE_URL`, whose role must be allowed to create databases (PostgreSQL `CREATEDB`; the Compose `app_migrator` is). |
| `db drop` | `<app> db drop` | Guarded, below. Prints `dropped ...` or `... does not exist`. |
| `db reset` | `db drop`, then `db setup` | Guarded once, before the drop. Against the Compose container it does not re-run `docker/postgres/init.sql`'s per-database statements (`REVOKE CONNECT ... FROM PUBLIC`, `GRANT CONNECT`, `ALTER DEFAULT PRIVILEGES ... TO app_runtime`): the generated migrations grant the runtime role what it needs, but the new database lets `PUBLIC` connect and gives tables you create by hand no default privileges; `docker compose down -v && docker compose up -d db` restores the original. |
| `db setup` | `db create`, `migrate`, `seed` | Stops at the first failure. |
| `db prepare` | `db exists`; then `migrate`, or `db setup` when it is missing | `db exists` exits 3 for "missing", so a failure is never mistaken for it. |
| `db migrate`, `db status`, `db seed` | as `migrate`, `migrate status`, `seed` | |
| `db rollback [--steps N]` | `<app> rollback [--steps N]` | Guarded like `drop`. |

The application's `db` subcommand is for scripts and containers too:
`<app> db create|exists|drop [--force-production]`. `db exists` exits 0 when the
database exists and 3 when it does not. A URL with an unescaped `/`, `?`, `#` or `@` in the password is refused as malformed (percent-encode them), and the error never shows it. Database names are limited to 1 to 63
letters, digits, `_`, `-` and `$`, and the engines' system databases
(`postgres`, `template0`, `template1`, `mysql`, `information_schema`,
`performance_schema`, `sys`) are never created or dropped. PostgreSQL drops
terminate other sessions first (a running development server cannot block
them). SQLite creates and removes the file.

### Destructive commands

`db drop`, `db reset` and `db rollback` are refused when `FORGE_ENV` is not
`development` or `test` (an unset `FORGE_ENV` is `development`; a value
the framework would reject counts as protected):

```text
error: refusing to drop the database of "shop": FORGE_ENV is production; pass --force-production, and confirm
```

Nothing has been built or run when this happens. With `--force-production` the
CLI asks for confirmation: a person types the application name at a terminal, or
`--yes` is given **and** `FORGE_ALLOW_DESTRUCTIVE=true` is set in the process
environment (a value other than `true` is refused; `--yes` alone is a usage
error). The CLI removes `FORGE_ALLOW_DESTRUCTIVE` from the environment it gives
the application, which would refuse the unknown key. Because only the application
sees the database URL, `<app> db drop` enforces the second half of the rule
itself: it refuses `FORGE_ENV=production` and a server that is not on this machine
(not loopback, `localhost` or a socket path starting with `/`, judged on what the
driver would connect to after the `host`, `hostaddr`, `port`, `dbname` and
`socket` query parameters) unless `--force-production` is
passed, whether it is started by `forge` or directly in a container. The CLI
passes `--force-production` on only after confirmation, and asks for it whenever
you give the flag, even in development, because it cannot see the address.

### `forge doctor`

Checks, prints and exits 1 when something is wrong:

```text
configuration: ok
environment: development
http address: 127.0.0.1:3000
database: postgres database "shop" on 127.0.0.1:5432
database reachability: the server accepts connections
toolchain: cargo 1.98.1 (...); rustc 1.98.1 (...)
```

- **Toolchain**: `cargo` and `rustc` run.
- **Configuration**: the process environment passes the same loader the
  application uses (`FORGE_*` keys are known, values valid). The message names
  the key; secret values never appear, and `--json` omits values altogether.
- **Database**: the URL is described (engine, database, host and port, never a
  user, password or query) and a TCP connection to the host is opened (3
  seconds). The CLI does not log in; the application does that with its own
  credentials, so a wrong password still shows up only when you migrate or
  serve. SQLite reports whether the file exists. A Unix socket is not checked.
  `FORGE_MIGRATION_DATABASE_URL` missing is mentioned, not an error.

`--json` prints the report Forge-Go prints: `schema_version`, `ok`,
`environment`, `database` (`postgres`, `mysql`, `sqlite` or `none`) and
`diagnostics` with `code`, `severity`, `summary` and `remediation`
(`configuration.invalid`, `database.unreachable`, `toolchain.cargo_missing`,
`toolchain.rustc_missing`). It does not include the toolchain versions.

### `forge dev`

Builds and runs the application, then follows changes:

- A polling watcher (standard library, on the supervising thread) scans every
  `--poll-interval` (default 500ms, 50ms to 1m) and reports one batch once the
  tree has been quiet for `--debounce` (default 300ms, 0 to 1m). Durations use Go
  syntax (`500ms`, `1s`, `1m30s`).
- It watches Rust sources and `build.rs`, `Cargo.toml`, `Cargo.lock`,
  `rust-toolchain.toml`, templates, assets and migrations (`.rs`, `.html`,
  `.css`, `.js`, `.mjs`, `.json`, `.sql`, `.txt`, images and fonts). It never
  enters dot-directories, `target`, `tmp`, `vendor`, `node_modules`, `storage`,
  `log`, `logs`, `coverage` or `dist`, nor, at the root, `tests`, `benches`,
  `examples`, `docs` or `docker`, and refuses trees beyond 50 000 files.
- **The old process keeps serving until a build succeeds.** Then it gets
  SIGTERM, is awaited (35 seconds, then killed) and the new binary starts. A
  failed build prints Cargo's diagnostics and leaves the running application
  alone; if the tree changes while a build runs, that build is discarded and
  the build starts over. A crashed application waits for the next change.
- The application and `cargo build` each run in their own process group, which
  is signalled as a whole, so helpers they started do not outlive a restart or
  Ctrl-C. Progress lines start with `forge dev:` on standard error.
- `--no-reload` builds and runs once, forwarding signals. `NAME` selects one
  binary when the package has several.

The environment is yours (`FORGE_ENV` defaults to `development` in the
application); `forge dev` does not read `.env` files.

### Differences from Forge-Go

Same command names, flags, messages and exit codes, except:

- `forge rollback --steps N`: Rust CLIs spell long flags with two dashes
  (Go's `flag` package accepts `-steps` and `--steps`).
- `forge db create|drop` and `exists` run in the application, not the CLI, so
  the CLI never holds a database credential. They use
  `FORGE_MIGRATION_DATABASE_URL` for both the credentials and the database
  name; Forge-Go takes the name from `FORGE_DATABASE_URL` and the credentials
  from the privileged URL.
- `forge dev` has no browser-reload proxy, and does not read `.env.development`
  or `.env.local` (generated Rust applications have no env files).
- Not ported: `forge routes` (Rust routes are registered by code and
  composition hooks at run time, so a source scan would be wrong and an
  accurate listing needs the application's composition, which needs its
  configuration), `forge inspect`, `db schema:dump`, `db schema:load` and
  `db console` (they run `pg_dump`, `psql` and friends with the database
  credential), `destroy`, `deploy`, `logs`, `app`, `credentials`. They are
  tracked in [Rails parity](rails-parity.md).
- `forge doctor` also checks the toolchain and that the database server answers.
- Progress lines of `forge dev` carry no time stamp.

## Generated application binary

Every application built with `forge::App` accepts these subcommands:

| Command | Behavior |
| --- | --- |
| `serve` (default) | Loads and validates `FORGE_*` configuration, starts the HTTP server, and shuts down gracefully on SIGTERM/SIGINT. |
| `healthcheck` | Sends `GET /health/live` to the loopback address on the configured port. Exits `0` if healthy, `1` otherwise. |
| `migrate` | For database-enabled apps, applies all pending embedded migrations (with `App::jobs`/`App::events`, the framework job and outbox schemas first). |
| `migrate status` | Lists every migration as `applied`, `pending`, `modified`, `disabled` or `missing`; applies nothing. |
| `rollback [--steps N]` | For database-enabled apps, reverts the latest reversible migration (N of them with `--steps`, 1 to 1000). |
| `db create` / `db exists` / `db drop [--force-production]` | Database-enabled apps, with `FORGE_MIGRATION_DATABASE_URL`: creates the database unless it exists, exits 0 when it exists and 3 when it does not, or drops it (refused when `FORGE_ENV` is not development or test, or the server is not on this machine, unless `--force-production`). |
| `setup --tenant SLUG --email EMAIL [--organization NAME] [--password-stdin]` | Full-stack PostgreSQL apps: creates the first tenant and its owner (see below). |
| `seed` | Database apps: runs `seed` in `src/bootstrap/seeds.rs` in one transaction, committed only on success. |
| `jobs [work]` | With `App::jobs` (feature `jobs`): runs job workers, cron schedules and the outbox dispatcher without HTTP until `SIGTERM`/`SIGINT`. |
| `jobs status` | Prints job counts per queue and state and the oldest due time. |
| `jobs dead [LIMIT]` | Lists dead jobs, newest first (default 50, at most 500); payloads are never printed. |
| `jobs retry JOB_ID` / `jobs discard JOB_ID` | Requeues a dead job with a fresh attempt budget, or deletes it. |
| `jobs dead-events [LIMIT]` / `jobs retry-event EVENT_ID` | With `App::events`: lists or requeues dead outbox events. |
| `version` | Prints the application version. |
| `help` | Prints usage, including the application's commands. |

`setup` and `seed` are application commands registered with
`forge::App::command(name, summary, handler)`. Application commands parse
`--name value` options strictly, never accept a password argument, and,
when the application declared a database, run with a pool connected through
`FORGE_MIGRATION_DATABASE_URL`: they create accounts and seed data that the
least-privilege runtime role must not be able to write. A usage error exits
with code `2`, any other failure with `1`.

```sh
export FORGE_MIGRATION_DATABASE_URL='postgres://app_migrator:app_migrator@127.0.0.1:5432/shop'
forge setup --tenant acme --email owner@example.com --organization "Acme Inc"
printf '%s\n' "$OWNER_PASSWORD" | forge setup --tenant acme --email owner@example.com --password-stdin
forge seed
# the same through Cargo:
cargo run -- setup --tenant acme --email owner@example.com --organization "Acme Inc"
printf '%s\n' "$OWNER_PASSWORD" | cargo run -- setup --tenant acme --email owner@example.com --password-stdin
cargo run -- seed
docker compose run --rm app setup --tenant acme --email owner@example.com --password-stdin < password.txt
```

`setup` creates the tenant (`forge_tenants`, slug `^[a-z][a-z0-9-]{1,62}$`,
display name from `--organization` or the slug), the owner principal and an
active membership with the `owner` role, plus an
`auth.setup.owner_provisioned` audit event, in one transaction under a
PostgreSQL advisory lock. It refuses once any tenant exists, so running it
again (or twice concurrently) creates nothing more and it can never add an
account to, or take over, a running installation. The password is typed
twice at a terminal with echo off, piped as one line with
`--password-stdin`, or read from `FORGE_SETUP_PASSWORD`; it is never an
argument, never printed or logged, and must have at least 12 characters.
`setup` prints the tenant id: tenant-scoped scaffold pages live under
`/tenants/<tenant id>/`.

`seed` is like `rails db:seed`: write idempotent statements (`INSERT ... ON
CONFLICT DO NOTHING`) so seeding twice changes nothing. Seeds are not for
accounts or credentials.

During development `forge migrate`, `forge rollback`, `forge setup` and
`forge seed` run these commands for you (see
[Application lifecycle commands](#application-lifecycle-commands)); in a
container or a release call the binary directly.

`healthcheck` exists because the distroless runtime image has no shell or
`curl`. The Dockerfile's exec-form `HEALTHCHECK` runs `<binary> healthcheck`.
Configuration variables are listed in
[deployment](deployment.md#configuration).

See also [ADR 0009](adr/0009-rails-style-generated-delivery-assets.md).
