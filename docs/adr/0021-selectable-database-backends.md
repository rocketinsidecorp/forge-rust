# ADR 0021: Selectable database backends with `forge new -d`

- Status: Accepted
- Date: 2026-10-14
- Refines: [ADR 0004](0004-tenancy-and-rls.md) and [ADR 0007](0007-sqlx-postgresql-first.md)
- Refined by: [ADR 0024](0024-scaffold-generator.md)
- Decision owner: project owner (2026-10-14: "siga igual o Rails … opção de multi banco")

## Context

Rails lets a team pick the database when the application is created
(`rails new app -d postgresql|mysql|trilogy|sqlite3`) and generates matching
configuration, containers and migrations. Forge was PostgreSQL-only. Teams that
standardize on MySQL/MariaDB, or that want SQLite's zero-infrastructure
single-node deployment (the Rails 8 default), could not adopt Forge.

SQLx, the adapter chosen in ADR 0007, supports PostgreSQL, MySQL/MariaDB and
SQLite. Several Forge security properties are PostgreSQL features, not SQLx
features: row-level security (ADR 0004), separate migration/runtime roles
(ADR 0011), transaction-scoped advisory locks and `LISTEN/NOTIFY`.

## Decision

1. `forge new <app> -d <backend>` (`--database`) selects the backend:
   `postgresql` (alias `postgres`), `mysql` (MySQL 8.4+), `mariadb`
   (MariaDB 11.4+ LTS), `sqlite3` (alias `sqlite`), or `none`. MariaDB is a
   first-class choice with its own container, CI matrix entry and integration
   tests; it shares the framework's MySQL-protocol driver. **PostgreSQL stays the
   default**: Forge is PostgreSQL-first and RLS is its tenant-isolation defense
   in depth. Rails 8 defaults to SQLite; Forge deliberately does not.
2. The generator renders backend-specific Cargo features, `FORGE_DATABASE_URL`
   examples, Compose services (`postgres:18`, `mysql:8.4` or `mariadb:11.8`
   with health checks and a named volume; SQLite uses a volume owned by the runtime uid at
   `/data`), Docker image settings, CI smoke tests and migrations.
3. The framework exposes one generic database contract: `forge-db` has a sealed
   `Backend` trait implemented for each SQLx driver behind additive features
   (`postgres`, `mysql`, `sqlite`), `Database<B>` with the aliases
   `PgDatabase`, `MySqlDatabase` (MySQL and MariaDB, `mysql://` or
   `mariadb://` URLs) and `SqliteDatabase`, and backend-independent
   migrate/rollback/readiness/error classification. The facade declares the
   backend with `App::postgres(..)`, `App::mysql(..)` or `App::sqlite(..)`;
   declaring two, or a `FORGE_DATABASE_URL` whose scheme does not match the
   declared backend, fails startup (exit 78).
4. UUIDv7 remains the identifier on every backend. IDs are generated in Rust
   (`Id::<T>::new()`); databases never generate UUIDs (`gen_random_uuid()` is
   v4 and is forbidden in generated SQL). Column types: PostgreSQL `uuid` (with
   `CHECK (uuid_extract_version(id) = 7)` on PostgreSQL 17+), MySQL and
   MariaDB `BINARY(16)` (identical migrations; MariaDB's native `UUID` type is
   not used so both share one schema), SQLite `BLOB`. Decoding a non-v7 value is an error everywhere.
5. Capability matrix. A capability a backend cannot provide is refused
   explicitly by the generator and at startup; it is never emulated silently.

   | Capability | PostgreSQL | MySQL | MariaDB | SQLite |
   | --- | --- | --- | --- | --- |
   | Mandatory `TenantContext` in tenant-sensitive ports | yes | yes | yes | yes |
   | Row-level security (defense in depth) | yes | no | no | no |
   | Separate migrator/runtime roles | yes | yes (grants) | yes (grants) | no (file permissions) |
   | Transaction-scoped advisory lock | `pg_advisory_xact_lock` | named lock (`GET_LOCK`), session-scoped | named lock (`GET_LOCK`), session-scoped | single writer |
   | Job claiming (Phase 4) | `FOR UPDATE SKIP LOCKED` | `FOR UPDATE SKIP LOCKED` | `FOR UPDATE SKIP LOCKED` (10.6+) | polling with `BEGIN IMMEDIATE` |
   | Pub/sub (Phase 4) | `LISTEN/NOTIFY` | polling table | polling table | polling table |

6. Without RLS, tenant isolation on MySQL, MariaDB and SQLite rests on the
   application layer: generated repositories always bind the tenant predicate
   from `TenantContext`, and the shared two-tenant isolation suite runs against
   every backend. Documentation and `forge capabilities` state this difference;
   applications with strict multi-tenant isolation requirements should use
   PostgreSQL.
7. Generated identity, session, membership, audit and login-throttle
   persistence must exist for every supported backend with the same contracts
   and tests. Until a backend has full parity, `forge new -d <backend>` reports
   the missing modules explicitly instead of generating PostgreSQL SQL.
8. CI runs the generated-application end-to-end suite and the database
   integration suites for each backend, MariaDB included (matrix).

## Consequences

- Teams can adopt Forge on the database they already operate, and small
  deployments can run as a single binary plus a SQLite file.
- The generator, migrations and persistence templates multiply per backend
  (MySQL and MariaDB share SQL); the
  matrix CI and the shared contract suites are what keep them equivalent.
- MySQL, MariaDB and SQLite applications lose database-enforced tenant
  isolation. This is a documented, visible trade-off, not a silent downgrade.
- PostgreSQL-specific features (RLS, roles, `LISTEN/NOTIFY`, JSONB operators)
  stay first-class and are not reduced to a lowest common denominator.
