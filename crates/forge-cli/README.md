# Forge CLI

`forge` is the command-line entry point for Forge applications.

```text
forge id                         print a UUIDv7
forge new <name> [flags]         create an application
forge generate migration <name>  create a reversible migration pair
forge generate scaffold <name> <field:type>... --global|--tenant-scoped [--plural P] [--dry-run]
                                 HTML CRUD through every ring, with tests
forge check | test [ARGS] | format | lint | build [NAME]
forge version                    print the CLI version
forge doctor [--json]            toolchain, FORGE_* settings, database reachability
forge dev [NAME] [--no-reload] [--poll-interval D] [--debounce D]
                                 run; rebuild and restart gracefully on change
forge migrate [status] | rollback [--steps N] | seed
forge setup --tenant SLUG --email EMAIL [--organization NAME] [--password-stdin]
forge db create | drop | reset | setup | prepare | migrate | rollback | status | seed
                                 database lifecycle, like Rails' db:* tasks
forge openapi [--output FILE | --check FILE]
                                 the application's OpenAPI 3.1 document (ADR 0039)
```

`forge new` creates a standalone Cargo package with Forge's hexagonal module
boundaries, an architecture test, server-rendered views (layout, templates,
digested assets, Turbo and Stimulus, CSRF-protected forms; `--api` for JSON
only) and, by default, the delivery assets a production service needs:

- `Dockerfile` (multi-stage, locked build, distroless non-root runtime,
  self-probing `HEALTHCHECK`), `.dockerignore`, `compose.yaml` and the opt-in
  local observability stack (`compose.observability.yaml`, `observability/`);
- `.github/workflows/ci.yml` (fmt, clippy, tests, image build and smoke test)
  and `.github/dependabot.yml`.

Flags: `-d <DATABASE>`, `--api`, `--skip-docker`, `--skip-ci`,
`--skip-lockfile`, and `--forge-path <PATH>` to depend on a local Forge
checkout (`crates/forge`) instead of the release tag.
It refuses to overwrite an existing path. Templates live in `templates/app`;
`examples/hello-forge` is their committed output and a unit test fails when
the two drift (`FORGE_BLESS=1 cargo test -p forge-cli` regenerates it).

`forge generate scaffold` renders `templates/scaffold` for the application's
backend, formats the Rust with `rustfmt`, writes only new files (all or
nothing) and prints the module and `compose` lines to add; its golden output
is under `tests/fixtures/scaffold` and `tests/scaffold.rs` (ignored, slow)
builds scaffolded applications end to end.

The quality commands run Cargo directly in the current directory; no shell is
involved. `dev`, `migrate`, `rollback`, `setup`, `seed` and `db` build the
application with Cargo and run its own subcommands (`<app> migrate`, `<app> db
create`, ...), so this crate holds no database code and reads no database URL;
`db drop`, `db reset` and `db rollback` are refused when `FORGE_ENV` is not
`development` or `test` ([ADR 0036](../../docs/adr/0036-application-lifecycle-commands.md)).
Their end-to-end tests are in `tests/lifecycle.rs` (fake `cargo` and application)
and `tests/scaffold.rs` (a real PostgreSQL application). See
[docs/cli.md](../../docs/cli.md) for the full reference.
