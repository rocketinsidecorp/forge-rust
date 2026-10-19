# Forge CLI

`forge` is the command-line entry point for Forge applications.

```text
forge id                         print a UUIDv7
forge new <name> [flags]         create an application
forge generate migration <name>  create a reversible migration pair
forge generate scaffold <name> <field:type>... --global|--tenant-scoped [--plural P] [--dry-run]
                                 HTML CRUD through every ring, with tests
forge check | test | format | lint | build
```

`forge new` creates a standalone Cargo package with Forge's hexagonal module
boundaries, an architecture test, server-rendered views (layout, templates,
digested assets, Turbo and Stimulus, CSRF-protected forms; `--api` for JSON
only) and, by default, the delivery assets a production service needs:

- `Dockerfile` (multi-stage, locked build, distroless non-root runtime,
  self-probing `HEALTHCHECK`), `.dockerignore` and `compose.yaml`;
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
involved. See [docs/cli.md](../../docs/cli.md) for the full reference.
