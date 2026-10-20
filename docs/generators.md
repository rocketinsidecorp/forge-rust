# Generators

`forge generate` writes tested, reviewable code the way `rails g` does, and
`forge destroy` takes it away again. The commands, arguments and rules are
Forge-Go's ([ADR 0038](adr/0038-generators-and-destroy.md), the port of
Forge-Go ADR 0041). Every generator follows the same rules:

- **Plain code.** What is generated is ordinary Rust in the hexagonal rings
  (domain, application, adapters, infrastructure), formatted with the
  application's `rustfmt` and clean under `clippy -D warnings`; no macros to
  reverse-engineer.
- **Never overwrite.** A generator refuses to write over any file or through
  a symlink, and writes nothing at all when it refuses (files are created
  with `create_new`, and removed again if a later write fails). `--dry-run`
  prints the exact files without creating anything.
- **Never edit your code.** `src/bootstrap/mod.rs` and the `mod.rs` files are
  yours: a generator prints the `pub mod` lines and the statements to add,
  and says where.
- **Never public by accident.** Anything that serves data asks an access
  policy for a permission, and the printed wiring denies everyone
  (`DenyAll`) until the application decides.
- **Validated names.** Names and fields become Rust identifiers, file names,
  SQL identifiers and URL segments, so they are strict lowercase
  `snake_case` (letters, digits, single underscores, starting with a letter,
  at most 40 characters) and never a Rust keyword.

