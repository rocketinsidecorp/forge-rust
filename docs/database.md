# Databases

Forge supports PostgreSQL, MySQL 8.4+ / MariaDB 11.4+ and SQLite behind
additive Cargo features, following
[ADR 0021](adr/0021-selectable-database-backends.md). PostgreSQL is the default
and the only backend with database-enforced tenant isolation (row-level
security). Backends are never emulated: a capability a backend cannot provide is
not offered on it.

## Choosing a backend

| | PostgreSQL | MySQL 8.4+ / MariaDB 11.4+ | SQLite |
| --- | --- | --- | --- |
| Cargo feature (`forge`, `forge-db`) | `postgres` (default in `forge`) | `mysql` | `sqlite` |
| Declaration | `App::postgres(migrator)` | `App::mysql(migrator)` | `App::sqlite(migrator)` |
| `FORGE_DATABASE_URL` schemes | `postgres://`, `postgresql://` | `mysql://`, `mariadb://` | `sqlite:` |
| Pool type | `forge::db::PgDatabase` | `forge::db::MySqlDatabase` | `forge::db::SqliteDatabase` |
| Composition accessor | `Composition::postgres()` | `Composition::mysql()` | `Composition::sqlite()` |
| UUIDv7 column type | `uuid` | `BINARY(16)` | `BLOB` |
| Row-level security | yes | no | no |
| Transaction-scoped advisory lock | `DbTransaction::advisory_xact_lock` | no | no (single writer) |
| Migration lock | session advisory lock | `GET_LOCK` named lock | none needed (single writer) |
| DDL inside migrations | transactional | implicit commits (a failed migration leaves it *dirty*) | transactional |

MySQL and MariaDB speak one protocol and share one backend: `App::mysql`
accepts `mysql://` and `mariadb://` URLs and runs the same migrations
(`BINARY(16)` ids) against MySQL 8.4+ and MariaDB 11.4+.

```toml
# Cargo.toml of an application on SQLite only
forge = { path = "../forge/crates/forge", default-features = false, features = ["sqlite"] }
```

Features are additive: enabling several compiles several drivers, but an
application declares exactly one (`App::postgres` and `App::mysql` together exit
with code 1 and a message naming both).

## Application wiring

```rust
use forge::{App, Composition};

fn main() -> std::process::ExitCode {
    App::new("notes")
        .postgres(forge::db::sqlx::migrate!())
        .compose(|composition: &mut Composition<'_>| {
            let database = composition.postgres()?.clone();
            // build adapters around `database`, register routes ...
            let _ = database;
            Ok(())
        })
        .run()
}
```

Declaring a backend:

- makes `FORGE_DATABASE_URL` mandatory for `serve` and checks its scheme;
  a URL for another engine, or a missing URL, exits with code `78` (the message
  names the expected schemes and never prints the URL);
- creates a lazily connecting pool (the process starts while the database is
  down) and hands it to `App::compose` hooks through the typed accessor, which
  fails with `ComposeError::DatabaseNotDeclared` for any other backend;
- makes `GET /health/ready` ping the database: `200 {"status":"ready"}` or
  `503 {"status":"unavailable","checks":{"database":"unavailable"}}`. Liveness
  (`/health`, `/health/live`) never depends on the database;
- closes the pool, bounded, after the server has drained;
- runs `migrate` and `rollback` with `FORGE_MIGRATION_DATABASE_URL` (same
  scheme rules; it is never derived from the runtime URL, ADR 0011);
