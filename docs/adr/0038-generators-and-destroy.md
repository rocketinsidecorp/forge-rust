# ADR 0038: Model, controller, mailer and queue job generators, and `forge destroy`

> **Update (2026-10-26, D21, [forge-rust#93](https://github.com/rocketinsidecorp/forge-rust/issues/93)).**
> Generators no longer print wiring to paste for scaffold, resource, model,
> controller and view: they declare their modules and mount their routes in
> `src/bootstrap/routes_gen.rs`, and `forge destroy` takes both back out
> ([generators](../generators.md#the-routes-seam)). Mailer and job still
> print theirs, as Forge-Go's do. The text below is kept as decided.

- **Status:** Accepted
- **Date:** 2026-10-20
- **Port of:** Forge-Go ADR 0041 (rocketinsidecorp/forge-rust#15)

## Context

`forge generate` offered a migration, the sign-in pages and an HTML scaffold.
The parity matrix ([Rails parity](../rails-parity.md)) listed the rest of
`rails g` (model, controller, mailer, job) as missing, and nothing undid a
generation the way `rails d` does. Forge-Go shipped both in its ADR 0041,
with the same commands, flags and safety rules this port keeps.

## Decision

1. **One generator toolkit.** The new generators reuse the scaffold's field
   grammar, naming rules, rendering, `rustfmt` pass, all-or-nothing
   create-new writes (`OpenOptions::create_new`, removed again on failure),
   symlink refusal and printed wiring (`crates/forge-cli/src/generators.rs`).
   No generator edits an existing file: each prints the module declarations
   and the `fn compose` lines to add.
2. **A model is the scaffold minus its HTTP layer**, byte for byte: the
   migration, entity, repository port, use cases, SQLx repository adapter
   and repository tests, in the domain, application and infrastructure
   rings, for every backend, with UUIDv7 identifiers. A model and a scaffold
   of one plural collide (the `create_PLURAL` migration exists) instead of
   mixing layers.
3. **Controllers are read-only and guarded.** `forge generate controller
   NAME ACTION... [--api] [--public]` writes `GET` handlers (HTML pages with
   askama views, or JSON under `/v1/NAME` with `--api`), their routes and
   tests, in the HTTP adapter ring. `create`, `update` and `destroy` are
   refused with a pointer to the scaffold. Without `--public` every route asks
   the shared `GlobalAccess` policy (`src/adapters/http/access.rs`, written by
   the first scaffold or controller that needs it) for `NAME:read`, and the
   printed wiring passes `DenyAll` until the application decides who may read.
4. **Every permission has one owner.** In Forge-Rust a permission is the
   string constant a generated module declares and the bootstrap's
   `RbacPolicy` grants, so "registering" it means declaring that constant.
   The generators scan `src/**/*.rs` for the permission as a string literal
   (`"posts:read"`, hand-written code included):
   - `generate controller NAME` reuses `NAME:read` when
     `src/application/use_cases/NAME.rs` (a model or scaffold) declares it
     (`pub use ...::READ_PERMISSION`), and refuses when any other file does;
   - `generate model|scaffold` refuses when another file declares
     `PLURAL:read` or `PLURAL:write`.
5. **Mailers are typed and fail closed.** `forge generate mailer NAME
   METHOD[:field,...]...` writes `src/adapters/email/NAME_mailer.rs` (one data
   struct and one `build_METHOD`/`METHOD` pair per email over
   `forge::mail::Templates`), askama templates in
   `templates/mailers/NAME/` (`.subject.txt`, `.txt` and `.html`, so the HTML
   part is escaped) and tests over `forge::mail::testing::Recorder`. Without
   `FORGE_MAIL_URL` the mailer is `unconfigured`: every send fails with
   `MailError::NotConfigured`. Previews are served at `/_mail/NAME` only with
   `FORGE_MAIL_PREVIEW=true` in development or test. To let that handler sit
   next to the password-reset previews at `/_mail`, `PreviewHandler` now
   passes a path whose first segment names none of its previews to the next
   layer (it used to answer `404`), so nested preview handlers work in any
   order.
6. **Jobs need `--queue`.** `forge generate job NAME [field:type...]
   --queue` writes a `forge::jobs::Job` payload, its `JobHandler`,
   `register`, `enqueue` (in the caller's transaction) and a `perform` that
   fails permanently until written, under `src/adapters/jobs/` (whose
   `mod.rs` the first job writes), plus tests that need no database. Jobs are
   PostgreSQL-only (ADR 0025). Without `--queue` the command is refused: the
   Rust scheduler runs queue jobs (`Schedule::new`), so there is no separate
   in-process stub to generate.
7. **`forge destroy` regenerates instead of keeping a manifest.** `forge
   destroy model|scaffold|controller|mailer|job ARGUMENTS... [--dry-run]
   [--force] [--migrations]` takes the generator's arguments, renders what it
   would write (the migrations at the versions found in `migrations/`) and
   removes a file only while it is byte-identical. Otherwise:
   - a file that differs is listed, nothing is removed and the command
     fails, unless `--force`;
   - a changed locale file of a locale other than English (a translation of
     the scaffold's text) is kept and reported as `keep`, not refused;
   - migrations are kept unless `--migrations` (run `forge rollback` first);
     a destroy that finds only kept files says so;
   - shared files (the access policy, the live-update transport, the jobs
     module) are never planned for removal;
   - files a bigger or dependent generation needs are refused: `destroy
     model` while the scaffold's pages exist, and `destroy model|scaffold`
     while a controller reuses its permission; `--force` overrides the
     modification and ownership checks, nothing else;
   - nothing is removed through a symlink or that is not a regular file;
     directories that become empty are removed, except the application's
     layout directories.
   `forge destroy migration` explains why migrations are not destroyed.

## Consequences

- `forge destroy` cannot find what it cannot regenerate: with other
  arguments, the files differ and nothing is removed.
- Removing an empty directory does not know whether it existed before the
  generator ran; the layout directories `forge new` creates are always kept.
  Destroy compares, then removes: do not run it while another process writes
  the same files.
- Generated code is ordinary Rust, compiled and linted with the application:
  every generator is covered by tests that generate into a fresh PostgreSQL
  application, apply the printed wiring, run `cargo fmt --check`, Clippy with
  warnings denied and `cargo test`, then destroy everything and check the
  application is the one `forge new` wrote and still builds.

### Deviations from Forge-Go

- HTML controllers need a full-stack application (they render pages, and
  are wired in `fn compose`); `--api` controllers also work in a
  `forge new --api` application, whose `fn compose` mounts them with
  `DenyAll`. Guarded controllers need a database application (the access
  policy reads the session stores). `--public` works without a database.
- A guarded controller declares (or reuses) a constant instead of writing a
  permission migration, because Forge-Rust permissions live in code
  (`RbacPolicy`), not in a `forge_permissions` table; there is therefore no
  controller rollback to restrict.
- `generate job` without `--queue` is refused (decision 6), and jobs are
  PostgreSQL-only, as `forge::jobs` is.
- Mailer previews are in the mailer module (`previews()`), not a separate
  file; the mailer starts unconfigured without a start-up warning (the
  printed wiring and `is_configured()` say so).
- A model in a `forge new --api` application is refused until that template
  adds the `forge` dev-dependency with `testing` the repository tests need.

## Follow-ups

- `forge routes` (planned with the lifecycle follow-ups) to list the
  controller routes.
- `generate model` in `--api` applications and `generate controller --api`
  in them, once `forge new --api` has a composition hook and test
  dependencies (rocketinsidecorp/forge-rust#15 follow-up).