| Command | Creates | Rails |
| ------- | ------- | ----- |
| `forge generate migration NAME` | an empty up/down migration pair | `rails g migration` |
| `forge generate model NAME field:type...` | migration, entity, repository port, use cases, SQLx repository, tests | `rails g model` |
| `forge generate controller NAME ACTION...` | HTTP handlers, routes, views, tests; no persistence | `rails g controller` |
| `forge generate mailer NAME METHOD...` | a mailer with subject, text and HTML templates, previews, tests | `rails g mailer` |
| `forge generate job NAME [field:type...] --queue` | a typed job of the persistent queue, its handler, registration, enqueue function, tests | `rails g job` |
| `forge generate scaffold` | a model plus HTML CRUD pages ([cli](cli.md#forge-generate-scaffold)) | `rails g scaffold` |
| `forge generate authentication` | the sign-in pages | `rails g authentication` |
| `forge destroy GENERATOR ARGUMENTS...` | removes what a generator created | `rails d` |

## Model

```sh
forge generate model post title:string 'body:text?' published:boolean --tenant-scoped --dry-run
forge generate model post title:string 'body:text?' published:boolean --tenant-scoped
forge migrate
forge test
```

Takes exactly the arguments of `forge generate scaffold` without
`--broadcast` (`--tenant-scoped` or `--global`, `--plural`, field types
`string`, `text`, `integer`, `boolean`, `timestamp`, `uuid`, a trailing `?`
for optional) and writes exactly the model files the scaffold writes, byte
for byte, without the HTTP layer:

```text
migrations/<version>_create_posts.{up,down}.sql
src/domain/entities/post.rs                 entity, validation, typed errors
src/application/ports/post_repository.rs    PostRepository port
src/application/use_cases/posts.rs          PostService, READ_PERMISSION, WRITE_PERMISSION
src/infrastructure/database/posts.rs        PgPostRepository (MySql..., Sqlite... per backend)
tests/posts_repository.rs                   against a real database
```

The printed wiring declares the modules and builds the service where you
need it:

```rust
let posts = crate::application::use_cases::posts::PostService::new(std::sync::Arc::new(
    crate::infrastructure::database::posts::PgPostRepository::new(composition.postgres()?.clone()),
));
```

Tenant-scoped models take the authorized `TenantContext` in every use case,
bind the tenant in every statement and, on PostgreSQL, run behind forced
row-level security. IDs are UUIDv7 generated in Rust. Adding the pages later
is `forge generate scaffold` on a new name: a model and a scaffold of one
plural collide instead of mixing layers. A `forge new --api` application is
refused until it has the `forge` dev-dependency with `testing` the repository
tests need.

## Controller

```sh
forge generate controller pages home about_us --public --dry-run
forge generate controller pages home about_us --public
forge generate controller reports monthly            # guarded: asks for reports:read
forge generate controller status_checks index --api  # JSON under /v1
```

`NAME` is usually plural; each `ACTION` is a read-only `GET` route
(`create`, `update` and `destroy` change data, belong to
`forge generate scaffold`, which also generates the model, the form and the
CSRF-protected routes they need, and are refused here).

| Action | HTML route | with `--api` |
| ------ | ---------- | ------------ |
| `index` | `GET /NAME` | `GET /v1/NAME` |
| any other, `about_us` | `GET /NAME/about_us` | `GET /v1/NAME/about_us` |

```text
src/adapters/http/NAME_controller.rs          handlers, routes(), READ_PERMISSION
src/adapters/http/views/NAME_controller.rs    view models (HTML only)
templates/NAME/ACTION.html                    one page per action (HTML only)
tests/NAME_controller_http.rs
src/adapters/http/access.rs                   shared access policy, when missing
```

The pages extend the application layout (CSRF meta tag, flash, translations)
and show a title and text you replace; `--api` handlers answer a small JSON
document (`NAMEBody`) and are described in the OpenAPI document by
`operation` (summary `NAME ACTION`, tag `NAME`, the permission and the access
policy's `503`; [OpenAPI](openapi.md)). Controllers are wired in `fn compose`, so they need a
full-stack application.

`routes` mounts every route behind a guard:

- Without `--public` it takes an `Arc<dyn GlobalAccess>` and asks it for
  `NAME:read` on every request. The printed wiring passes `DenyAll`;
  implement `GlobalAccess` (see `src/adapters/http/access.rs`) to decide who
  may read. Guarded controllers need a database application, whose session
  stores the access policy uses.
- `--public` mounts the routes with no guard and prints a warning. Use it for
  pages anyone may read; it also works without a database.

Generated tests check that every action answers, that only `GET` is routed,
that the guard is asked for `NAME:read` once per route, and that `DenyAll`
refuses every action. With `--api` every response in those tests is also
checked against the OpenAPI document (`CheckResponses`).

## Mailer

```sh
forge generate mailer user welcome:name,url password_reset --dry-run
forge generate mailer user welcome:name,url password_reset
forge test
```

`NAME` is the mailer (`user` becomes `UserMailer`); each `METHOD` is one
email, optionally followed by `:field,field`, the string fields of its data.

```text
src/adapters/email/user_mailer.rs                  UserMailer, UserWelcome, ..., previews()
templates/mailers/user/welcome.subject.txt         one line
templates/mailers/user/welcome.txt                 text/plain part (nothing escaped)
templates/mailers/user/welcome.html                text/html part (every value escaped)
tests/user_mailer.rs
```

It builds on [`forge::mail`](mail.md). Each email has typed data
(`UserWelcome { name, url }`), `build_welcome(to, &data)` that renders the
complete message (for previews and tests) and `welcome(to, &data)` that
renders, then sends. Addresses, subjects and bodies pass the validation of
every message, so a header injection is refused before a byte is sent.
`UserMailer::from_config` uses `FORGE_MAIL_URL` and `FORGE_MAIL_FROM`; without
a URL the mailer is unconfigured and every send fails with
`MailError::NotConfigured` instead of pretending to deliver.

The printed wiring builds the mailer in `fn compose` and, only with
`FORGE_MAIL_PREVIEW=true` in development or test, serves its previews at
`/_mail/NAME` (next to the password reset preview at `/_mail`):

```sh
FORGE_MAIL_URL=file:tmp/mail FORGE_MAIL_FROM='Shop <shop@example.com>' FORGE_MAIL_PREVIEW=true forge dev
# open http://127.0.0.1:3000/_mail/user
```

Generated tests deliver through `forge::mail::testing::Recorder` (the real
encoder), check the subject and both parts, that hostile data is escaped in
the HTML part, that a failing sender records nothing, that an unconfigured
mailer refuses to send, that a header-injecting recipient is refused, and
that previews render and exist only in development.

## Job

```sh
forge generate job send_welcome user_id:uuid 'note:string?' --queue --dry-run
forge generate job send_welcome user_id:uuid 'note:string?' --queue
forge test
```

`--queue` is required: Forge-Rust jobs run on the persistent PostgreSQL queue
([jobs](jobs.md)), and a recurring job is a queue job with a `Schedule`.

```text
src/adapters/jobs/send_welcome.rs   SendWelcome (payload, impl Job), SendWelcomeHandler,
                                    register, enqueue, perform
src/adapters/jobs/mod.rs            written by the first job (shared)
tests/send_welcome_job.rs
```

The payload struct is the job's schema, with the model field types (`id` and
`version` are allowed names here). The job is named `send_welcome`, runs on
the `default` queue with five attempts and a one-minute timeout.
`register(composition.jobs()?)` registers the handler; `enqueue(&mut
transaction, &job, options)` stores the job atomically with your data.
`perform` returns a permanent error until you write it, so a placeholder can
never report success: the job goes straight to `dead`, visible in
`cargo run -- jobs dead`. Like Forge-Go's generator, it does not edit the
bootstrap: the printed wiring puts the `register` line at the end of
`jobs_configure` in `src/bootstrap/mod.rs`, which `forge new` generates for
PostgreSQL applications together with the queue declaration
(`.jobs(forge::WorkerMode::InServe)`) and the `jobs` feature of `forge`.
For an application generated before that, the wiring names `fn compose` and
also prints the declaration and the feature. The tests need no database.

## Permission ownership

A Forge-Rust permission is a string constant a generated module declares
(`READ_PERMISSION = "posts:read"`) and the bootstrap's `RbacPolicy` grants.
Each permission has **exactly one owner**, and the generators never give it a
second one:

```sh
forge generate model widget name:string --global
forge generate controller widgets dashboard   # reuses widgets:read: pub use ...::use_cases::widgets::READ_PERMISSION
```

```sh
forge generate controller reports monthly     # declares reports:read
forge generate model report title:string --global
# refused: src/adapters/http/reports_controller.rs already declares the permission
# `reports:read`. Choose another name or --plural, or destroy that generation first.
```

A permission is "declared" when any `src/**/*.rs` names it as a string
literal (`"widgets:read"`), hand-written code included. A controller reuses
only the permission of the model or scaffold of its name and refuses any
other owner.

## Destroy

```sh
forge destroy scaffold post title:string body:text --tenant-scoped --dry-run
forge destroy scaffold post title:string body:text --tenant-scoped
forge destroy controller pages home about_us --public
forge destroy mailer user welcome:name,url password_reset
forge destroy job send_welcome user_id:uuid 'note:string?' --queue
forge rollback && forge destroy model post title:string body:text --tenant-scoped --migrations
```

`forge destroy GENERATOR ARGUMENTS...` takes the arguments the generator was
run with (copy the generate command and replace `generate` with `destroy`),
renders what it would create and removes exactly those files. It can
therefore be exact, and it works for code generated by any earlier version.

- A file that differs from what the generator would write from these
  arguments (edited, generated with other arguments, or written by another
  generator) is **never removed**: the command lists such files, removes
  nothing and fails. `--force` removes them anyway.
- A scaffold's locale file of another locale than English that you
  translated (`config/locales/posts.es.json`) is kept and reported as `keep`,
  not refused.
- Files a bigger or dependent generation needs are refused the same way:
  `destroy model post` while the scaffold's `src/adapters/http/posts.rs`
  exists, and `destroy model|scaffold` while a controller reuses its
  permission. Destroy that generation first, or pass `--force`.
- **Migrations are kept**: a migration that ran is part of the database's
  history. Run `forge rollback`, then pass `--migrations`; a destroy that
  finds nothing but kept files says so. `forge destroy migration` explains
  the same.
- Shared files, the access policy (`src/adapters/http/access.rs`), the
  live-update transport (`src/adapters/http/streams.rs`) and the jobs module
  (`src/adapters/jobs/mod.rs`), are never removed; directories are removed
  when they become empty, except the application's layout directories.
- `--dry-run` lists what would be removed. Nothing is removed through a
  symlink, and only regular files are removed.
- No existing file is edited: destroy prints the wiring to take out.

Known limitations: a directory that becomes empty is removed even if it
existed, empty, before the generator ran; and destroy compares each file,
then removes it, so do not run it while an editor or another command writes
the same files.

## Reading the code

The generators live in `crates/forge-cli/src`: `generators.rs` (planned files,
roles, overwrite and symlink refusal, the permission scan), `scaffold/` (the
model and scaffold), `controller.rs`, `mailer.rs`, `job.rs`, `destroy.rs`, and
the templates in `crates/forge-cli/templates/{scaffold,controller,mailer,job}`.
`crates/forge-cli/tests/scaffold.rs` generates every artifact into a fresh
PostgreSQL application, applies the printed wiring, runs `cargo fmt --check`,
Clippy with warnings denied and `cargo test`, then destroys everything and
checks the application is restored and still builds.
