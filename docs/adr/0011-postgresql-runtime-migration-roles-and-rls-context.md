# ADR 0011: Separate PostgreSQL migration/runtime roles and transaction-local RLS context

- Status: Accepted
- Date: 2026-09-27
- Refines: [ADR 0004](0004-tenancy-and-rls.md), [ADR 0007](0007-sqlx-postgresql-first.md)
- Refined by: [ADR 0044](0044-refused-row-level-security-bypass-and-runtime-role-tests.md)

## Context

PostgreSQL RLS is only useful as defense in depth when the serving application
cannot bypass policies and does not normally hold schema-owner credentials.
Using one connection URL for migrations and request handling gives the runtime
more database authority than it needs.

Pooled connections also make session-scoped tenant state dangerous. A tenant or
principal value that survives transaction completion can leak into unrelated
requests.

## Decision

Database-enabled Forge applications use separate PostgreSQL credentials:

- the migration role owns schema changes and is used only by migrate/rollback;
- the runtime role has only data privileges required by the application;
- the runtime role is not superuser, database/schema owner, role creator, or
  BYPASSRLS;
- the serving process receives only the runtime credential.

Forge configuration therefore distinguishes FORGE_DATABASE_URL from
FORGE_MIGRATION_DATABASE_URL. Database commands require the migration URL and do
not fall back to the runtime URL.

Generated PostgreSQL infrastructure starts tenant-sensitive work in an explicit
transaction and installs the authorized tenant and principal with:

    set_config('forge.tenant_id', ..., true)
    set_config('forge.principal_id', ..., true)

The true third argument makes both settings transaction-local. RLS policies read
them with current_setting(..., true) and fail closed when a setting is absent.

Generated local Compose bootstrap creates distinct app_migrator and app_runtime
roles. These credentials are development-only; production deployments provision
equivalent least-privilege roles through their database/security platform.

Administrative cross-tenant access is not granted to the runtime role. It
requires a later explicit capability, separate role, and audit path.

## Consequences

- A compromised application process does not automatically gain migration/DDL
  authority.
- Runtime connections cannot bypass RLS through ordinary role privileges.
- Tenant/principal context disappears at transaction end and does not persist on
  pooled connections.
- Migration credentials must be kept out of serving containers/processes.
- Migrations must grant the runtime role the required table/sequence privileges.
- Every tenant table needs an explicit RLS policy and negative two-tenant tests.
- Operators must manage two credential classes instead of one.
