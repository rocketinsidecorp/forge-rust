# ADR 0044: Refused row-level security bypass and runtime-role tests

- Status: Accepted
- Date: 2026-10-26
- Refines: [ADR 0011](0011-postgresql-runtime-migration-roles-and-rls-context.md)
- Parity with: Forge-Go ADR 0046 (PostgreSQL runtime and migration roles,
  refused RLS bypass)

## Context

ADR 0011 separates the migration role from the runtime role and requires the
runtime role to be neither a superuser nor `BYPASSRLS`. Nothing enforced that
rule: an application whose `FORGE_DATABASE_URL` named a superuser served
normally, and every `FORCE ROW LEVEL SECURITY` policy was silently void. The
generated repository tests had the same gap: they ran as the test server's
superuser and switched to a probe role inside one of its transactions, so the
role an application serves with never ran them. Forge-Go closed both gaps in
its QG1 work (its ADR 0046), and its reviews found two bypasses of a naive
check: a URL with `options=-c role=<runtime>` makes `current_user` look
harmless while the login role is a superuser, and a `public.=` operator
earlier on a `search_path` setting can make the check's comparisons lie.

## Decision

1. **Refusal outside development and test.**
   `PgDatabase::require_row_security(environment)` (forge-db) does nothing in
   `development` and `test` and otherwise fails with
   `DbError::RowSecurityBypass` when the login role (`session_user`) or the
   role in effect (`current_user`) is a superuser, has `BYPASSRLS`,
   `CREATEROLE` (before PostgreSQL 16 it can grant itself a `BYPASSRLS`
   membership) or `REPLICATION` (with `wal_level=logical`, logical decoding
   reads every tenant's changes); is a member, directly or through other
   roles, of a superuser, `BYPASSRLS` or `REPLICATION` role
   (`pg_has_role(..., 'MEMBER')`, whatever the grant's `INHERIT` and `SET`
   options); is a member of `pg_execute_server_program`,
   `pg_write_server_files` or `pg_read_server_files` (paths to superuser, or
   to every tenant's data files); or owns, or is a member of the owner of, a
   table of the database with row-level security enabled
   (`pg_catalog.pg_class.relrowsecurity`), because an owner can run `ALTER
   TABLE ... NO FORCE ROW LEVEL SECURITY` and then read every tenant (found
   by the independent review of this ADR's first version; Forge-Go shares
   the gap and ports the rule). It also fails when the two differ: a
   role setting (`options=-c role=...`, `ALTER ROLE ... SET role`) makes the
   session act as another role and `SET ROLE NONE` returns it to the login
   role, so the URL must log in as the serving role itself. The environments
   match Forge-Go's ("every environment except development and test");
   Forge-Rust's `FORGE_ENV` has no `staging`, so today that is `production`.
2. **The query cannot be shadowed.** It runs in a `READ ONLY` transaction
   that first runs `SET LOCAL search_path = pg_catalog`, so operators resolve
   to PostgreSQL's own whatever `search_path` the URL or the role sets, and
   it names `pg_catalog.pg_roles`, `pg_catalog.pg_class` and
   `pg_catalog.pg_has_role` explicitly.
3. **Fail closed.** No row for the login role, rows that disagree on the
   session's roles, or any error (an unreachable server, a failed `SET`)
   refuses. The message names the role and the fix (log in as a
   `NOSUPERUSER NOBYPASSRLS NOCREATEROLE` runtime role, migrate with
   `FORGE_MIGRATION_DATABASE_URL`), never the URL; role names are quoted so
   they cannot forge log lines.
4. **Wired by the bootstrap ring.** The check is an infrastructure method of
   forge-db; the `forge::App` lifecycle, Forge-Rust's bootstrap, calls it
   on the pool as soon as it is created, for `serve` and `jobs work` before
   telemetry starts and before any composition hook receives the pool (as
   Forge-Go checks inside `Open`), and for the `jobs` operator commands
   before they query, and exits with status 1 (closing the pool) when it
   refuses. `routes` and `openapi` only inspect the composition and do not
   connect. MySQL, MariaDB and SQLite have no row-level security, so nothing
   is checked for them. As in Forge-Go there is no override variable.
5. **Tests run as the runtime role.** `forge::db::testing::RlsTestDatabase`
   (Forge-Go's `postgrestest.NewRLS`) creates `forge_test_<uuidv7>`, a
   migration role that owns it and applies the migrations, and a
   `NOSUPERUSER NOBYPASSRLS` runtime role with the default privileges of
   `docker/postgres/init.sql` (data privileges only, no temporary tables),
   named in `forge.runtime_role` so framework migrations grant it; with
   `create_with_worker`, a worker role named in `forge.worker_role`. The
   database and the roles are dropped afterwards, also on panic. It replaces
   the narrower kit forge-jobs had (`forge_jobs::testing::RlsTestDatabase` is
   now a re-export). `forge generate scaffold` writes PostgreSQL repository
   tests that run as that runtime role, a test that the runtime role cannot
   run `CREATE TABLE`, `CREATE TEMPORARY TABLE`, `ALTER TABLE`, `TRUNCATE` or
   `DROP TABLE` (and, for tenant resources, `DISABLE`/`NO FORCE ROW LEVEL
   SECURITY` or `DROP POLICY`), and the row-level security test as that role.
6. **`init.sql` is readable by the database container.** `forge new` makes
   `docker/postgres/init.sql` `0644` under `0755` directories whatever the
   umask, like the observability configuration, because the postgres image
   runs it as its own user.

## Consequences

- A production process configured with an administrator's URL stops at
  startup with a message naming the role, instead of serving with row-level
  security switched off.
- The generated tests prove what production relies on: the policies hold for
  the serving role and that role cannot change the schema.
- The check runs once at startup; a role changed afterwards is not noticed
  until the next start. Ownership is judged on tables with row-level
  security only: a serving role may own a table without it, which no policy
  protects anyway.
- Row-level security stops a query that forgets its tenant predicate, not SQL
  injected into the serving process, which can set `forge.tenant_id` itself.
- Migrations that grant to `app_runtime` by name (the generated identity
  tables narrow its privileges that way) do not reach a per-test runtime
  role, which keeps the default privileges on those tables: tests can be more
  permissive than production there, never stricter.
- Differences from Forge-Go: the generated `init.sql` grants sequences
  `USAGE, SELECT, UPDATE` (Forge-Go `USAGE, SELECT`); Forge-Rust's
  `migrate` never falls back to `FORGE_DATABASE_URL` (ADR 0011); the
  generated sign-in tests still run as the test server's user.
