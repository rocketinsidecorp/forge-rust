# ADR 0030: Framework migration ownership in the application's migration table

- Status: Accepted
- Date: 2026-10-20
- Refines: [ADR 0027](0027-rate-limiting.md), [ADR 0028](0028-cache.md) (migrations)
- Issue: rocketinsidecorp/forge-rust#10, item 1

## Context

`forge-ratelimit` and `forge-cache` ship reversible migrations
(`20261019000000_rate_limits`, `20261019000100_cache`, per backend) that the
application merges into its own migrator with `forge_db::combine_migrations`,
so they are recorded in the application's `_sqlx_migrations` table. The
migration runner treated the merged set as one: `rollback` reverted the
highest applied version. A generated application's own migrations are
`1..3` (and `forge generate migration` versions after them), so its first
`rollback` dropped `forge_rate_limits` or `forge_cache_entries` instead of the
application's newest migration. Turning a store off later (removing its set
from `combine_migrations`) made `migrate` fail with `VersionMissing` for the
framework version already applied.

`forge-jobs` and `forge-events` avoid both problems with their own bookkeeping
tables (`_forge_jobs_migrations`, `_forge_events_migrations`) and a separate
migrator that `App::jobs` / `App::events` apply before the application's.

## Decision

Keep the shared table for the two released sets and decide ownership by
version:

- `forge_db::FRAMEWORK_MIGRATIONS` lists every migration a Forge crate records
  in the application's table: version, SQLx description and owning crate.
  Each crate tests that every shipped migration, on every backend, is listed
  under its name.
- **Rollback scope.** `Database::rollback` (the application `rollback`
  command) reverts the newest applied migration that is *not* listed, even
  when a framework version is higher. When only framework migrations remain it
  fails with `DbError::FrameworkMigrationOnly` and reverts nothing; when
  nothing is applied it still returns `None`.
- **Explicit framework revert.** `Database::rollback_framework(&migrator)`
  reverts the newest applied framework migration the migrator contains and
  never an application migration. No CLI command exposes it: dropping a
  framework table is a deliberate operator or test action.
- **Disabled features.** An applied version the migrator does not contain is
  tolerated when it is listed (its feature was turned off); its table and
  bookkeeping row stay, and turning the feature on again finds it applied
  and checksum-verified. Any other unknown version still fails closed with
  `VersionMissing`.
- **Reserved versions.** A migration that uses a listed version under another
  description is refused by `combine_migrations` and by `migrate`/`rollback`
  (`DbError::Configuration`), so ownership is never decided for the wrong
  migration. Generated application versions (`1..3`, nanosecond timestamps)
  cannot collide in practice.
- **New framework schemas** get their own bookkeeping table and migrator, like
  jobs and events, instead of new entries in the list.

### Rejected: moving rate-limit and cache to their own tables now

Released migration history is immutable and existing databases already record
`20261019000000` / `20261019000100` in `_sqlx_migrations`. Moving them would
need a backend-specific transition that rewrites SQLx bookkeeping rows on
PostgreSQL, MySQL/MariaDB and SQLite, a new `App` registration API replacing
the documented `combine_migrations` usage, and the version list anyway (for
databases whose feature was turned off before the transition ran). The
version list fixes both defects with no data migration and no API removal.

## Consequences

- An application `rollback` can no longer drop a framework table, whatever the
  version order; a rollback with no application migration left exits non-zero
  with a message naming the framework migration.
- Turning the rate-limit or cache store off no longer breaks `migrate`; the
  unused table remains until an operator drops it (or turns the feature back
  on and calls `rollback_framework`).
- Reverting a framework migration is explicit library code, not a CLI command.
- `forge-db` carries two framework versions it does not otherwise know about;
  the crates' tests keep the list and the shipped files in sync.
- Forge-Go merges its framework sets (identity, queue, rate limit, cache) into
  one `forge_schema_migrations` table and its `rollback` reverts the most
  recently applied migration of any set, refusing unknown versions; it should
  adopt the same ownership rule (parity follow-up).
