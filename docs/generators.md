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
- **No wiring to paste (D21).** A generator declares each module it writes
  (`pub mod NAME;`, in sorted order) in the `mod.rs` of its directory, and
  `scaffold`, `resource` and `controller` mount their routes in
  `src/bootstrap/routes_gen.rs` (see [The routes seam](#the-routes-seam)).
  `src/bootstrap/mod.rs` stays yours: no generator edits it. The mailer and
  job generators print their registration, as Forge-Go's do.
- **Never public by accident.** Anything that serves data asks an access
  policy for a permission: tenant-scoped routes of an application with the
  sign-in pages need a signed-in member granted the permission (the `owner`
  role `forge setup` creates is); anything else answers `403` (`DenyAll`)
  until the application decides, or `--public` says so for a global
  resource of an application without Forge identity.
- **Validated names.** Names and fields become Rust identifiers, file names,
  SQL identifiers and URL segments, so they are strict lowercase
  `snake_case` (letters, digits, single underscores, starting with a letter,
  at most 40 characters) and never a Rust keyword. SQL keywords are fine
  (`forge g resource order ...` makes the table `orders`): generated SQL
  quotes every identifier.
- **Field types** (D22): `integer`, `boolean`, `timestamp`, `string`, `text`
  and `uuid`, with Forge-Go's `int`, `bool` and `time` and Rails' `datetime`
  as aliases; a trailing `?` makes a field optional.
- **Aliases.** `forge g` is `forge generate` and `forge d` is `forge
  destroy` (D25).

## The routes seam

`forge new` writes `src/bootstrap/routes_gen.rs` with an empty
`generated_routes` function, which `compose` calls last. The generators own
that file and rewrite it whole, formatted with the application's rustfmt:

- each generator adds a block opened by a `// forge:KIND NAME` comment
  (`// forge:scaffold products`), in its own scope, and `forge destroy` takes
  it back out;
- the shared blocks a block needs come once, before it, and go with the last
  block that uses them: the live-update `streams`, and the guards, which are
  module-level functions of `mod guards`: `protect` (tenant-scoped routes:
  sign-in, CSRF, a membership in the session's tenant and the `owner` role
  granted the permissions) and `public` (`--public`);
- a `// forge:checksum` line records what the generators wrote: a hand edit
  is refused, and nothing is written;
- running a generator again with the same arguments changes nothing
  (`identical`); the same block with other code (other arguments, another
  Forge version) is refused until `forge destroy` takes it out.

To replace a guard (to grant other roles, or put your own authentication in
front of `--public` routes), declare a function of that name with the
generated signature in `src/bootstrap/mod.rs` (or in another module of
`src/bootstrap`, imported there). The seam imports `super::*`: until a
generator runs again and leaves its own guard out, the name is ambiguous and
the build fails, so your guard is never silently ignored.

Generators and `forge destroy` take `.forge/generate.lock` while they run,
so two of them never edit the same files at once; the seam and the `mod.rs`
files are replaced atomically (a temporary file, synced, then renamed).

Tenant-scoped pages are at `/<plural>`: the tenant is the one the session
signed in to (the `__Host-forge_tenant` cookie the sign-in pages set beside
the session cookie), and every request checks an active membership in it.
Live-update streams keep their `/tenants/{tenant_id}/streams` path.

| Command | Creates | Rails |
| ------- | ------- | ----- |
| `forge generate migration NAME [field:type...]` | an up/down migration pair: `ALTER TABLE` statements for `add_X_to_TABLE` and `remove_X_from_TABLE` with fields, blank otherwise | `rails g migration` |
| `forge generate model NAME field:type...` | migration, entity, repository port, use cases, SQLx repository, tests | `rails g model` |
| `forge generate controller NAME ACTION...` | HTTP handlers, routes, views, tests; no persistence | `rails g controller` |
| `forge generate mailer NAME METHOD...` | a mailer with subject, text and HTML templates, previews, tests | `rails g mailer` |
| `forge generate job NAME [field:type...] [--queue]` | a typed job of the persistent queue, its handler, registration, enqueue function, tests | `rails g job` |
| `forge generate resource NAME field:type...` | a model plus a JSON CRUD API under `/v1/<plural>`, behind bearer tokens | `rails g resource` |
| `forge generate scaffold` | a resource plus HTML CRUD pages ([cli](cli.md#forge-generate-scaffold)) | `rails g scaffold` |
| `forge generate view [DIR/]NAME` | one page in the layout, its view model, a test | |
| `forge generate authentication` | the sign-in pages | `rails g authentication` |
| `forge destroy GENERATOR ARGUMENTS...` | removes what a generator created | `rails d` |

## Model

```sh
forge generate model post title:string 'body:text?' published:boolean --tenant-scoped --dry-run
forge generate model post title:string 'body:text?' published:boolean --tenant-scoped
forge migrate
forge test
```

Takes exactly the arguments of `forge generate resource` (and of `forge
generate scaffold` without `--broadcast`: `--tenant-scoped` or `--global`,
`--plural`, field types `string`, `text`, `integer`, `boolean`, `timestamp`,
`uuid`, a trailing `?` for optional) and writes exactly the model files the
resource and the scaffold write, byte for byte, without the HTTP layer:

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
row-level security. IDs are UUIDv7 generated in Rust. Adding the HTTP layer
later is `forge generate resource` or `forge generate scaffold` on a new
name: a model, resource and scaffold of one plural collide instead of
mixing layers.

A model also works in a `forge new --api` application: it has no views and
the model needs none (a scaffold is refused there, because it writes pages;
see [In an API-only application](#in-an-api-only-application)).
Every application with a database has the `forge` dev-dependency with
`testing` that the repository tests need; one generated before that is told to
add `"testing"` to the features of that dev-dependency.

## Resource

```sh
forge generate resource product name:string 'summary:text?' price_cents:integer --tenant-scoped --dry-run
forge generate resource product name:string 'summary:text?' price_cents:integer --tenant-scoped
forge migrate
forge test
```

Forge-Go's `forge generate resource` ([ADR
0045](adr/0045-json-resources-and-bearer-api-authentication.md)): the model
files above, byte for byte, plus a JSON CRUD API under `/v1/<plural>`:

```text
src/adapters/http/products_api.rs     routes(), ProductJson, NewProduct, ProductChanges, ProductList
src/adapters/http/api_access.rs       shared API access policies, when missing
tests/products_api.rs                 the API through the router, with real bearer tokens in memory
```

| Method | Path | Answer | Permission |
| --- | --- | --- | --- |
| `GET` | `/v1/products?cursor=ID` | `200` `{"items", "next_cursor"}`, creation order, 25 per page | `products:read` |
| `POST` | `/v1/products` | `201` with `Location`; `422` | `products:write` |
| `GET` | `/v1/products/{id}` | `200`; `404`; `400 invalid_id` | `products:read` |
| `PATCH` | `/v1/products/{id}` | `{"version", members to change}`: `200`; `409` when stale | `products:write` |
| `DELETE` | `/v1/products/{id}?version=N` | `204`; `409` when stale | `products:write` |

A response is `{"id", the fields, "version"}`. `PATCH` changes only the
members it names, and `null` clears an optional one (`forge::web::Patch`,
Forge-Go's `PatchField`); `null` for a required member and a value that
breaks a rule are `422 validation_failed` with a message per member in
`metadata`; a body that does not decode, or has a member the type lacks,
is `422 invalid_json`. Every route is described in the OpenAPI document
with the `bearerAuth` scheme and its permission, and the generated tests
check every response against it.

**Access.** Every route asks an access policy of
`src/adapters/http/api_access.rs` first, and nothing is public by default:

- **Tenant-scoped** resources take an `ApiTenantAccess`. In an application
  with bearer tokens (`forge new --api-auth`, or `forge generate
  authentication --api-auth`) the printed wiring passes
  `BearerTenantAccess`: exactly one `Authorization: Bearer` header with a
  live token, an active membership in the tenant **the token was issued
  for** and an RBAC grant (`owner` gets both permissions; the output prints
  a read-only `viewer` example). The tenant never comes from the path, the
  query or a header, so a token works only in its own tenant. Without
  bearer tokens, and on MySQL, MariaDB and SQLite, the wiring passes
  `DenyAll` (`403`) and says what to do.
- **Global** resources take an `ApiGlobalAccess`, wired with `DenyAll`
  until you implement your own rule: tenant roles cannot authorize rows
  every tenant shares. (Forge-Go lets tenant roles read a global resource and
  denies its writes; Forge-Rust closes both, as its global scaffold pages
  do.)

Refusals are JSON error reports: `401 credentials_required` without a
header, `401 credentials_invalid` for a malformed, unknown, expired, idle or
revoked token (both with `WWW-Authenticate: Bearer realm="forge"`),
`403 permission_denied`, `503` when a store fails. The use cases check the
grant again (defense in depth). The generated tests cover the round trip,
`401` without a valid token, `403` without the permission, another
tenant's rows (not listed, not found, not changed), validation, conflicts
and the `DenyAll` default.

The site's CSRF layer lets requests with a bearer header or a JSON body
through without a CSRF token, after its cross-site check, as Forge-Go's
does: a cross-site page cannot send either. A resource needs `fn compose`,
which `forge new` generates, with `--api` too
([API-only applications](#in-an-api-only-application)).

Differences from Forge-Go: Forge-Rust has no permission table, so no
migration registers `PLURAL:read`/`PLURAL:write`: the use cases declare
them (`READ_PERMISSION`, `WRITE_PERMISSION`) and the printed `RbacPolicy`
grants them ([permission ownership](#permission-ownership)). Responses have
no `created_at`/`updated_at` (the entity does not carry them), pages are in
creation (UUIDv7) order with the service's page size instead of newest
first with `limit`, and a body that does not decode is `422` where Forge-Go
answers `400`.

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
policy's `503`; [OpenAPI](openapi.md)). Controllers are wired in `fn compose`: HTML ones need a
full-stack application, `--api` ones also work in a `forge new --api` one.

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

## View

```sh
forge generate view welcome --dry-run       # templates/pages/welcome.html and its view model
forge generate view welcome
forge generate view admin/audit_log         # templates/admin/audit_log.html
forge test
```

Forge-Go's `forge generate view`: one server-rendered page,
`templates/DIR/NAME.html` (`DIR` defaults to `pages`; `layouts` is refused),
the same stub a controller action gets: it fills the `title` and `content`
blocks of `templates/layouts/application.html`. Askama checks a template
only through the type that renders it, so the generator also writes the view
model and a test that renders the page and proves it escapes its text:

```text
templates/pages/welcome.html
src/adapters/http/views/pages_welcome_view.rs   PagesWelcomeView { layout, title, content }
tests/pages_welcome_view.rs
```

Nothing routes to the page. Declare the printed `pub mod` line in
`src/adapters/http/views/mod.rs` and render it from a handler that has the
application's `Site`:

```rust
forge::web::html(&crate::adapters::http::views::pages_welcome_view::PagesWelcomeView {
    layout: site.layout(&request),
    title: "Welcome",
    content: "Find me in templates/pages/welcome.html",
})
```

An application created with `--api` has no layout and no Askama: the view
generator refuses it, where Forge-Go's adds its views package (adding
Askama, `build.rs` and the asset pipeline means editing `Cargo.toml`, which
no Forge-Rust generator does). `forge destroy view NAME` removes the three
files while they are unchanged, and refuses a page that the `DIR` controller
renders (`forge destroy view pages/about_us` after `forge generate
controller pages about_us`) unless `--force`.

## In an API-only application

An application created with `forge new --api` has no views, layout, assets,
translations or browser middleware, and no generator adds them to it (as in
Forge-Go). It has `fn compose` in `src/bootstrap/mod.rs`, like a full-stack
application, so the generators that write JSON layers work in it and print
their wiring for it. The ones that write pages refuse it with the reason and
write nothing, `--dry-run` included:

| Generator | In a `forge new --api` application |
| --------- | ---------------------------------- |
| `generate view` | refused: pages need the layout, assets and browser middleware |
| `generate scaffold` | refused: it writes HTML pages; `generate resource` writes the JSON API alone |
| `generate controller NAME ACTION...` | refused; with `--api` it writes JSON endpoints under `/v1/NAME` |
| `generate authentication [--api-auth]` | refused: the sign-in pages need views and CSRF, and the JSON APIs sit beside them |
| `generate resource`, `model`, `controller --api`, `mailer`, `job`, `migration` | as in a full-stack application; nothing under `templates/`, `assets/` or `config/locales/` |

```sh
forge new shop --api -d sqlite3 --skip-docker && cd shop
forge generate resource product name:string --global           # JSON under /v1/products
forge generate controller status_checks index --api            # JSON under /v1/status_checks
forge generate scaffold product name:string --global           # refused
forge generate view about                                      # refused
```

The printed wiring goes at the end of `fn compose`, before `Ok(())`. An
API-only application has no Forge identity wiring (on PostgreSQL it has the
identity, audit and session tables, but no sign-in, no sessions and no bearer
tokens), so every route is wired with the `DenyAll` guard and answers `403`:
replace it with the authentication of your API clients by implementing
`ApiGlobalAccess` or `ApiTenantAccess` (`src/adapters/http/api_access.rs`, for
resources) or `GlobalAccess` (`src/adapters/http/access.rs`, for controllers).
Forge-Rust recognizes the application by the absence of
`src/adapters/http/views/mod.rs`.

## Mailer

```sh
forge generate mailer user welcome:name,url password_reset --dry-run
forge generate mailer user welcome:name,url password_reset
forge test
```

`NAME` is the mailer (`user` becomes `UserMailer`); each `METHOD` is one
email, optionally followed by `:field,field`, the string fields of its data.
A mailer cannot be named `password_reset`: the sign-in pages preview their
password-reset email at `/_mail/password_reset`, the path the mailer's own
previews would take (a method of that name in another mailer is fine).

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
literal (`"widgets:read"`) or assembles it with `concat!`/`format!` from the
literals `"widgets"` and `":read"`, hand-written code included, and whatever
its encoding (a file that is not UTF-8 is searched too). A name built at run
time cannot be found by reading source. The scan fails closed: a symbolic link
to a directory or a Rust file under `src/`, or a file it cannot read, stops the
generator with a message instead of leaving a gap. A controller reuses only the
permission of the model or scaffold of its name and refuses any other owner.

## Destroy

```sh
forge destroy scaffold post title:string body:text --tenant-scoped --dry-run
forge destroy scaffold post title:string body:text --tenant-scoped
forge destroy resource product name:string price_cents:integer --tenant-scoped
forge destroy controller pages home about_us --public
forge destroy mailer user welcome:name,url password_reset
forge destroy job send_welcome user_id:uuid 'note:string?' --queue
forge destroy view admin/audit_log
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
- Each file is checked again right before it is removed, not only when the
  plan is made: one that was edited, or replaced by a symbolic link, in the
  meantime is left alone (and the command says which files it had already
  removed), with or without `--force`.
- A scaffold's locale file of another locale than English that you
  translated (`config/locales/posts.es.json`) is kept and reported as `keep`,
  not refused.
- Files a bigger or dependent generation needs are refused the same way:
  `destroy model post` while the scaffold's `src/adapters/http/posts.rs` or
  the resource's `src/adapters/http/posts_api.rs` exists, `destroy resource
  post` while the scaffold's pages exist, `destroy model|resource|scaffold`
  while a controller reuses its
  permission, and `destroy view pages/about_us` while the `pages` controller
  renders that page. Destroy that generation first, or pass `--force`.
- **Migrations are kept**: a migration that ran is part of the database's
  history. Run `forge rollback`, then pass `--migrations`; a destroy that
  finds nothing but kept files says so. `forge destroy migration` explains
  the same.
- Shared files, the access policies (`src/adapters/http/access.rs` and
  `src/adapters/http/api_access.rs`), the live-update transport
  (`src/adapters/http/streams.rs`) and the jobs module
  (`src/adapters/jobs/mod.rs`), are never removed; directories are removed
  when they become empty, except the application's layout directories.
- `--dry-run` lists what would be removed. Nothing is removed through a
  symlink, and only regular files are removed.
- No existing file is edited: destroy prints the wiring to take out.

Known limitations: a directory that becomes empty is removed even if it
existed, empty, before the generator ran; and a file can still change in the
instant between its last check and its removal (std has no descriptor-relative
unlink), so do not run destroy while an editor or another command writes the
same files. In that instant, a parent directory swapped for a symbolic link
would make the removal go through the link and delete a same-named file
outside the application; this takes someone with write access to the
application racing the command.

## Reading the code

The generators live in `crates/forge-cli/src`: `generators.rs` (planned files,
roles, overwrite and symlink refusal, the permission scan), `scaffold/` (the
model, resource and scaffold), `controller.rs`, `mailer.rs`, `job.rs`, `view.rs`,
`destroy.rs`, and the templates in
`crates/forge-cli/templates/{scaffold,controller,mailer,job,view}`.
`crates/forge-cli/tests/scaffold.rs` generates every artifact into a fresh
PostgreSQL application, applies the printed wiring, runs `cargo fmt --check`,
Clippy with warnings denied and `cargo test`, then destroys everything and
checks the application is restored and still builds.
