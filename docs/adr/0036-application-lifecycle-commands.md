# ADR 0036: Application lifecycle commands

- Status: Accepted
- Date: 2026-10-20
- Refines: [ADR 0021](0021-selectable-database-backends.md) (a backend's lifecycle is now driven from the CLI), [ADR 0030](0030-framework-migration-ownership.md) (`rollback --steps`, `migrate status`)
- Ported from: Forge-Go ADR 0035 (development loop and database lifecycle): same command names, flags, messages and refusals, with the differences listed below

## Context

Rails' developer loop is `bin/dev` and `rails db:create|drop|reset|setup|prepare`.
Forge-Go's CLI has `forge dev`, `forge migrate|rollback|setup|seed`, `forge db
...`, `forge doctor` and `forge version`; the owner's parity rule (2026-10-20)
says every user-facing capability exists in both frameworks with the same
commands, names and style. The Rust CLI had only `id`, `new` and `generate`
(and Cargo wrappers), while the generated README already told people to run
`forge migrate`.

Three constraints shape the port:

- The CLI must not hold database credentials. Forge-Go's CLI links `pgx`, the
  MySQL driver and runs `pg_dump`/`psql` with credentials it parses from the
  URL. In Rust the application already links SQLx, owns the migrator and runs
  with the migration credential, so the CLI should only start it.
- Destructive commands are the most dangerous thing a CLI can do.
- A process the CLI starts must not outlive it, and a restart loop must not
  wreck a running application when the code is momentarily broken.

## Decision

### The CLI runs the application's own subcommands

`forge migrate`, `rollback`, `setup`, `seed` and every `forge db` command run
`<app> migrate`, `<app> db create`, ... The CLI builds the package once per
invocation with `cargo build --locked --message-format=json-render-diagnostics`
(Cargo's rendered diagnostics still reach the terminal), reads the binary's path
from the `compiler-artifact` messages and runs that binary as a direct child
with the caller's terminal, working directory and environment. It is not
`cargo run`: one build per invocation however many commands `db setup` chains,
no Cargo process between the CLI and the application (signals and exit codes
reach it unchanged), and no guess at the target directory.

The CLI never reads the database URL for these commands. Only the application
sees credentials, as it does in production. The one setting that is the CLI's
own, `FORGE_ALLOW_DESTRUCTIVE`, is removed from the application's environment
(the application rejects unknown `FORGE_*` keys at startup).

A directory is an application when its `Cargo.toml` has a `[package]` name and a
`forge` dependency; any other directory is refused before anything is built, so
`forge db drop` can never build and run an unrelated project.

### New application subcommands (`forge` crate)

- `migrate status`: one line per migration, `applied`, `pending`, `modified`
  (checksum differs: `migrate` refuses), `disabled` (a Forge feature no longer
  composed, tolerated by ADR 0030) or `missing` (unknown, fail-closed). Framework
  schemas with their own bookkeeping table (jobs, outbox) are listed too.
  `Database::migration_status` only reads: when the bookkeeping table is absent
  (PostgreSQL `42P01`, MySQL `42S02`, SQLite "no such table") nothing is applied,
  the table is not created and the output starts with `no migrations applied`.
- `rollback --steps N` (1 to 1000): reverts N application migrations, newest
  first, stopping at the first failure or when none is left. Framework
  migrations are never reverted, as before.
- `db create|exists|drop [--force-production]` with `FORGE_MIGRATION_DATABASE_URL`,
  on top of SQLx's `MigrateDatabase` (maintenance database, same TLS rules) in
  `forge_db::{database_exists, create_database, drop_database}`. Names are
  limited to `[A-Za-z0-9_$-]{1,63}` not starting with `$` or `-`, and the
  engines' system databases are refused, so no name needs quoting or can reach
  another database. PostgreSQL drops use SQLx's forced drop (other sessions
  terminated). `db exists` exits 0 or 3, so `forge db prepare` can tell "missing"
  from a failure without parsing output.
- What a URL names is decided on what the **driver** reads, never on the URL's
  text. `forge_db::target::<B>(url)` parses with SQLx's own options parser
  (`PgConnectOptions`, `MySqlConnectOptions`), so the `host`, `hostaddr`, `port`
  and `dbname` query parameters of PostgreSQL, the `socket` parameter of MySQL
  and the `PG*` environment defaults count. The name check (charset, system
  databases) and the locality guard (`FORGE_ENV` aside, only a loopback host or
  a socket path starting with `/` is local; a socket wins over a host, as it does
  when connecting) run on those effective values, so
  `postgres://m:pw@localhost/shop?host=prod` is a remote server and
  `?dbname=postgres` is refused. A URL is **well formed** only when the `url`
  crate parses it and no `@` follows its authority: an unescaped `/`, `?` or `#`
  in a password ends the authority early and would leave part of the password
  in a "host" or a database name, so such a URL is refused, with a message that
  tells the user to percent-encode and never shows the URL.
  `forge_config::database_url` (`describe`, `endpoint`, `is_well_formed`,
  `validate_name`) applies the same rules for `forge doctor`, which has no
  driver (it ignores `PG*` environment defaults).

`db` joins the built-in command names: an application that registered its own
`db` command must rename it, because it now fails like any other built-in name.

### Protection of destructive commands

`db drop`, `db reset` and `db rollback` (not the top-level `rollback`, as in
Forge-Go) are refused unless `FORGE_ENV` is `development` or `test`. The rule is
enforced twice, because each side sees only half of it:

- the CLI sees `FORGE_ENV` and refuses production before it builds or runs
  anything: `refusing to drop the database of "shop": FORGE_ENV is production;
  pass --force-production, and confirm`. An unset `FORGE_ENV` is development;
  a value the framework would reject counts as protected.
- the application sees the URL and refuses `FORGE_ENV=production` and a server
  that is not on this machine (loopback, `localhost`, a Unix socket) unless
  `--force-production` is passed, so `<app> db drop` in a container is guarded
  too.

`--force-production` makes the CLI ask for confirmation, always (the CLI cannot
tell that a development database is remote): a person types the application
name at a terminal (Forge-Go types the database name, which the CLI would have to
read from the URL), or `--yes` with `FORGE_ALLOW_DESTRUCTIVE=true` in the
process environment. `--yes` without `--force-production` is a usage error. The
CLI passes `--force-production` to the application only after confirmation.
`db reset` confirms once, before the drop.

The development Compose bootstrap gives `app_migrator` `CREATEDB`; `app_runtime`
keeps `NOCREATEDB`. Without it `db reset` would drop the database and then fail
to create it, leaving a container that only `docker compose down -v` repairs.
A database made by `db reset` is not `init.sql`'s: the script's per-database
statements (`REVOKE CONNECT ... FROM PUBLIC`, `GRANT CONNECT`, `ALTER DEFAULT
PRIVILEGES ... TO app_runtime`) are not run again. Generated migrations grant
the runtime role what it needs explicitly, so the application works, but
`PUBLIC` can connect to the new database and tables you create by hand get no
default privileges for `app_runtime`; to restore them run `docker compose down
-v && docker compose up -d db`, or repeat those statements in `psql`.
The role is development-only and creates only databases it then owns;
[deployment](../deployment.md) still requires the runtime role to be unable to.

### `forge dev`

A polling supervisor, standard library only: it scans the source tree every
`--poll-interval` (500ms default), reports a batch of changes after the tree has
been quiet for `--debounce` (300ms), builds with Cargo and restarts. The old
process keeps serving until a build succeeds; a failed build prints Cargo's
diagnostics and leaves it running; a build overtaken by an edit is discarded;
a crashed application waits for the next change instead of restarting in a loop.
Watched files are Rust sources, `build.rs`, `Cargo.toml`/`Cargo.lock`,
templates, assets and migrations; build output, hidden, runtime and (at the
root) test directories are skipped, and a tree beyond 50 000 files is refused.

Process ownership: the application and the build each run in their own process
group (Unix `process_group(0)`), so a stop reaches rustc and the application's
helpers; stopping is SIGTERM to the group, a 35-second grace (longer than the
framework's 15-second default shutdown), then SIGKILL, at once on a second
Ctrl-C. Dropping an owned child kills and reaps it. One signal thread (a Tokio
current-thread runtime that only sets a flag, stopped and joined when the
command ends) installs the SIGINT/SIGTERM handlers; the thread that owns a
child polls the flag. Nothing is detached. Lifecycle commands that may prompt
for a password (`setup`) run the application in the CLI's own group so it can
read the terminal. Signals are sent with `rustix` (no `unsafe`, no shell).

Not ported from Forge-Go: the browser-reload proxy (a reviewed Rust port with
the same CSP rules is a follow-up) and `.env.development`/`.env.local`
(generated Rust applications have no env files). The restart replaces the binary
Cargo wrote in `target/`, which Unix allows while it runs; on Windows the build
of a running application fails until it is stopped, so `forge dev` there needs
a copy per restart (follow-up).

### `forge doctor`

Toolchain (`cargo`, `rustc`), configuration (the application's own
`AppConfig::from_vars`, so unknown keys and bad values are found the same
way), and, when `FORGE_DATABASE_URL` is set, the redacted description of the
database and whether its host accepts a TCP connection within 3 seconds. The
CLI does not log in. `--json` is Forge-Go's versioned report. Exit status 1 when
anything is wrong; `FORGE_ALLOW_DESTRUCTIVE` is not treated as an unknown key.

### Not ported, and why

- `forge inspect`: Forge-Go scans the source for literal `Handle` calls. Rust
  routes are registered by closures, helper functions and composition hooks
  with computed paths (`format!("{collection}/new")`), so a scan would be wrong.
  An accurate listing needs the application's composition and therefore its
  configuration. `forge routes` is now an application subcommand
  (`Router::routes()` plus a built-in `routes`, which composes the application
  without listening, like `openapi`); `inspect` is tracked in
  [Rails parity](../rails-parity.md#parity-follow-ups).
- `db schema:dump|schema:load`: they run `pg_dump`, `psql` and friends with the
  credential. They need application subcommands that pass it through the
  child's environment; tracked with the above. `db console` was ported later
  (2026-10-25, [CLI](../cli.md#application-lifecycle-commands)): the CLI runs
  the client itself, with the URL's credentials in the child's environment or
  a private option file, never in an argument.
- `destroy`, `deploy`, `logs`, `app`, `credentials`, `i18n`: outside this change.

### Dependencies

No new package in `Cargo.lock`. `forge-cli` depends on workspace crates and
entries it did not use before: `forge-config` (configuration and URL
redaction, which now reads URLs with the `url` crate SQLx already locks), `serde_json` (Cargo's messages, `doctor --json`), `tokio` (the
signal thread; features already unified in the workspace) and `rustix` with the
`process` feature (signals to a process group; the entry's features were
`fs` and `std`). See [dependency policy](../dependency-policy.md#cli-lifecycle-dependencies).

## Consequences

- Development and production share one migration, seed and database
  implementation; the CLI is a thin, credential-free driver.
- A framework change (the new application subcommands) is needed before
  generated applications can use the commands: applications generated with the
  default `GitTag` source get them with the next release tag; `--forge-path`
  applications now.
- `forge db create|drop` need `CREATEDB` on the migration role; the Compose
  role has it, a managed database's migration role usually does not, and then
  `create`/`drop`/`reset` are for local databases only.
- `forge dev` has no browser reload and no `.env` handling until the follow-ups.
- Each command that builds pays a Cargo no-op check (about 100 ms when current).

## Verification

`forge-cli` unit tests drive the lifecycle with a fake application (order of
calls, stopping at the first failure, every production refusal, confirmation
and `--yes` rules, `FORGE_ALLOW_DESTRUCTIVE` stripping), the dev supervisor with
a fake host (replace after a successful build, keep the old process on a failed
or stale build), the watcher rules and `doctor` with fake probes (secrets never
printed). `tests/lifecycle.rs` runs the real binary with a fake `cargo` and a
fake application, including SIGTERM/SIGINT while `forge dev` runs and the
production refusals before anything is built. `tests/scaffold.rs` generates a
PostgreSQL application and runs `doctor`, `db prepare|status|reset|drop`,
`migrate`, `rollback` and `setup` against a real server, with the CLI creating
and dropping the database. The `forge` crate tests `migrate status`, `rollback
--steps` and `db` with SQLite, including the production guard.
