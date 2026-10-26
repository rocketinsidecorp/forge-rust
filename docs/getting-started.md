# Getting started with Forge

This guide builds a small blog from `forge new` to a deployable application:
server-rendered pages, sign-in with two-factor authentication, translations,
email, a background job, an OpenAPI document, encrypted credentials and a
deploy configuration. It is the Forge-Rust version of the Rails "Getting
Started" guide and follows the
[Forge-Go guide](https://github.com/rocketinsidecorp/forge-go/blob/main/docs/GETTING_STARTED.md)
with the same command names. The order differs in places: this guide adds
two-factor sign-in, `forge routes` and encrypted credentials (section 11), and
keeps Forge-Go's live-updates demo (5.2), its "comments" model and controller
(5.4) and its SQLite note (end of section 4) as subsections. Every command in it was run
against this repository on Linux with PostgreSQL 16, except the Docker
commands (sections 4, 12 and 13): no Docker daemon was available for that run,
and the text says so where it matters. Absolute paths in the samples are
shortened to `/path/to/blog`, and the `Compiling` and `Finished` lines that
Cargo prints are left out.

Forge is a Rust framework with Rails conventions, so the names carry over:

| Rails | Forge |
| ----- | ----- |
| `rails new blog` | `forge new blog` |
| `bin/rails server` | `forge dev` (rebuilds and restarts when you save) |
| `bin/rails db:prepare`, `db:migrate` | `forge db prepare`, `forge migrate` |
| `rails g scaffold`, `g model`, `g controller`, `g mailer`, `g job` | `forge generate scaffold`, `model`, `controller`, `mailer`, `job` |
| `rails d ...` | `forge destroy ...` |
| `rails routes` | `forge routes` |
| `rails test` | `forge test` |
| `rails credentials:edit` | `forge credentials edit` |
| Kamal | `forge deploy` |

One difference to know up front: **generators never edit existing files**, such
as `src/bootstrap/mod.rs`, your composition root. They print the code to add
and where, and you paste it. That is deliberate (the file is yours), and this
guide shows each paste in full, with a small shell helper so that it is also
copy-and-paste.

Contents: [1 Prerequisites](#1-prerequisites) ·
[2 Install the CLI](#2-install-the-cli) ·
[3 Create the application](#3-create-the-application) ·
[4 Set up and run it](#4-set-up-and-run-it) ·
[5 Scaffold posts](#5-scaffold-posts) ·
[6 Authentication and two-factor](#6-authentication-and-two-factor-authentication) ·
[7 Translations](#7-translations) ·
[8 Email](#8-email) ·
[9 Background jobs](#9-background-jobs) ·
[10 Routes, OpenAPI, tests and diagnostics](#10-routes-openapi-tests-and-diagnostics) ·
[11 Credentials](#11-credentials) ·
[12 Observability](#12-observability) ·
[13 Deploy](#13-deploy) ·
[14 Undo with `forge destroy`](#14-undo-with-forge-destroy) ·
[15 Where to go next](#15-where-to-go-next)

## 1. Prerequisites

- **The Rust toolchain, 1.98.1.** The repository pins it in
  `rust-toolchain.toml` (with `clippy` and `rustfmt`), and so does every
  generated application, so with [rustup](https://rustup.rs) the right
  version is installed the first time you run `cargo` in either directory.
  `cargo --version` inside the application must say `1.98.1`.
- **A PostgreSQL server.** PostgreSQL is the default database and the only one
  with sign-in and tenants, so this guide uses it. The generated `compose.yaml`
  starts PostgreSQL 18 for you if you have Docker (with Compose). Without
  Docker, any PostgreSQL server you can reach as a superuser works; this guide
  was run against PostgreSQL 16.
- **`psql`**, the PostgreSQL client, for the no-Docker setup in section 4 and
  to look up a tenant id in section 5. **`curl`**, **`openssl`** and **`git`**
  for a few commands, and **`perl`** (installed on macOS and Linux) for the
  small edits the generators ask you to make by hand.
- **Docker** only for the Compose database, `docker build`, the observability
  stack and the servers of `forge deploy`. Nothing else here needs it.

## 2. Install the CLI

`forge` is the CLI of this repository. Until the first release tag exists,
install it from a checkout, and keep the checkout: step 3 points new
applications at it.

```sh
git clone https://github.com/rocketinsidecorp/forge-rust
cd forge-rust
cargo install --locked --path crates/forge-cli
cd ..
forge version
forge --help          # the command list
```

```text
forge 0.1.0
```

`cargo install` puts `forge` in `~/.cargo/bin`; make sure that directory is on
your `PATH`. (`cargo install --locked --git
https://github.com/rocketinsidecorp/forge-rust forge-cli` installs the same CLI
without a checkout.)

## 3. Create the application

```sh
forge new blog --forge-path "$PWD/forge-rust/crates/forge"
cd blog
```

```text
Created Forge application `blog` at /path/to/blog
    Updating crates.io index
     Locking 256 packages to latest Rust 1.98.1 compatible versions
      Adding crypto-common v0.1.6 (available: v0.1.7)
```

`forge new` writes the application and generates `Cargo.lock`. Generated
applications depend on `forge` through the Git tag `v0.1.0`, which does not
exist yet, so plain `forge new blog` cannot resolve its dependencies;
`--forge-path` points the application at your checkout instead. (A path
dependency outside the application directory is not part of a Docker build
context, so `docker build` of this application is not supported before the
first release tag: see the "Before the first tag" note in the
[README](../README.md#traces-metrics-and-logs), which vendors a copy of the
framework the way `scripts/e2e-generated-app.sh` does. Sections 12 and 13 build
images; run them once the tag exists.) Choose another database with `-d mysql`, `-d mariadb` or `-d sqlite3`,
a JSON-only service with `--api`, and the default language of the pages with
`--locale en|es|pt-BR`.

```text
blog/
  src/domain/ application/      rules, use cases and ports (no I/O, no HTTP)
  src/adapters/http/            handlers, views and the sign-in, password and two-factor pages
  src/infrastructure/           PostgreSQL, audit, sessions, tenancy, cache, telemetry
  src/bootstrap/mod.rs          the composition root: wires the routes (yours to edit)
  src/bootstrap/{setup,seeds}.rs  the `setup` and `seed` commands
  templates/                    Askama templates: layout, welcome, sign-in, passwords, MFA
  assets/                       CSS and JavaScript, digested and embedded; Turbo and Stimulus, no Node
  config/locales/               translations, one JSON file per locale and area
  config/credentials/           encrypted credentials (the .key file is git-ignored)
  migrations/                   SQL migrations, embedded in the binary
  tests/                        architecture, HTTP, OpenAPI and authentication tests
  Dockerfile, compose.yaml      distroless production image; PostgreSQL + migrate + app
  compose.observability.yaml, observability/   opt-in Grafana, Prometheus, Tempo and Loki
  .github/workflows/ci.yml      the CI workflow
```

The code you add later follows the hexagonal layout: `src/domain` (rules),
`src/application` (use cases and ports), `src/adapters` (HTTP and other
protocols), `src/infrastructure` (database and the like) and `src/bootstrap`
(the only place that picks implementations). `tests/architecture.rs` fails
when an inner ring imports an outer one; [the architecture
page](architecture.md) explains why.

## 4. Set up and run it

You will use two terminals from here on: **terminal 1** runs `forge dev` and
shows the application's log, **terminal 2** runs every other command. Forge
reads its settings from `FORGE_*` environment variables, not from files, so
keep them in two small files that you load into a terminal when you need them
(`.env.*` is git-ignored). The runtime role is least-privilege; the migration
role owns the schema and is used only by `migrate`, `seed` and `setup`. In
terminal 2:

```sh
cat > .env.development <<'EOF'
FORGE_DATABASE_URL=postgres://app_runtime:app_runtime@127.0.0.1:5432/blog
EOF
cat > .env.migrate <<'EOF'
FORGE_MIGRATION_DATABASE_URL=postgres://app_migrator:app_migrator@127.0.0.1:5432/blog
EOF
set -a; . ./.env.development; . ./.env.migrate; set +a     # bash or zsh
```

Start PostgreSQL with its two roles (`app_migrator` and `app_runtime`) and the
empty `blog` database. With Docker, the Compose file does it:

```sh
docker compose up -d db
```

Without Docker, run the same bootstrap script against your server as a
superuser (change the URL to match your server):

```sh
psql 'postgres://postgres:postgres@127.0.0.1:5432/postgres' -f docker/postgres/init.sql
```

```text
CREATE ROLE
CREATE ROLE
CREATE DATABASE
You are now connected to database "blog" as user "postgres".
...
ALTER DEFAULT PRIVILEGES
```

Now the database lifecycle, like Rails' `db:*` tasks. Every command runs the
application's own subcommand, so the CLI never reads your database URL:

```sh
forge db prepare        # create the database if missing, otherwise migrate; safe to repeat
```

```text
postgres database "blog" on 127.0.0.1:5432 exists
2026-10-20T23:24:03.207656Z  INFO forge::database::commands: applied migration app="blog" version=1 description="forge jobs"
2026-10-20T23:24:03.207769Z  INFO forge::database::commands: migrations applied app="blog" count=1
2026-10-20T23:24:03.417585Z  INFO forge::database::commands: applied migration app="blog" version=1 description="forge audit"
2026-10-20T23:24:03.417651Z  INFO forge::database::commands: applied migration app="blog" version=2 description="forge identity"
2026-10-20T23:24:03.417661Z  INFO forge::database::commands: applied migration app="blog" version=3 description="forge authentication"
2026-10-20T23:24:03.417668Z  INFO forge::database::commands: applied migration app="blog" version=4 description="forge mfa"
2026-10-20T23:24:03.417674Z  INFO forge::database::commands: migrations applied app="blog" count=4
```

The first build compiles the framework and takes a few minutes. The other
commands of the family:

```sh
forge db status         # applied and pending migrations (same as: forge migrate status)
forge db create         # create the database; here it already exists
forge db migrate        # apply pending migrations (same as: forge migrate)
forge db seed           # run src/bootstrap/seeds.rs; keep it idempotent
forge db reset          # drop, create, migrate and seed: a clean development database
```

```text
applied   1_forge_audit
applied   2_forge_identity
applied   3_forge_authentication
applied   4_forge_mfa
postgres database "blog" on 127.0.0.1:5432 already exists
... INFO forge::database::commands: no pending migrations app="blog"
... INFO forge::database::commands: no pending migrations app="blog"
Seed data loaded.
dropped postgres database "blog" on 127.0.0.1:5432
created postgres database "blog" on 127.0.0.1:5432
... INFO forge::database::commands: migrations applied app="blog" count=1
... INFO forge::database::commands: migrations applied app="blog" count=4
Seed data loaded.
```

`forge db drop` and `forge db rollback` exist too. Both, and `reset`, are
refused when `FORGE_ENV` is `production` and for a server that is not on this
machine unless you pass `--force-production` (see `forge db drop --help`).
`forge db create` and `drop` need a migration role that may create databases;
the Compose one may.

Now create the first organization, tenant and owner account. The command
prompts twice for the password (12 or more characters) and never takes it as
an argument:

```sh
forge setup --tenant acme --email owner@example.com --organization "Acme Inc"
```

```text
Owner password: 
Owner password (again): 
Created tenant acme (01a15b83-4594-75f5-9685-f9591edbba09) and its owner owner@example.com (01a15b83-4594-75f5-9685-f95a58b2aa60) with the owner role.
Sign in at /session/new?tenant=acme
```

The ids differ on your machine. `forge setup` refuses once a tenant exists.
Scripts pipe the password instead of typing it, and containers run the same
command through the application binary (`blog setup ...`, see [the CLI
reference](cli.md)); never put the password in a command line:

```sh
printf 'Owner password: '; read -rs OWNER_PASSWORD; echo   # bash or zsh
printf '%s\n' "$OWNER_PASSWORD" | forge setup --tenant acme --email owner@example.com --password-stdin
unset OWNER_PASSWORD
```

(Run it only for a second, empty database: here the first `setup` already
created the tenant, so it answers `error: identity is already set up: a tenant
exists, and setup only provisions an empty identity database`.)

Run the application in **terminal 1**, with the runtime URL only (the migration
credential stays out of the server process):

```sh
set -a; . ./.env.development; set +a
forge dev
```

```text
forge dev: watching /path/to/blog; the application restarts when files change (Ctrl-C stops)
forge dev: building
forge dev: running
2026-10-20T22:51:43.088279Z  INFO forge::app: listening app="blog" version="0.1.0" environment="development" address=127.0.0.1:3000 trusted_proxies=0 telemetry=true
2026-10-20T23:24:11.238240Z  INFO forge::background: no job handler is registered yet
```

Open <http://localhost:3000/> for the welcome page. In terminal 2:

```sh
curl http://127.0.0.1:3000/v1/status
curl http://127.0.0.1:3000/health/live
curl http://127.0.0.1:3000/health/ready
```

```text
{"service":"blog","status":"ok"}
{"status":"live"}
{"status":"ready"}
```

Leave `forge dev` running for the rest of the guide. It watches the sources,
templates, assets, locales and migrations, rebuilds, swaps the process once the
new build works and keeps the old one serving while a build fails (the error is
printed in terminal 1). It does not watch your environment: after changing a
`.env.*` file, press Ctrl-C and start it again with the two commands above.
(`forge dev --no-reload` builds and runs once; `cargo run` does the same
without the CLI.)

Check what you have so far:

```sh
forge doctor
```

```text
configuration: ok
environment: development
http address: 127.0.0.1:3000
database: postgres database "blog" on 127.0.0.1:5432
database reachability: the server accepts connections
toolchain: cargo 1.98.1 (797e8a9bc 2026-08-05); rustc 1.98.1 (48a229cea 2026-09-01)
```

### SQLite instead of PostgreSQL

```sh
forge new blog -d sqlite3 --forge-path "$PWD/forge-rust/crates/forge"
cd blog
export FORGE_MIGRATION_DATABASE_URL='sqlite://storage/development.sqlite3?mode=rwc'
export FORGE_DATABASE_URL='sqlite://storage/development.sqlite3?mode=rwc'
forge db prepare        # creates storage/development.sqlite3
forge dev
```

```text
sqlite database storage/development.sqlite3 does not exist
created sqlite database storage/development.sqlite3
... INFO forge::database::commands: no pending migrations app="blog"
Seed data loaded.
```

Generators, tests, views, translations, OpenAPI and `forge destroy` work the
same, and no Docker is needed. What SQLite does not have is Forge identity:
there is no `forge setup` (the application refuses it), no sign-in pages
(`forge generate authentication` is refused, with the reason) and nothing that
supplies a tenant, so scaffold with `--global` instead of `--tenant-scoped`;
tenant isolation on SQLite rests on the application layer, not on row-level
security. A scaffold's pages refuse everyone (`DenyAll`) until you replace it
in the printed wiring with your own `GlobalAccess`. Sections 5 (without
sign-in), 7, 8, 10, 11 and 14 apply; the rest of this guide was run on PostgreSQL. The
same goes for `-d mysql` and `-d mariadb`; see
[ADR 0021](adr/0021-selectable-database-backends.md) for what each backend has.

## 5. Scaffold posts

In terminal 2:

```sh
forge generate scaffold post title:string body:text --tenant-scoped --broadcast
```

```text
  create       migrations/1792536720719875471_create_posts.up.sql
  create       migrations/1792536720719875471_create_posts.down.sql
  create       src/domain/entities/post.rs
  create       src/application/ports/post_repository.rs
  create       src/application/use_cases/posts.rs
  create       src/infrastructure/database/posts.rs
  create       src/adapters/http/access.rs
  create       src/adapters/http/streams.rs
  create       src/adapters/http/posts.rs
  create       src/adapters/http/views/posts.rs
  create       templates/posts/index.html
  create       templates/posts/show.html
  create       templates/posts/new.html
  create       templates/posts/edit.html
  create       templates/posts/_form.html
  create       templates/posts/not_found.html
  create       templates/posts/_row.html
  create       config/locales/posts.en.json
  create       tests/posts_http.rs
  create       tests/posts_repository.rs
```

This is `rails g scaffold`: a migration, the entity, a repository port, use
cases, a PostgreSQL repository, HTML pages with CSRF-protected forms,
translations, live updates and tests for every layer. After the file list the generator prints
the wiring (not repeated here).

- `post` is the singular name (`snake_case` or `PascalCase`); fields are
  `name:type` with `type` one of `string`, `text`, `integer`, `boolean`,
  `timestamp`, `uuid`; a trailing `?` (quoted: `'subtitle:string?'`) makes a
  field optional.
- `--tenant-scoped` binds every query to the signed-in organization (routes
  live under `/tenants/{tenant_id}/`) and, on PostgreSQL, enforces it again
  with row-level security. `--global` is for data every tenant shares.
- `--broadcast` adds live updates over Server-Sent Events (section 5.2; it
  needs the `FORGE_STREAMS_KEY` setting in production, see [views](views.md)).
  Leave it out for a resource that does not change under the reader.
- Add `--dry-run` to any generator to list its files without writing them.

### 5.1 Wire it into the application

The wiring has two parts: eight `pub mod` lines, and a block of code at the
end of `fn compose` in `src/bootstrap/mod.rs`, before its `Ok(())`. First a
helper for the second part. It takes the code on standard input and inserts it
at the end of `compose` (or of the function you name; section 9 uses
`jobs_configure`). Define it once in terminal 2. The generators print the same
code, and you may paste it into your editor instead:

```sh
wire() { FN=${1:-compose} W=$(cat) perl -0pi -e 's/(fn $ENV{FN}\(.*?\n)(    Ok\(\(\)\)\n\})/$1$ENV{W}\n$2/s' src/bootstrap/mod.rs; }
```

The module lines (keeping each file's `mod` lines sorted):

```sh
echo 'pub mod post;' >> src/domain/entities/mod.rs
echo 'pub mod post_repository;' >> src/application/ports/mod.rs
echo 'pub mod posts;' >> src/application/use_cases/mod.rs
perl -0pi -e 's/\n\nuse forge::tenancy/\n\npub mod posts;\n\nuse forge::tenancy/' src/infrastructure/database/mod.rs
perl -0pi -e 's/^pub mod authentication;/pub mod access;\npub mod authentication;\npub mod posts;\npub mod streams;/m' src/adapters/http/mod.rs
perl -0pi -e 's/^pub mod welcome;/pub mod posts;\npub mod welcome;/m' src/adapters/http/views/mod.rs
```

The composition. It grants the `owner` role (the role `setup` gave you) both
`posts:read` (index, show) and `posts:write` (new, create, edit, update,
delete); permissions match exactly and every other role is denied until you
name it here:

```sh
wire <<'EOF'
    // posts: tenant-scoped, under /tenants/{tenant_id}/posts.
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
    let pool = composition.postgres()?.pool().clone();
    // Sessions end after FORGE_AUTH_SESSION_IDLE_TIMEOUT without a request.
    let sessions = crate::infrastructure::auth::PostgresSessionStore::new(pool.clone())
        .with_idle_timeout(
            forge::auth::SessionIdleTimeout::new(composition.config().auth.session_idle_timeout)
                .map_err(forge::ComposeError::other)?,
        );
    let access = crate::adapters::http::access::SessionTenantAccess::new(
        std::sync::Arc::new(sessions),
        std::sync::Arc::new(crate::infrastructure::tenancy::PgMembershipStore::new(pool)),
        policy,
    );
    // Live updates, once for every broadcasting resource. FORGE_STREAMS_KEY
    // (`openssl rand -base64 32`, the same on every replica) is required in
    // production.
    let streams =
        crate::adapters::http::streams::Streams::register(composition, forge::Fanout::Postgres)?;
    let posts = crate::application::use_cases::posts::PostService::new(std::sync::Arc::new(
        crate::infrastructure::database::posts::PgPostRepository::new(
            composition.postgres()?.clone(),
        ),
    ));
    crate::adapters::http::posts::routes(
        composition.router(),
        &site,
        posts,
        std::sync::Arc::new(access),
        &streams,
    )?;
EOF
forge format
```

Apply the migration (terminal 2 has the migration URL) and look at the routes.
`forge dev` has already rebuilt by itself:

```sh
forge migrate
forge routes | grep -E 'METHOD|posts'
```

```text
... INFO forge::database::commands: no pending migrations app="blog"
... INFO forge::database::commands: applied migration app="blog" version=1792538664283175633 description="create posts"
... INFO forge::database::commands: migrations applied app="blog" count=1
METHOD  PATH                                          ACTION  SOURCE
GET     /tenants/{tenant_id}/posts                    -       src/adapters/http/posts.rs:99
POST    /tenants/{tenant_id}/posts                    -       src/adapters/http/posts.rs:99
GET     /tenants/{tenant_id}/posts/new                -       src/adapters/http/posts.rs:99
DELETE  /tenants/{tenant_id}/posts/{id}               -       src/adapters/http/posts.rs:99
GET     /tenants/{tenant_id}/posts/{id}               -       src/adapters/http/posts.rs:99
PATCH   /tenants/{tenant_id}/posts/{id}               -       src/adapters/http/posts.rs:99
PUT     /tenants/{tenant_id}/posts/{id}               -       src/adapters/http/posts.rs:99
GET     /tenants/{tenant_id}/posts/{id}/edit          -       src/adapters/http/posts.rs:99
```

Look up your tenant id if you lost it (section 4 printed it):

```sh
psql "$FORGE_MIGRATION_DATABASE_URL" -Atc "select id from forge_tenants where slug = 'acme'"
```

```text
01a15b83-4594-75f5-9685-f9591edbba09
```

Open <http://localhost:3000/session/new?tenant=acme>, sign in as
`owner@example.com`, then open `http://localhost:3000/tenants/TENANT_ID/posts`
with your id, click **New post**, fill in a title and a body and save. A blank
title shows the form again with `422` and the message beside the field.
Signed-out visitors are sent to the sign-in page and back. Use `localhost` or
`127.0.0.1`: the session cookies are `__Host-` cookies, which browsers accept
only from a secure context, and both count as one.

Prefer the terminal? The same flow with `curl` (the password is read without
echo and goes to `curl` on standard input, never in its arguments; every form
needs the CSRF token from the page it came from):

```sh
B=http://localhost:3000 JAR=$(mktemp)
TENANT_ID=$(psql "$FORGE_MIGRATION_DATABASE_URL" -Atc "select id from forge_tenants where slug = 'acme'")
token() { sed -n 's/.*name="csrf_token" value="\([^"]*\)".*/\1/p' | head -1; }
printf 'Owner password: '; read -rs OWNER_PASSWORD; echo   # bash or zsh
T=$(curl -s -c $JAR -b $JAR "$B/session/new?tenant=acme" | token)
printf '%s' "$OWNER_PASSWORD" | curl -s -o /dev/null -c $JAR -b $JAR -d "csrf_token=$T" -d tenant=acme -d email=owner@example.com --data-urlencode 'password@-' $B/session
unset OWNER_PASSWORD
T=$(curl -s -c $JAR -b $JAR $B/tenants/$TENANT_ID/posts/new | token)
curl -si -c $JAR -b $JAR -d "csrf_token=$T" --data-urlencode "title=Hello Forge" --data-urlencode "body=My first post" $B/tenants/$TENANT_ID/posts | grep -iE '^(HTTP|location)'
curl -s -b $JAR $B/tenants/$TENANT_ID/posts | grep '<td>Hello'
rm -f "$JAR"
```

```text
HTTP/1.1 303 See Other
location: /tenants/01a15b83-4594-75f5-9685-f9591edbba09/posts/01a15b84-d22d-7468-b872-fe5e744347c4
      <td>Hello Forge</td>
```

(Once two-factor authentication is on, section 6, the sign-in answers `303`
to `/session/mfa` and this script stops there; use the browser.)

### 5.2 Live updates

Because the posts were scaffolded with `--broadcast`, the index page
subscribes to a stream: open `/tenants/TENANT_ID/posts` in two browser windows,
signed in to the same organization, and create, edit or delete a post in one;
the other changes without a reload. The server pushes Turbo Streams over
Server-Sent Events, on a channel whose name is signed (HMAC-SHA256, expiring
after 24 hours) and bound to the tenant, and serves it only to callers the
access policy grants `posts:read`. Delivery is at most once and without
replay, so the next page load is always authoritative. `Fanout::Postgres`
(in the wiring) reaches every replica through `LISTEN/NOTIFY`.

Without a second browser, watch the stream from the terminal. The page
carries the signed stream address in a `<turbo-stream-source>` element; open
it with `curl -N` and, in another terminal, create a post:

```sh
B=http://localhost:3000 JAR=$(mktemp)
TENANT_ID=$(psql "$FORGE_MIGRATION_DATABASE_URL" -Atc "select id from forge_tenants where slug = 'acme'")
token() { sed -n 's/.*name="csrf_token" value="\([^"]*\)".*/\1/p' | head -1; }
printf 'Owner password: '; read -rs OWNER_PASSWORD; echo   # bash or zsh
T=$(curl -s -c $JAR -b $JAR "$B/session/new?tenant=acme" | token)
printf '%s' "$OWNER_PASSWORD" | curl -s -o /dev/null -c $JAR -b $JAR -d "csrf_token=$T" -d tenant=acme -d email=owner@example.com --data-urlencode 'password@-' $B/session
unset OWNER_PASSWORD
SRC=$(curl -s -b $JAR $B/tenants/$TENANT_ID/posts | sed -n 's/.*<turbo-stream-source src="\([^"]*\)".*/\1/p')
curl -sN -b $JAR "$B$SRC"        # stays open; Ctrl-C ends it
```

In a third terminal, with the same `$B`, `$JAR`, `$TENANT_ID` and `token`
(run it from the same shell session, or define them again):

```sh
T=$(curl -s -c $JAR -b $JAR $B/tenants/$TENANT_ID/posts/new | token)
curl -s -o /dev/null -c $JAR -b $JAR -d "csrf_token=$T" --data-urlencode "title=Hello Forge" --data-urlencode "body=My first post" $B/tenants/$TENANT_ID/posts
```

The first terminal prints the keep-alive and then the change, as the
fragments the browser applies:

```text
retry: 3000
: connected

data: <turbo-stream action="remove" target="posts_empty"></turbo-stream>
data: <turbo-stream action="append" target="posts"><template><tr id="post_01a16f17-6a6d-7035-960e-6d14e0042555">
data:       <td>Hello Forge</td>
data:       <td>My first post</td>
data:       <td><a href="/tenants/01a16f13-5299-721e-a22c-3e7da64be2a4/posts/01a16f17-6a6d-7035-960e-6d14e0042555">Show</a> <a href="/tenants/01a16f13-5299-721e-a22c-3e7da64be2a4/posts/01a16f17-6a6d-7035-960e-6d14e0042555/edit">Edit</a></td>
data: </tr></template></turbo-stream>
data: 
```

In development the signing key is random per process (the application logs
`FORGE_STREAMS_KEY is not set`, and open pages stop updating after a
restart). Set `FORGE_STREAMS_KEY` to the same value on every replica in
staging and production (`openssl rand -base64 32`). [Views](views.md#live-updates)
has the protections and the limits.

### 5.3 Test

```sh
forge test
```

```text
test result: ok. 21 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.75s
...
test result: ok. 10 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.02s
test postgres_round_trip_locking_and_pages ... ignored, needs FORGE_TEST_DATABASE_URL
test postgres_row_level_security_isolates_tenants ... ignored, needs FORGE_TEST_DATABASE_URL
```

`forge test` is `cargo test --all-targets --all-features` in the application
(arguments pass through; section 10 has the rest of the toolbox). The
scaffold's tests cover validation, optimistic locking, the permission checks,
CSRF, tenant isolation (the repository tests, with row-level security forced
on the table's owner) and an end-to-end event stream. The repository tests
need a database server and are `ignored` by default; section 10 shows how to
run them.

### 5.4 Comments: a model and a controller

A **model** is the data and its rules without any HTTP; a **controller** is
read-only pages without any data. Generate the model first. A comment belongs
to a post, but there is no `references` type: `post_id:uuid` is a plain `uuid`
column and the migration adds no foreign key:

```sh
forge generate model comment post_id:uuid body:text --tenant-scoped
```

```text
  create       migrations/1792865223274081083_create_comments.up.sql
  create       migrations/1792865223274081083_create_comments.down.sql
  create       src/domain/entities/comment.rs
  create       src/application/ports/comment_repository.rs
  create       src/application/use_cases/comments.rs
  create       src/infrastructure/database/comments.rs
  create       tests/comments_repository.rs
```

These are the scaffold's model files without the pages, the access policy and
the stream. Declare the four modules it prints, apply the migration and test:

```sh
echo 'pub mod comment;' >> src/domain/entities/mod.rs
echo 'pub mod comment_repository;' >> src/application/ports/mod.rs
echo 'pub mod comments;' >> src/application/use_cases/mod.rs
perl -0pi -e 's/\n\npub mod posts;/\n\npub mod comments;\npub mod posts;/' src/infrastructure/database/mod.rs
forge migrate
forge test
```

The second wiring step it prints builds `CommentService` where you need it (in
a handler or a job). Rust warns about an unused variable, so do not paste it
before something uses it. For CRUD pages for comments use
`forge generate scaffold comment ...` instead of the model; a model and a
scaffold of the same plural collide on purpose.

Now a guarded page. It reuses the model's `comments:read` permission (a
permission has one owner, so the controller does not declare a second one):

```sh
forge generate controller comments index
```

```text
  create       src/adapters/http/comments_controller.rs
  create       src/adapters/http/views/comments_controller.rs
  create       templates/comments/index.html
  create       tests/comments_controller_http.rs
  reuse        src/adapters/http/access.rs
```

Declare its two modules and paste its route registration, which the generator
printed with `DenyAll`:

```sh
perl -0pi -e 's/^pub mod authentication;/pub mod authentication;\npub mod comments_controller;/m' src/adapters/http/mod.rs
perl -0pi -e 's/^pub mod posts;/pub mod comments_controller;\npub mod posts;/m' src/adapters/http/views/mod.rs
wire <<'EOF'
    crate::adapters::http::comments_controller::routes(
        composition.router(),
        &site,
        std::sync::Arc::new(crate::adapters::http::access::DenyAll),
    )?;
EOF
```

Edit the page: the layout and the heading stay, replace the text in
`templates/comments/index.html`, then check the route, the tests and the
guard:

```sh
cat > templates/comments/index.html <<'EOF'
{% extends "layouts/application.html" %}

{% block title %}{{ title }}{% endblock %}

{% block content %}
<h1>{{ title }}</h1>
<p>Moderation tools will live here.</p>
{% endblock %}
EOF
forge format
forge routes | grep -E 'METHOD|comments'
forge test
curl -si http://127.0.0.1:3000/comments | head -1
```

```text
METHOD  PATH                                          ACTION  SOURCE
GET     /comments                                     -       src/adapters/http/comments_controller.rs:65
test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
HTTP/1.1 403 Forbidden
```

The `403` is the point: a controller is never public by accident. It stays
closed until you replace `DenyAll` with your own `GlobalAccess`
implementation (`src/adapters/http/access.rs` explains it), or generate it
with `--public` for pages anyone may read. Other actions are other arguments:
`forge generate controller reports monthly` is `GET /reports/monthly` guarded
by `reports:read`, `--api` makes JSON handlers under `/v1`, and controllers
answer `GET` only; anything that changes data is a scaffold
([generators](generators.md#controller)).

## 6. Authentication and two-factor authentication

You already have it: `forge new` on PostgreSQL generates the sign-in,
sign-out, password-reset and two-factor pages, and `forge setup` created the
owner.

| URL | Page |
| --- | ---- |
| `/session/new?tenant=acme` | sign in (organization, email, password) |
| `/session/mfa` | authenticator or backup code, once two-factor is on |
| `/mfa` | two-factor settings: turn on, replace the authenticator, new backup codes, turn off |
| `/passwords/new` | forgot password (needs outgoing mail, section 8) |
| `/passwords/edit?token=...` | choose a new password from the emailed link |

A page asks for one permission (`posts:read`, ...) and roles hold permissions.
Sign-ins (success, failure, throttling) and permission denials are audited
([ADR 0026](adr/0026-authentication-pages-and-setup.md), [threat
model](threat-model.md)). To let a signed-in user sign out, put
`{% include "sessions/_sign_out.html" %}` in a template: it renders the
`DELETE /session` form. `forge generate authentication` writes the same pages
into an older PostgreSQL application; here it refuses, because they exist:

```sh
forge generate authentication --dry-run
```

```text
error: cannot generate authentication: refusing to overwrite existing files; nothing was written:
  migrations/00000000000000000003_forge_authentication.up.sql
  migrations/00000000000000000003_forge_authentication.down.sql
  migrations/00000000000000000004_forge_mfa.up.sql
  ...
  config/locales/authentication.en.json
```

### Session idle timeout

A session ends 24 hours after sign-in, or earlier after two hours without a
request (Forge-Go's default and variable). The next request is refused
exactly as with a revoked session: pages send the browser to
`/session/new`, JSON clients get `401`; signing in again creates a new
session. Change the idle limit, from `5m` to `720h` (there is no way to turn
it off; `720h` is the longest session lifetime), in your env file or the
deployment's environment:

```sh
echo "FORGE_AUTH_SESSION_IDLE_TIMEOUT=30m" >> .env.development
```

`PostgresSessionStore` (`src/infrastructure/auth/mod.rs`) keeps each
session's last use in `forge_sessions.last_seen_at` and writes it at most
once a minute per session: a conditional `UPDATE` runs only when the stored
use is more than a minute old and the session is still live (not revoked,
not expired, not idle), so most requests only read and concurrent requests
record one use. The database clock decides both, so replicas with skewed
clocks agree. If that write fails, the request fails with `503` (fail
closed) instead of passing unrecorded.

An application generated before the idle timeout keeps its sessions until it
adds migration `00000000000000000006_forge_session_idle_timeout` and the new
session store, and passes the timeout in `fn compose`. Run the migration
before deploying the new code, which reads the column; sessions open at the
upgrade count as used then, so nobody is signed out:

```sh
(cd /tmp && forge new shop-new -d postgresql --skip-docker --skip-ci --skip-lockfile)
cp /tmp/shop-new/migrations/00000000000000000006_forge_session_idle_timeout.*.sql migrations/
diff -u src/infrastructure/auth/mod.rs /tmp/shop-new/src/infrastructure/auth/mod.rs   # PostgresSessionStore
diff -u src/bootstrap/mod.rs /tmp/shop-new/src/bootstrap/mod.rs                       # the idle_timeout lines
grep -n 'PostgresSessionStore::new' src/bootstrap/mod.rs                      # every store must get .with_idle_timeout
PGOPTIONS='-c lock_timeout=5s' forge migrate   # or: cargo run -- migrate
```

Every `PostgresSessionStore` built in `fn compose` must get the timeout, not
only the sign-in pages' one: each tenant-scoped scaffold's wiring builds its
own store for `SessionTenantAccess`, and a store left at
`PostgresSessionStore::new(pool.clone())` would keep the two-hour default
whatever `FORGE_AUTH_SESSION_IDLE_TIMEOUT` says. Wiring printed by `forge
generate scaffold` before the idle timeout becomes:

```rust
let sessions = crate::infrastructure::auth::PostgresSessionStore::new(pool.clone())
    .with_idle_timeout(
        forge::auth::SessionIdleTimeout::new(composition.config().auth.session_idle_timeout)
            .map_err(forge::ComposeError::other)?,
    );
let access = crate::adapters::http::access::SessionTenantAccess::new(
    std::sync::Arc::new(sessions),
    std::sync::Arc::new(crate::infrastructure::tenancy::PgMembershipStore::new(pool)),
    policy,
);
```

`ALTER TABLE ... ADD COLUMN` rewrites nothing (the default `now()` is stored
once), but it takes a brief `ACCESS EXCLUSIVE` lock on `forge_sessions`,
which waits behind every open transaction that touched the table and blocks
sign-ins meanwhile. `lock_timeout` bounds that wait: if it expires the
migration rolls back and can be run again at a quieter moment. The runtime
role's table-wide `UPDATE` on `forge_sessions` covers the new column, so no
grant changes. `forge generate authentication` refuses an application whose
session store or migrations predate the idle timeout and points here.

### Two-factor authentication

Accounts can require a code from an authenticator app (TOTP) at every sign-in
([MFA](mfa.md)). The seeds are encrypted with `FORGE_AUTH_MFA_KEY`, the
unpadded base64url of 32 random bytes. Keep it in a secret manager in
production; for development put it in your env file and restart `forge dev`
(Ctrl-C in terminal 1, then the two commands from section 4):

```sh
echo "FORGE_AUTH_MFA_KEY=$(openssl rand -base64 32 | tr '+/' '-_' | tr -d '=')" >> .env.development
```

Signed in, open <http://localhost:3000/mfa>, enter your password, add the key
to an authenticator app (open the `otpauth://` link on a phone or type the
Base32 key) and confirm with your password and the app's 6-digit code. Keep the
10 backup codes the page shows once. You are signed out everywhere, and from now
on signing in asks for a code at `/session/mfa`. The settings page then reads
`Two-factor authentication is on. 10 backup codes left.` A code, or a backup
code, works only once, so after a failed sign-in wait for the app's next code.
Replacing the authenticator, generating new backup codes and turning two-factor
off are on the same page and ask for the password and a current code. Without
the key the page says two-factor is unavailable; an invalid key stops the
application at start-up.

## 7. Translations

Every generated page reads its text from `config/locales`. Add Brazilian
Portuguese by copying the three files you have, renaming the top-level key
(line 2 of each file), and translating the values:

```sh
cd config/locales
for pair in en:pt-BR authentication.en:authentication.pt-BR posts.en:posts.pt-BR; do
  cp "${pair%%:*}.json" "${pair##*:}.json"
  sed -i.bak '2s/"en"/"pt-BR"/' "${pair##*:}.json" && rm "${pair##*:}.json.bak"
done
sed -i.bak 's/"title": "Welcome to %{app}"/"title": "Bem-vindo ao %{app}"/' pt-BR.json && rm pt-BR.json.bak
cd ../..
forge i18n check
```

```text
config/locales: en, pt-BR; 129 keys, 114 referenced in templates and Rust code

en: complete

pt-BR: complete
```

A locale is just the files that exist; the copy is complete but still in
English except the welcome title. `forge i18n check` exits `1` and lists what a
locale lacks, plural forms it needs, and keys nothing references; `--json` is
for CI (`{"schema_version":1,"locales":["en","pt-BR"],"keys":129,...}`). Each
request picks its locale from the `locale` cookie, then `Accept-Language`, then
the default. `forge dev` rebuilds when the locale files change; once it prints
`running` again:

```sh
curl -s -H 'Accept-Language: pt-BR' http://127.0.0.1:3000/ | grep '<h1'
curl -s -H 'Accept-Language: en' http://127.0.0.1:3000/ | grep '<h1'
```

```text
        <h1 id="welcome-title">Bem-vindo ao blog</h1>
        <h1 id="welcome-title">Welcome to blog</h1>
```

To make Portuguese the default, set `FORGE_I18N_DEFAULT_LOCALE=pt-BR`
(an environment variable, no rebuild). For a new application, `forge new shop
--locale pt-BR` ships the generated pages already translated into `pt-BR`, `es`
or `en`. In templates use `{{ layout.i18n.t("welcome.title") }}`; in handlers
`site.i18n(&request).t("...").text()`. See [i18n](i18n.md).

## 8. Email

Generate a mailer with typed methods, subject, text and HTML templates, browser
previews and tests:

```sh
forge generate mailer user welcome:name,url
```

```text
  create       src/adapters/email/user_mailer.rs
  create       templates/mailers/user/welcome.subject.txt
  create       templates/mailers/user/welcome.txt
  create       templates/mailers/user/welcome.html
  create       tests/user_mailer.rs
```

Wire it, with the helper from section 5. The mailer reads `FORGE_MAIL_URL` and
`FORGE_MAIL_FROM`; without `FORGE_MAIL_URL` every delivery fails closed instead
of pretending to send. Previews render each email with fixture data, send
nothing, and exist only in development and test with `FORGE_MAIL_PREVIEW=true`:

```sh
echo 'pub mod user_mailer;' >> src/adapters/email/mod.rs
wire <<'EOF'
    // UserMailer: FORGE_MAIL_URL and FORGE_MAIL_FROM configure delivery.
    let user_mailer = crate::adapters::email::user_mailer::UserMailer::from_config(
        composition.config(),
        composition.telemetry(),
    )
    .map_err(forge::ComposeError::other)?;
    // FORGE_MAIL_PREVIEW=true (development and test only) shows the emails at
    // /_mail/user without sending them.
    if composition.config().mail.preview {
        let previews = forge::mail::PreviewHandler::new(
            composition.config(),
            "/_mail/user",
            user_mailer.previews(),
        )
        .map_err(forge::ComposeError::other)?;
        composition.router().layer(previews);
    }
EOF
forge format
```

Configure development mail: every message becomes an `.eml` file under
`tmp/mail`, and previews are on. These settings also turn on the
password-reset emails, which need a mail transport and the public URL of the
reset page. Then restart `forge dev` (Ctrl-C, then the two commands of section
4):

```sh
cat >> .env.development <<'EOF'
FORGE_MAIL_URL=file:tmp/mail
FORGE_MAIL_FROM="blog <no-reply@example.com>"
FORGE_MAIL_PREVIEW=true
FORGE_AUTH_PASSWORD_RESET_URL=http://localhost:3000/passwords/edit
EOF
echo '/tmp/' >> .gitignore
```

```text
... WARN forge_mail::file: development mail sender active: messages are written to files and NOT delivered dir=tmp/mail
```

Open <http://localhost:3000/_mail/user/> for the preview list and
`/_mail/user/welcome` for the email rendered with fixture data (headers, the
HTML part in a sandboxed frame, the text part and the raw MIME source). The
password-reset email has its own preview at `/_mail`:

```sh
curl -s http://localhost:3000/_mail/user/ | grep '<li>'
curl -s http://localhost:3000/_mail | grep '<li>'
```

```text
<ul><li><a href="/_mail/user/welcome">welcome</a> &mdash; UserMailer: welcome</li></ul>
<ul><li><a href="/_mail/password_reset">password_reset</a> &mdash; The email a password reset request sends</li></ul>
```

Edit the templates (`templates/mailers/user/welcome.*`) and reload. To see a
real delivery, request a password reset at
<http://localhost:3000/passwords/new?tenant=acme> and open the newest
`tmp/mail/*.eml`, or from the terminal (`token` is the function from section
5):

```sh
B=http://localhost:3000 JAR=$(mktemp)
T=$(curl -s -c $JAR -b $JAR "$B/passwords/new?tenant=acme" | token)
curl -s -o /dev/null -w '%{http_code}\n' -c $JAR -b $JAR -d "csrf_token=$T" -d tenant=acme -d email=owner@example.com $B/passwords
sleep 1                      # the email is written a moment after the redirect
grep -h '^Subject' tmp/mail/*.eml
rm -f "$JAR"
```

```text
303
Subject: Reset your blog password
```

Send your own email from code through the typed method
(`user_mailer.welcome(to, &data).await`): data is escaped for its context and
header injection is refused before a byte is sent. Run the mailer's tests, and
in production set `FORGE_MAIL_URL=smtps://user:password@smtp.example.com:465`
([mail](mail.md)):

```sh
forge test --test user_mailer
```

```text
running 5 tests
test a_header_injecting_recipient_is_refused ... ok
test welcome_escapes_its_data_in_the_html_part ... ok
test previews_render_and_exist_only_in_development ... ok
test a_failing_sender_records_nothing_and_an_unconfigured_mailer_fails ... ok
test welcome_is_delivered_with_its_subject_and_both_parts ... ok

test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

## 9. Background jobs

Forge's jobs live in a PostgreSQL queue: retries with backoff, timeouts, dead
jobs, workers that run inside `serve`, and cron schedules that fire exactly
once across replicas ([jobs](jobs.md)). `forge new` on PostgreSQL has already
wired the queue into `src/bootstrap/mod.rs`: `.jobs(forge::WorkerMode::InServe)`
makes `serve` run the workers (`Dedicated` leaves them to `jobs work`), the
framework's job schema was applied by `forge db prepare`, and `jobs_configure`
is where you register jobs and schedules. No job exists yet, so the queue is
empty (while `forge dev` is rebuilding, `cargo run` may print "Blocking
waiting for file lock" for a few seconds; it continues once the build is done):

```sh
cargo run -q -- jobs status
```

```text
no jobs
```

Generate a typed job with its payload, handler, enqueue function and tests:

```sh
forge generate job send_welcome user_id:uuid 'note:string?' --queue
```

```text
  create       src/adapters/jobs/mod.rs
  create       src/adapters/jobs/send_welcome.rs
  create       tests/send_welcome_job.rs
```

(`--queue` is required: Forge-Rust schedules are queue jobs too.) The generator
prints the two module lines and the registration for `jobs_configure`; the
`jobs` feature and `.jobs(...)` it can also ask for are already in your
application. The last block below also schedules the job every minute so you
can watch the queue work; the first tick fires at once:

```sh
perl -0pi -e 's/^pub mod http;/pub mod http;\npub mod jobs;/m' src/adapters/mod.rs
echo 'pub mod send_welcome;' >> src/adapters/jobs/mod.rs
wire jobs_configure <<'EOF'
    crate::adapters::jobs::send_welcome::register(composition.jobs()?)
        .map_err(forge::ComposeError::other)?;
    // Run it every minute, UTC, to watch the queue work.
    composition
        .jobs()?
        .schedule(
            forge::jobs::Schedule::new(
                "send_welcome.every_minute",
                "* * * * *".parse().map_err(forge::ComposeError::other)?,
                &crate::adapters::jobs::send_welcome::SendWelcome {
                    user_id: forge::core::Uuid::nil(),
                    note: None,
                },
            )
            .map_err(forge::ComposeError::other)?,
        )
        .map_err(forge::ComposeError::other)?;
EOF
```

The generated `perform` fails permanently until you write it, so a placeholder
can never report success (such a job goes straight to `dead`). Implement it,
and replace the test that asserts the placeholder:

```sh
perl -0pi -e 's/pub async fn perform\(job: SendWelcome\).*\z/pub async fn perform(job: SendWelcome) -> Result<(), JobError> {\n    println!("send_welcome ran for user {}", job.user_id);\n    Ok(())\n}\n/s' src/adapters/jobs/send_welcome.rs
perl -0pi -e 's/#\[tokio::test\]\nasync fn the_placeholder_fails_permanently.*\z/#[tokio::test]\nasync fn perform_succeeds() {\n    assert!(perform(payload()).await.is_ok());\n}\n/s' tests/send_welcome_job.rs
forge format
forge test --test send_welcome_job
```

```text
running 3 tests
test the_job_has_a_stable_valid_name_and_bounded_attempts ... ok
test the_handler_registers_once ... ok
test perform_succeeds ... ok

test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

`forge dev` rebuilds as soon as you save, and terminal 1 shows the schedule
enqueue the job and a worker run it:

```text
... INFO forge_jobs::scheduler: scheduled job enqueued schedule="send_welcome.every_minute" job="send_welcome" job_id=01a15b88-662f-76e4-9cf0-38de4f3ceb37
send_welcome ran for user 00000000-0000-0000-0000-000000000000
... INFO job: forge_jobs::worker: job completed queue="default" job="send_welcome" job_id=01a15b88-662f-76e4-9cf0-38de4f3ceb37 attempt=1 elapsed_ms=0 trace_id=0c5fec8f6055547f
```

The operator commands are subcommands of the application binary (they need the
runtime URL, which terminal 2 has):

```sh
cargo run -q -- jobs status         # counts per queue and state
cargo run -q -- jobs dead 5         # dead jobs, newest first (payloads are never shown)
```

```text
QUEUE                    STATE            JOBS  OLDEST_DUE
default                  completed           2  -
no dead jobs
```

`jobs retry JOB_ID` requeues a dead job with a fresh attempt budget, `jobs
discard JOB_ID` deletes it, and `cargo run -- jobs work` runs the workers
without HTTP (the `Dedicated` mode). Delivery is at least once, so make
`perform` idempotent. Enqueue from your own code inside the transaction that
writes the business state, with
`crate::adapters::jobs::send_welcome::enqueue(&mut transaction, &job, options)`:
roll the transaction back and no job exists.

Remove the every-minute schedule now that you have seen it work (deleting the
block from `jobs_configure` by hand does the same):

```sh
perl -0pi -e 's/    \/\/ Run it every minute.*?\.map_err\(forge::ComposeError::other\)\?;\n(?=    Ok)//s' src/bootstrap/mod.rs
forge format
```

## 10. Routes, OpenAPI, tests and diagnostics

`forge routes` runs the composed application without listening and prints every
HTML and JSON route with the source line that registered it, like `rails
routes`; filter it like `rails routes | grep`:

```sh
forge routes | grep -E 'METHOD|/v1|/session'
```

```text
METHOD  PATH                                          ACTION  SOURCE
DELETE  /session                                      -       src/adapters/http/authentication.rs:265
POST    /session                                      -       src/adapters/http/authentication.rs:265
GET     /session/mfa                                  -       src/adapters/http/authentication.rs:265
POST    /session/mfa                                  -       src/adapters/http/authentication.rs:265
GET     /session/new                                  -       src/adapters/http/authentication.rs:265
GET     /v1/status                                    -       src/adapters/http/status.rs:24
```

JSON routes declare their types and Forge builds an OpenAPI 3.1 document from
them. `GET /v1/status` and the health routes are described already:

```sh
curl -s http://127.0.0.1:3000/openapi.json | head -c 120      # served in development and test
forge openapi --output openapi.json                           # write it, to commit and diff
forge openapi --check openapi.json                            # exit 1 when the file is stale
```

```text
openapi: 30 routes are not described (forge::openapi::describe or typed): GET /, GET /assets/application-3af03523ae51c631.css, ...
openapi: wrote /path/to/blog/openapi.json (4 paths)
openapi: 30 routes are not described (forge::openapi::describe or typed): GET /, GET /assets/application-3af03523ae51c631.css, ...
openapi: /path/to/blog/openapi.json is up to date
```

The HTML pages are not part of the API document, so they are listed on stderr
and not failed. The paths in the file are `/health`, `/health/live`,
`/health/ready` and `/v1/status`. `forge openapi` with no option prints the
document. Staging and production serve `/openapi.json` only with
`FORGE_OPENAPI_ENABLED=true`: the document lists every described route.
Describe your own JSON routes with `forge::openapi::typed` or `describe`
([OpenAPI](openapi.md)).

The rest of the toolbox:

```sh
forge test --test architecture      # one test file: arguments replace the defaults
forge test                          # cargo test --all-targets --all-features
forge check && forge lint           # cargo check; clippy with warnings denied
forge format                        # cargo fmt --all
forge build                         # release binary, from the locked dependency graph
forge doctor --json                 # versioned, secret-free report for scripts and CI
```

```text
running 2 tests
test detects_grouped_aliased_and_inline_references ... ok
test rings_respect_dependency_direction ... ok

test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

A plain `forge test` runs every generated suite that needs no database (the
unit, architecture, HTTP, OpenAPI, mailer and job tests). Tests that talk to a
database server (the repository and authentication tests, and the scaffold's)
show up as `ignored` unless you ask for them and name a server whose user may
create and drop databases; they create and drop their own:

```sh
FORGE_TEST_DATABASE_URL=postgres://postgres:postgres@127.0.0.1:5432/postgres forge test -- --include-ignored
```

`forge rollback` (also `forge db rollback`) reverts your newest application
migration, never a framework table. The generated `.github/workflows/ci.yml`
runs the same checks in CI.

## 11. Credentials

Secrets that do not belong in `.env` files live encrypted in
`config/credentials/ENV.json.enc`, which you commit; the key stays out of git
(`config/credentials/development.key`, mode 0600, created by `forge new`, or
the `FORGE_MASTER_KEY` variable in deployed environments). Like `rails
credentials:edit`:

```sh
forge credentials show
```

```text
{
  "encryption": {
    "active": "[REDACTED]",
    "keys": {
      "k1": "[REDACTED]"
    }
  }
}
```

`show` prints the structure with every value redacted (`--reveal` prints the
values, in an interactive terminal only). `edit` decrypts into a private
temporary file, opens `$VISUAL` or `$EDITOR`, validates the JSON, re-encrypts
atomically and wipes the plaintext. Add a secret of your own, for example
`"payments": { "api_key": "sk_test_example" }` next to `"encryption"`, save and
quit:

```sh
EDITOR=nano forge credentials edit      # any editor; defaults to $VISUAL, then $EDITOR
forge credentials show
```

```text
saved config/credentials/development.json.enc (key 45dedd9339080d0b)
{
  "encryption": {
    ...
  },
  "payments": {
    "api_key": "[REDACTED]"
  }
}
```

Create another environment's pair, and rotate its key when someone who knew it
leaves. The new key list is written to the key file and the old keys stay
readable until every deployment has the new file (`--keep-old`):

```sh
forge credentials init --environment production
forge credentials rotate-key --environment production --keep-old
```

```text
  create  config/credentials/production.key
  create  config/credentials/production.json.enc
  ignore  /config/credentials/*.key in .gitignore and .dockerignore

The key in config/credentials/production.key is the only way to read config/credentials/production.json.enc.
Back it up in a password manager or secret store; it is never committed.
Deploy with:  FORGE_MASTER_KEY=$(cat config/credentials/production.key)   (see docs/credentials.md)
Next:         forge credentials edit --environment production
re-encrypted config/credentials/production.json.enc with key 4e6c85ab78a2934f; the previous key list stays readable (1 key(s)); drop it once every deployment has the new file
update FORGE_MASTER_KEY wherever it is stored (docs/credentials.md, "Rotating the key")
```

Back up every key: without it the file cannot be read. Application code reads
values with `forge::credentials::open`, and `forge::encryption` encrypts a
column with the key ring that is already in the file
([credentials](credentials.md), [encryption](encryption.md)).

## 12. Observability

Telemetry is on by default and exports nothing. To see metrics without any
extra tooling, start `forge dev` with a private metrics listener (Ctrl-C, then
the commands of section 4 with the variable in front) and ask for them:

```sh
FORGE_TELEMETRY_METRICS_ADDR=127.0.0.1:9464 forge dev
curl -s http://127.0.0.1:3000/v1/status
curl -s http://127.0.0.1:9464/metrics | grep http_server_request_duration_seconds_count
```

```text
... INFO forge::app: metrics listening on GET /metrics address=127.0.0.1:9464
{"service":"blog","status":"ok"}
http_server_request_duration_seconds_count{http_request_method="GET",http_route="/v1/status",http_response_status_class="2xx"} 1
```

Every generated application also has an opt-in stack: an OpenTelemetry
Collector, Prometheus, Tempo (traces), Loki (logs) and Grafana with a starter
dashboard, every port bound to `127.0.0.1`. It needs Docker and runs the
application as a container, replacing `forge dev` while it is up (it builds the
application image, so it needs the release tag; see the note in section 3):

```sh
export GRAFANA_ADMIN_PASSWORD="$(openssl rand -base64 18)"   # required, also for `down`
docker compose -f compose.yaml -f compose.observability.yaml up --build
curl http://127.0.0.1:3000/
```

Open Grafana at <http://127.0.0.1:3001> (user `admin`, the password in
`$GRAFANA_ADMIN_PASSWORD`: `echo "$GRAFANA_ADMIN_PASSWORD"` in the same shell;
3001 because the application has 3000): the dashboard "Forge application",
Explore > Tempo for traces and Explore > Loki for logs. Stop it with
`docker compose -f compose.yaml -f compose.observability.yaml down` (add `-v`
to delete the data). Details, the metric catalogue and what is never
recorded: [observability](observability.md).

## 13. Deploy

`forge deploy` ships your image to servers that have only SSH and Docker,
Kamal-style, with health-gated, zero-downtime switches behind **forge-proxy**
and migrations before the switch. Its `config/deploy.yml` is Forge-Go's format.
These commands need servers and a registry, so this guide runs only the first
two; [deployment](deployment.md#forge-deploy) walks through a fresh VPS.

```sh
forge deploy init
```

```text
wrote config/deploy.yml
wrote .forge/secrets
wrote .gitignore
wrote .dockerignore
next: edit config/deploy.yml and .forge/secrets, then run forge deploy setup
```

`init` never overwrites. It writes a commented `config/deploy.yml` (service,
image, registry, hosts, the `proxy` block with your domain, health check and
`migrate`) and `.forge/secrets` (mode 0600, git- and Docker-ignored), which
names the secrets and where each one comes from. Edit both. The placeholders
are `192.0.2.10` and `app.example.com`; the secrets file maps names to values
or to your local environment (`REGISTRY_PASSWORD=$REGISTRY_PASSWORD`), so
export what it names before every command, prompting for the password so it
never lands in your history or a command line. `forge deploy setup --dry-run`
prints every remote command with secrets redacted and changes nothing (the
generated secrets file also maps `FORGE_DATABASE_URL` and
`FORGE_MIGRATION_DATABASE_URL` from your environment: this terminal still has
them from section 4; set the production values before a real deploy), but it
still asks each host `docker version` over SSH first, so the host must be
reachable with your SSH key and have Docker installed:

```sh
printf 'Registry password: '; read -rs REGISTRY_PASSWORD; echo   # bash or zsh
export REGISTRY_PASSWORD
forge deploy setup --dry-run
unset REGISTRY_PASSWORD
```

```text
192.0.2.10      $ set -C; mkdir -p .forge/blog && cat > .forge/blog/lock.tmp.$$ || { rm -f .forge/blog/lock.tmp.$$; exit 1; }; claim() { if err=$(ln .forge/blog/lock.tmp.$$ .forge/blog/lock/info 2>&1); then return 0; fi; [ -e .forge/blog/lock/info ] || echo "$err" >&2; return 1; }; won=; if mkdir .forge/blog/lock 2>/dev/null; then claim && won=1; elif [ ! -e .forge/blog/lock/info ] && [ -n "$(find .forge/blog/lock -maxdepth 0 -mmin +1 2>/dev/null)" ]; then if claim; then won=1; { echo 'took over a stale lock without a record (an earlier acquire was interrupted)' >&2; true; }; fi; fi; rm -f .forge/blog/lock.tmp.$$; [ -n "$won" ] && exit 0; cat .forge/blog/lock/info 2>/dev/null; exit 75
192.0.2.10      < stdin: lock record (holder, time, message)
192.0.2.10      # lock acquired (setup)
192.0.2.10      ? docker version --format '{{.Server.Version}}'
192.0.2.10      # docker 27.3.1
192.0.2.10      $ mkdir -p .forge/blog/env && chmod 700 .forge .forge/blog .forge/blog/env
192.0.2.10      $ docker network inspect forge >/dev/null 2>&1 || docker network create forge
192.0.2.10      $ docker login registry.example.com --username acme --password-stdin
192.0.2.10      < stdin: registry password (REGISTRY_PASSWORD)
192.0.2.10      $ docker container inspect forge-proxy >/dev/null 2>&1 && docker start forge-proxy >/dev/null || docker run --detach --name forge-proxy --network forge --restart unless-stopped --publish 80:8080 --publish 443:8443 ...
192.0.2.10      # forge-proxy running
192.0.2.10      $ record=$(cat) && if [ ! -d .forge/blog/lock ]; then echo 'lock is already released' >&2; exit 77; elif [ -n "$record" ] && [ "$(cat .forge/blog/lock/info 2>/dev/null)" = "$record" ]; then rm -rf .forge/blog/lock; else echo 'lock is held by someone else; left in place' >&2; exit 76; fi
192.0.2.10      < stdin: own lock record (release only if unchanged)
warning: proxy.image registry.example.com/acme/forge-proxy:0.1.0 is not pinned by digest; pin it (image@sha256:...) so every host runs the reviewed build
dry run: nothing was changed (20ms)
```

(Against an unreachable host the same command stops with `error: 192.0.2.10:
docker is not available: ... ssh failed: ssh: connect to host 192.0.2.10 port
22: No route to host`. The sample above is from a host whose `docker version`
answered, with the `docker` and `ssh` calls routed to a local stand-in because
no Docker daemon was available; the planned commands are the CLI's own.)

With real servers the sequence is:

```sh
forge deploy setup                     # docker check, network, forge-proxy (80/443, Let's Encrypt)
forge deploy                           # build, push, migrate, switch on every host
forge deploy details                   # versions per host and role
forge logs -f --grep ERROR             # logs from every host, prefixed host|
forge app exec migrate status          # a one-off container of the deployed image
forge deploy rollback 3f2a9c1d07be     # back to a retained version
forge deploy maintenance on            # serve the 503 page; `off` restores the app
```

A deploy migrates before any switch, keeps the previous containers until every
host passes its health check, and routes everything back if one fails (exit
code 4 means it reverted everywhere). Before the first one: build and push the
proxy image once from a Forge checkout (`docker build -f
crates/forge-proxy/Dockerfile -t REGISTRY/forge-proxy:TAG .`, then `docker
push`; once a release tag exists you can pull and `cosign verify` the
signed `ghcr.io/rocketinsidecorp/forge-proxy` image instead, see [supply
chain](supply-chain.md)) and pin its digest in `config/deploy.yml`; set `FORGE_AUTH_MFA_KEY`,
`FORGE_STREAMS_KEY` (if you broadcast) and your mail settings as secrets; and
read the production notes in [deployment](deployment.md). The application
image itself is built from the generated `Dockerfile` (`docker build -t blog
.`), which needs a framework dependency that resolves inside the build context
(the release tag, or the vendored copy described in the "Before the first tag"
note of the [README](../README.md#traces-metrics-and-logs)).

## 14. Undo with `forge destroy`

`forge destroy GENERATOR ARGUMENTS...` takes the arguments you generated with
and removes exactly the files the generator wrote, but **only while they are
unchanged**. A generated mailer you did not edit can go:

```sh
forge destroy mailer user welcome:name,url --dry-run
```

```text
  would remove src/adapters/email/user_mailer.rs
  would remove templates/mailers/user/welcome.subject.txt
  would remove templates/mailers/user/welcome.txt
  would remove templates/mailers/user/welcome.html
  would remove tests/user_mailer.rs

Forge did not edit any existing file. Take out the wiring you added by hand, which the generator printed as:
...
```

You edited the job, so it refuses:

```sh
forge destroy job send_welcome user_id:uuid 'note:string?' --queue --dry-run
```

```text
error: refusing to destroy: 2 file(s) differ from what this generator would write from these arguments (edited since, generated with other arguments, or written by another generator), so nothing was removed:
  src/adapters/jobs/send_welcome.rs
  tests/send_welcome_job.rs
Review them, or pass --force to remove them anyway
```

Without `--dry-run` it removes the files, and with `--force` even edited ones.
Migrations are history: they stay unless you roll them back first
(`forge rollback`) and then pass `--migrations`. Destroy never edits
`src/bootstrap/mod.rs` (it prints the wiring to remove), never removes shared
files and removes directories only when they become empty. It supports
`model`, `scaffold`, `controller`, `mailer` and `job`.

## 15. Where to go next

- [README](../README.md): the full command cheat sheet, deploy, telemetry and the jobs reference.
- [Generators](generators.md): every generator, `--api` controllers, permissions, destroy.
- [Views](views.md): layouts, partials, Hotwire, live updates with `--broadcast`.
- [Database](database.md): the `forge db` lifecycle, transactions, repositories, MySQL, MariaDB and SQLite.
- [MFA](mfa.md), [architecture](architecture.md) and the [threat model](threat-model.md).
- [i18n](i18n.md), [mail](mail.md), [jobs](jobs.md), [OpenAPI](openapi.md).
- [Credentials](credentials.md), [encryption](encryption.md), [rate limiting](rate-limiting.md), [caching](caching.md), [storage](storage.md).
- [Observability](observability.md), [deployment](deployment.md) and the [supply chain](supply-chain.md) (fuzzing, SBOMs, signed images).
- [CLI reference](cli.md) and the [ADRs](adr).
- [Rails parity](rails-parity.md): what Forge has and lacks relative to Rails.