- on PostgreSQL, outside development and test, refuses to start `serve`,
  `jobs work` and the `jobs` operator commands when `FORGE_DATABASE_URL`'s
  role could bypass row-level security ([Database roles](#database-roles)).

Applications generated before `forge new -d` keep working: without a
declaration `FORGE_DATABASE_URL` is not interpreted and the handlers registered
with `App::migrations` run for `migrate`/`rollback`. A declaration takes
precedence over `App::migrations`.

## Database roles

Row-level security is defense in depth only when the role the application
serves with cannot bypass it. A PostgreSQL application therefore uses two
roles ([ADR 0011](adr/0011-postgresql-runtime-migration-roles-and-rls-context.md),
[ADR 0044](adr/0044-refused-row-level-security-bypass-and-runtime-role-tests.md)):

| Role | Variable | Attributes and privileges |
| --- | --- | --- |
| runtime (`app_runtime` in development) | `FORGE_DATABASE_URL` | `NOSUPERUSER NOBYPASSRLS NOCREATEDB NOCREATEROLE`; owns no table or schema; `SELECT, INSERT, UPDATE, DELETE` on the tables and `USAGE, SELECT, UPDATE` on the sequences, no temporary tables. `FORCE ROW LEVEL SECURITY` policies apply to it and it cannot run DDL. |
| migration (`app_migrator` in development) | `FORGE_MIGRATION_DATABASE_URL` | owns the database and the `public` schema and applies the migrations, so it owns every table; `NOSUPERUSER NOBYPASSRLS`. Used by `migrate`, `rollback`, `setup`, `seed` and `forge db`, never by the serving process. |

`forge new` writes `docker/postgres/init.sql`, which the Compose `db` service
runs once to create both roles and the database (development-only
passwords).

**Refusal outside development and test.** In every environment except
`development` and `test` (`FORGE_ENV=production`), `serve`, `jobs work` and
the `jobs` operator commands check the role `FORGE_DATABASE_URL` logs in as
(`session_user`) and the role in effect (`current_user`) before using the
pool (`PgDatabase::require_row_security`), and exit with status 1 when either

- is a superuser or has `BYPASSRLS`;
- has `CREATEROLE` (before PostgreSQL 16 it can grant itself membership in a
  `BYPASSRLS` role);
- is a member, directly or through other roles, of a superuser or
  `BYPASSRLS` role (it could `SET ROLE` to it);

or when the two differ: a role setting (`?options=-c%20role%3D...`,
`ALTER ROLE ... SET role`) makes the session act as another role, and
`SET ROLE NONE` turns it back into the login role, so the URL must log in as
the serving role itself. The query runs in a `READ ONLY` transaction with
`SET LOCAL search_path = pg_catalog` and qualified catalog names, so a view
or operator earlier on a `search_path` setting cannot hide anything, and any
failure to run it (an unreachable server included) refuses too. The error
(`DbError::RowSecurityBypass`, code `row_security_bypass`) names the role and
the fix, never the URL:

```text
the database role bypasses row-level security: refusing to serve outside
development and test as PostgreSQL role "postgres", which is a superuser; set
FORGE_DATABASE_URL to log in directly as a NOSUPERUSER NOBYPASSRLS
NOCREATEROLE runtime role with only data privileges and migrate with
FORGE_MIGRATION_DATABASE_URL (docs/database.md, "Database roles")
```

There is no override variable. Development and test are not checked, so
local tools may connect as an administrator; `routes` and `openapi` do not
connect. The check does not refuse a role that owns tables: `FORCE` still
applies to an owner, but an owner can switch it off, so keep the serving role
separate from the migration role. MySQL, MariaDB and SQLite have no
row-level security and nothing is checked.

**Provisioning in production.** Create the equivalent of `init.sql` once, as
an administrator, then migrate with the migration role and serve with the
runtime role only:

```sql
CREATE ROLE shop_migrator LOGIN PASSWORD '...' NOSUPERUSER NOCREATEROLE NOBYPASSRLS;
CREATE ROLE shop_runtime  LOGIN PASSWORD '...' NOSUPERUSER NOCREATEDB NOCREATEROLE NOBYPASSRLS;
CREATE DATABASE shop OWNER shop_migrator;
-- connected to shop:
REVOKE CONNECT, TEMPORARY ON DATABASE shop FROM PUBLIC;
GRANT CONNECT ON DATABASE shop TO shop_migrator, shop_runtime;
REVOKE ALL ON SCHEMA public FROM PUBLIC;
GRANT ALL ON SCHEMA public TO shop_migrator;
GRANT USAGE ON SCHEMA public TO shop_runtime;
ALTER DEFAULT PRIVILEGES FOR ROLE shop_migrator IN SCHEMA public
    GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO shop_runtime;
ALTER DEFAULT PRIVILEGES FOR ROLE shop_migrator IN SCHEMA public
    GRANT USAGE, SELECT, UPDATE ON SEQUENCES TO shop_runtime;
ALTER DATABASE shop SET forge.runtime_role = 'shop_runtime';
```

`forge.runtime_role` names the runtime role for the migrations that grant it
explicitly (the job queue and outbox; the generated identity and scaffold
migrations grant `app_runtime` by name, so name the production runtime role
`app_runtime` or grant those tables' privileges yourself).

```sh
FORGE_ENV=production FORGE_MIGRATION_DATABASE_URL='postgres://shop_migrator:PASSWORD@db.internal:5432/shop?sslmode=verify-full' cargo run -- migrate
FORGE_ENV=production FORGE_DATABASE_URL='postgres://shop_runtime:PASSWORD@db.internal:5432/shop?sslmode=verify-full' cargo run
```

## `forge-db`

`Database<B: Backend>` is the handle for one backend (`PgDatabase`,
`MySqlDatabase`, `SqliteDatabase`). `Backend` is sealed and implemented for
`sqlx::Postgres`, `sqlx::MySql` and `sqlx::Sqlite`.

| API | Notes |
| --- | --- |
| `connect(&SecretString)`, `connect_with(url, PoolSettings)` | eager; an unreachable server is the retryable `DbError::Unavailable` after the acquire timeout |
| `connect_lazy(..)`, `connect_lazy_with(..)` | no I/O until first use; call inside a Tokio runtime |
| `pool()`, `scheme()`, `ping()`, `close()` | `Database` is `Clone` (shared pool) and its `Debug` never contains the URL |
| `begin()` | `DbTransaction<B>`; query with `.execute(tx.connection())`; dropping rolls back |
| `migrate(&Migrator) -> Vec<i64>` | versions applied, ascending |
| `rollback(&Migrator) -> Option<i64>` | reverts the latest applied migration; `IrreversibleMigration` without a down migration |
| `require_row_security(Environment)` (PostgreSQL) | outside development and test, `DbError::RowSecurityBypass` when the pool's role could bypass row-level security ([Database roles](#database-roles)); the `forge::App` lifecycle calls it |
| `expect_rows(result, n)` | `DbError::Conflict` unless exactly `n` rows were affected; accepts every backend's query result (optimistic locking) |

Defaults: 10 connections, 5 second acquire timeout, no statement timeout
(`PoolSettings`). `PoolSettings::with_statement_timeout` bounds every
statement on the server, set on each new connection; `PoolSettings::from_config`
is the serving pool of the configuration (below).

### Connection pool and TLS

`serve` and `jobs work` size their pool from Forge-Go's keys
([deployment](deployment.md#configuration)):

| Key | Default | Effect |
| --- | --- | --- |
| `FORGE_DATABASE_MAX_CONNS` | `10` | Pool size, `1` to `1000`; in-serve jobs share it. |
| `FORGE_DATABASE_CONNECT_TIMEOUT` | `5s` | The acquire timeout: waiting for a free connection and opening one. |
| `FORGE_DATABASE_STATEMENT_TIMEOUT` | `30s` | `SET statement_timeout` (PostgreSQL), `SET SESSION max_statement_time` (MariaDB, every statement), `SET SESSION max_execution_time` (MySQL, read-only `SELECT` only) on each new connection; nothing on SQLite. `1ms` to `596h`. |
| `FORGE_DATABASE_ALLOW_PLAINTEXT` | `false` | Lifts the production TLS rule. |

Migrations, `setup`, `seed` and the `db` commands connect with the default
`PoolSettings`, without the statement timeout, so long schema changes are not
cancelled. In production both URLs must verify the server certificate
(`sslmode=verify-full`, `ssl-mode=VERIFY_IDENTITY`, a Unix
socket, or SQLite) unless `FORGE_DATABASE_ALLOW_PLAINTEXT=true`
([database TLS](deployment.md#database-tls)).

```sh
export FORGE_DATABASE_URL='postgres://app_runtime:PASSWORD@db.internal:5432/shop?sslmode=verify-full'
export FORGE_DATABASE_MAX_CONNS=20 FORGE_DATABASE_CONNECT_TIMEOUT=2s FORGE_DATABASE_STATEMENT_TIMEOUT=10s
cargo run
```

A URL whose scheme does not belong to the chosen backend fails with
`DbError::Configuration`, naming the expected schemes and never the URL.

`Database<B>` also implements the backend-neutral `TransactionManager` contract
and `DbTransaction<B>` the `Transaction` contract; `DbError` converts into
`DatabaseError` (contract) and `ForgeError`.

### Identifiers and versions

`forge_core::Id<T>` and `forge_core::Version` implement SQLx `Type`, `Encode`
and `Decode` for every enabled driver by delegating to `Uuid` and `i64`
(feature `sqlx`; feature `postgres` adds the array types). Ids are generated in
Rust (`Id::<T>::new()`); databases never generate UUIDs. Decoding re-validates:
a non-v7 UUID or a version below 1 is a decode error on every backend.

### Error classification

`DbError` classifies by the driver's portable kind plus backend codes:

| Variant | PostgreSQL | MySQL / MariaDB | SQLite |
| --- | --- | --- | --- |
| `UniqueViolation` | `23505` | `1062` family | `SQLITE_CONSTRAINT_UNIQUE`/`_PRIMARYKEY` |
| `ForeignKeyViolation` | `23503` | `1451`, `1452` | `SQLITE_CONSTRAINT_FOREIGNKEY` |
| `CheckViolation` | `23514` | `3819` (MySQL), `4025` (MariaDB) | `SQLITE_CONSTRAINT_CHECK` |
| `NotNullViolation` | `23502` | `1048`, `1364` | `SQLITE_CONSTRAINT_NOTNULL` |
| `SerializationFailure` (retryable) | `40001`, `40P01` | `1213` deadlock, `1205` lock wait timeout | `SQLITE_BUSY`, `SQLITE_LOCKED` |
| `Unavailable` (retryable) | class `08`, `57P01`-`57P03`, `53300` | class `08`, `1040`, `1053`, `1927`, `4031`, I/O, TLS | `SQLITE_IOERR`, `SQLITE_CANTOPEN`, `SQLITE_FULL`, `SQLITE_READONLY` |

`Display` never contains SQL, row values or credentials. `constraint()` returns
the constraint name when known: PostgreSQL reports it; MySQL/MariaDB and SQLite
`CHECK` names are recovered from the driver message and accepted only if they
look like an identifier; SQLite reports no name for unique/foreign-key
violations.

### PostgreSQL only

- `advisory_key(name)` and `DbTransaction::advisory_xact_lock(key)`
  (`pg_advisory_xact_lock`, released with the transaction so it cannot leak
  through the pool).
- Row-level security and role context (`set_config(.., true)`), ADR 0004 and
  ADR 0011. On MySQL/MariaDB and SQLite, tenant isolation rests on the
  application layer; use PostgreSQL where database-enforced isolation is a
  requirement.

### SQLite

Every connection enables foreign keys, uses WAL (not for in-memory or
`mode=ro` URLs) and waits up to 5 seconds for a lock before failing with a
retryable busy error. `sqlite:path` does not create a missing file; add
`?mode=rwc` for that. `sqlite::memory:` pins a single connection (a memory
database lives only while a connection holds it).

## Testing

Feature `testing` (`forge` forwards it to `forge-db`) adds
`forge::db::testing::TestDatabase<B>`: an isolated, migrated database per test.

| Backend | Administrative URL | Isolation |
| --- | --- | --- |
| PostgreSQL | `FORGE_TEST_DATABASE_URL` | database `forge_test_<uuidv7>` |
| MySQL | `FORGE_TEST_MYSQL_URL` | database `forge_test_<uuidv7>` |
| MariaDB | `FORGE_TEST_MARIADB_URL` via `create_from_env(MARIADB_URL_ENV, ..)` | database `forge_test_<uuidv7>` |
| SQLite | none | file in a unique temporary directory (always available) |

The database is removed when the value is dropped, including when the test body
panics (the guard drops a server database from a helper thread with its own
runtime); `cleanup().await` does it asynchronously and reports failures.

`forge::db::testing::RlsTestDatabase` (PostgreSQL, Forge-Go's
`postgrestest.NewRLS`) adds the roles of a deployment, so tests run as the
role an application serves with and row-level security applies to them as in
production. `RlsTestDatabase::create([&MIGRATOR])` provisions
`forge_test_<uuidv7>` owned by a fresh `forge_test_migrator_<suffix>` role,
applies the migrations as that role, and connects a fresh
`forge_test_runtime_<suffix>` role (`NOSUPERUSER NOBYPASSRLS`, owns nothing,
the default privileges of `init.sql`, no temporary tables, named in
`forge.runtime_role`); `create_with_worker` adds a worker role named in
`forge.worker_role`. `admin()`, `migrator()`, `runtime()` and `worker()` are
the pools, `runtime_url()` the runtime role's URL for code that opens its
own pool. The database and the roles are dropped afterwards, also on panic.
`FORGE_TEST_DATABASE_URL` must be a superuser (as in the generated Compose
service and CI). Generated repository tests use it:

```sh
FORGE_TEST_DATABASE_URL=postgres://postgres:postgres@127.0.0.1:5432/postgres \
  cargo test -- --include-ignored   # repository, RLS and DDL tests as a fresh runtime role
```
`Composition::for_testing(..).with_postgres(db)` exercises a composition
function without the process lifecycle.

The `forge-db` suite is one set of assertions instantiated per backend:

```sh
docker run -d --rm --name pg      -e POSTGRES_PASSWORD=forge -p 127.0.0.1:55461:5432 postgres:18
docker run -d --rm --name mysql   -e MYSQL_ROOT_PASSWORD=forge -p 127.0.0.1:55462:3306 mysql:8.4
docker run -d --rm --name mariadb -e MARIADB_ROOT_PASSWORD=forge -p 127.0.0.1:55463:3306 mariadb:11.8

FORGE_TEST_DATABASE_URL=postgres://postgres:forge@127.0.0.1:55461/postgres \
FORGE_TEST_MYSQL_URL=mysql://root:forge@127.0.0.1:55462/mysql \
FORGE_TEST_MARIADB_URL=mariadb://root:forge@127.0.0.1:55463/mysql \
  cargo test -p forge-db -- --include-ignored
```

SQLite runs on every `cargo test`; server backends are `#[ignore]`d unless
their variable is set and `--include-ignored` is passed.

## MySQL and MariaDB differences

Verified against MySQL 8.4 and MariaDB 11.8 with the shared suite; the same
migrations and the same assertions pass on both:

- check-constraint failures use different codes (`3819` vs `4025`) and messages;
  both classify as `CheckViolation` with the constraint name;
- `4025` means something else on MySQL (different SQLSTATE) and is only treated
  as a check violation with SQLSTATE `23000`;
- duplicate-key messages name `table.index` on MySQL 8 and `index` on MariaDB;
  both yield the index name;
- deadlock (`1213`) and lock wait timeout (`1205`) are identical.

## Migrations shipped by Forge crates

`forge-ratelimit` and `forge-cache` embed reversible migrations per backend
(`forge_ratelimit::postgres::MIGRATIONS`, `forge_cache::sqlite::MIGRATIONS`, ...)
with timestamp versions (`20261019000000`, `20261019000100`). They share the
application's `_sqlx_migrations` table, so merge them with the application's own
and declare the result: `forge::db::combine_migrations(&[&APP, &forge::cache::postgres::MIGRATIONS])`
returns one `Migrator` for `App::postgres(..)`; duplicate versions are refused.

These versions are listed in `forge_db::FRAMEWORK_MIGRATIONS` and owned by the
framework ([ADR 0030](adr/0030-framework-migration-ownership.md)):

- `Database::rollback` (the application's `rollback` command) reverts the
  application's newest applied migration, never a framework one, even when a
  framework version is higher (for example while the generated `1..3` are the
  only application migrations). With only
  framework migrations left it fails with `DbError::FrameworkMigrationOnly`
  and reverts nothing.
- `Database::rollback_framework(&migrations)` is the explicit way to revert the
  newest framework migration the migrator contains.
- Removing a set from `combine_migrations` (the feature is turned off) keeps
  `migrate` working: its applied version is recognized and left alone. A
  version that no set and no framework crate owns still fails with
  `VersionMissing`.
- An application migration may not reuse a listed version.
