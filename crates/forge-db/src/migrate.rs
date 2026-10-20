//! Backend-independent migration runner built on SQLx's `Migrate` trait.
//!
//! It mirrors `Migrator::run` / `Migrator::undo` (lock, bookkeeping table,
//! dirty and checksum checks) but reports exactly which versions were applied
//! or reverted, and never leaves a session-scoped migration lock on a pooled
//! connection: after any failure the connection is closed instead of being
//! returned to the pool.
//!
//! Migrations shipped by Forge crates share the application's bookkeeping
//! table (ADR 0030): they are listed in [`FRAMEWORK_MIGRATIONS`], never
//! reverted by the application rollback, and tolerated when their feature is
//! no longer part of the migrator.

use std::collections::HashMap;

use sqlx::{
    Pool,
    migrate::{AppliedMigration, Migrate, MigrateError, Migrator},
    pool::PoolConnection,
};
use tracing::debug;

use crate::{Backend, DbError};

/// A migration that a Forge crate records in the application's migration
/// table, merged in with [`combine_migrations`] (ADR 0030).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct FrameworkMigration {
    /// The version, reserved for this migration on every backend.
    pub version: i64,
    /// The description SQLx derives from the file name (`rate limits` for
    /// `20261019000000_rate_limits.up.sql`).
    pub description: &'static str,
    /// The Forge crate that ships the migration.
    pub owner: &'static str,
}

/// Every migration that Forge crates record in the application's migration
/// table (ADR 0030).
///
/// The versions are reserved: an application migration with one of them and
/// another description is refused. An applied version listed here is
/// framework-owned, so [`Database::rollback`](crate::Database::rollback)
/// never reverts it, and [`Database::migrate`](crate::Database::migrate)
/// tolerates it when the migrator no longer contains it (the feature was
/// turned off). Released entries are immutable; framework schemas added later
/// use their own bookkeeping table, like `forge-jobs` and `forge-events`.
pub const FRAMEWORK_MIGRATIONS: &[FrameworkMigration] = &[
    FrameworkMigration {
        version: 20_261_019_000_000,
        description: "rate limits",
        owner: "forge-ratelimit",
    },
    FrameworkMigration {
        version: 20_261_019_000_100,
        description: "cache",
        owner: "forge-cache",
    },
];

fn framework_migration(version: i64) -> Option<&'static FrameworkMigration> {
    FRAMEWORK_MIGRATIONS
        .iter()
        .find(|migration| migration.version == version)
}

/// Which applied migrations a revert may touch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RevertScope {
    /// The application's own migrations: every version not listed in
    /// [`FRAMEWORK_MIGRATIONS`].
    Application,
    /// The framework migrations the migrator contains.
    Framework,
}

/// Applies pending migrations; returns the versions applied, ascending.
pub(crate) async fn apply_pending<B: Backend>(
    pool: &Pool<B>,
    migrator: &Migrator,
) -> Result<Vec<i64>, DbError> {
    let mut connection = pool.acquire().await?;
    let result = apply_locked(&mut *connection, migrator).await;
    finish(connection, result).await
}

/// Where a migration stands relative to the database.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum MigrationState {
    /// Recorded as applied, and the file is unchanged.
    Applied,
    /// Known to the application, not applied yet.
    Pending,
    /// Applied, but the migration file changed afterwards: `migrate` refuses
    /// to continue until the change is undone.
    Modified,
    /// Applied by a Forge feature the application no longer composes
    /// ([`FRAMEWORK_MIGRATIONS`]); tolerated.
    Disabled,
    /// Applied, but unknown to the application and not a framework
    /// migration: `migrate` fails closed.
    Missing,
}

impl MigrationState {
    /// The lowercase word `migrate status` prints.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Applied => "applied",
            Self::Pending => "pending",
            Self::Modified => "modified",
            Self::Disabled => "disabled",
            Self::Missing => "missing",
        }
    }
}

/// One migration and its state, as [`Database::migration_status`] reports it.
///
/// [`Database::migration_status`]: crate::Database::migration_status
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct MigrationStatus {
    /// The migration version.
    pub version: i64,
    /// The description SQLx derives from the file name (`create posts` for
    /// `20261020000000_create_posts.up.sql`); empty for a version the
    /// application does not know.
    pub description: String,
    /// Where it stands.
    pub state: MigrationState,
}

/// Lists every migration of `migrator` and every applied version, ascending.
pub(crate) async fn status<B: Backend>(
    pool: &Pool<B>,
    migrator: &Migrator,
) -> Result<Vec<MigrationStatus>, DbError> {
    let mut connection = pool.acquire().await?;
    let result = status_of(&mut *connection, migrator).await;
    finish(connection, result).await
}

async fn status_of<C: Migrate>(
    connection: &mut C,
    migrator: &Migrator,
) -> Result<Vec<MigrationStatus>, DbError> {
    check_reserved_versions(migrator)?;
    connection
        .ensure_migrations_table(&migrator.table_name)
        .await?;
    let applied: HashMap<i64, AppliedMigration> = connection
        .list_applied_migrations(&migrator.table_name)
        .await?
        .into_iter()
        .map(|migration| (migration.version, migration))
        .collect();
    let mut statuses: Vec<MigrationStatus> = migrator
        .iter()
        .filter(|migration| migration.migration_type.is_up_migration())
        .map(|migration| MigrationStatus {
            version: migration.version,
            description: migration.description.to_string(),
            state: match applied.get(&migration.version) {
                None => MigrationState::Pending,
                Some(existing) if existing.checksum == migration.checksum => {
                    MigrationState::Applied
                }
                Some(_) => MigrationState::Modified,
            },
        })
        .collect();
    for version in applied.keys().copied() {
        if migrator.version_exists(version) {
            continue;
        }
        statuses.push(match framework_migration(version) {
            Some(framework) => MigrationStatus {
                version,
                description: framework.description.to_owned(),
                state: MigrationState::Disabled,
            },
            None => MigrationStatus {
                version,
                description: String::new(),
                state: MigrationState::Missing,
            },
        });
    }
    statuses.sort_by_key(|status| status.version);
    Ok(statuses)
}

/// Reverts the most recently applied migration within `scope`.
pub(crate) async fn revert_latest<B: Backend>(
    pool: &Pool<B>,
    migrator: &Migrator,
    scope: RevertScope,
) -> Result<Option<i64>, DbError> {
    let mut connection = pool.acquire().await?;
    let result = revert_locked(&mut *connection, migrator, scope).await;
    finish(connection, result).await
}

async fn apply_locked<C: Migrate>(
    connection: &mut C,
    migrator: &Migrator,
) -> Result<Vec<i64>, DbError> {
    let applied = prepare(connection, migrator).await?;

    for migration in migrator
        .iter()
        .filter(|m| m.migration_type.is_up_migration())
    {
        if let Some(existing) = applied.get(&migration.version)
            && existing.checksum != migration.checksum
        {
            return Err(MigrateError::VersionMismatch(migration.version).into());
        }
    }

    let mut newly_applied = Vec::new();
    for migration in migrator
        .iter()
        .filter(|m| m.migration_type.is_up_migration())
    {
        if applied.contains_key(&migration.version) {
            continue;
        }
        connection.apply(&migrator.table_name, migration).await?;
        newly_applied.push(migration.version);
    }
    release(connection, migrator).await?;
    Ok(newly_applied)
}

async fn revert_locked<C: Migrate>(
    connection: &mut C,
    migrator: &Migrator,
    scope: RevertScope,
) -> Result<Option<i64>, DbError> {
    let applied = prepare(connection, migrator).await?;
    let latest = applied
        .keys()
        .copied()
        .filter(|version| match scope {
            RevertScope::Application => framework_migration(*version).is_none(),
            RevertScope::Framework => {
                framework_migration(*version).is_some() && migrator.version_exists(*version)
            }
        })
        .max();
    let Some(latest) = latest else {
        release(connection, migrator).await?;
        if scope == RevertScope::Application
            && let Some(framework) = applied
                .keys()
                .copied()
                .filter_map(framework_migration)
                .max_by_key(|migration| migration.version)
        {
            return Err(DbError::FrameworkMigrationOnly {
                version: framework.version,
                owner: framework.owner,
            });
        }
        return Ok(None);
    };
    let down = migrator
        .iter()
        .find(|m| m.version == latest && m.migration_type.is_down_migration())
        .ok_or(DbError::IrreversibleMigration { version: latest })?;
    connection.revert(&migrator.table_name, down).await?;
    release(connection, migrator).await?;
    Ok(Some(latest))
}

/// Takes the migration lock, creates the bookkeeping table and returns the
/// applied migrations by version after the same safety checks SQLx performs.
async fn prepare<C: Migrate>(
    connection: &mut C,
    migrator: &Migrator,
) -> Result<HashMap<i64, AppliedMigration>, DbError> {
    check_reserved_versions(migrator)?;
    if migrator.locking {
        connection.lock().await?;
    }
    for schema in migrator.create_schemas.iter() {
        connection.create_schema_if_not_exists(schema).await?;
    }
    connection
        .ensure_migrations_table(&migrator.table_name)
        .await?;
    if let Some(version) = connection.dirty_version(&migrator.table_name).await? {
        return Err(MigrateError::Dirty(version).into());
    }
    let applied = connection
        .list_applied_migrations(&migrator.table_name)
        .await?;
    // A framework version the migrator no longer contains belongs to a
    // feature that was turned off; any other unknown version fails closed.
    if !migrator.ignore_missing
        && let Some(missing) = applied.iter().map(|m| m.version).find(|version| {
            !migrator.version_exists(*version) && framework_migration(*version).is_none()
        })
    {
        return Err(MigrateError::VersionMissing(missing).into());
    }
    Ok(applied.into_iter().map(|m| (m.version, m)).collect())
}

/// Refuses an application migration that reuses a reserved framework version,
/// so ownership is never decided for the wrong migration.
fn check_reserved_versions(migrator: &Migrator) -> Result<(), DbError> {
    for migration in migrator.iter() {
        if let Some(framework) = framework_migration(migration.version)
            && migration.description != framework.description
        {
            return Err(DbError::configuration(format!(
                "migration version {} is reserved for the {} migration of {}; give the application migration another version",
                migration.version, framework.description, framework.owner
            )));
        }
    }
    Ok(())
}

async fn release<C: Migrate>(connection: &mut C, migrator: &Migrator) -> Result<(), DbError> {
    if migrator.locking {
        connection.unlock().await?;
    }
    Ok(())
}

/// Returns the connection to the pool on success. After a failure it may still
/// hold the session-level migration lock, so it is closed instead; ending the
/// session releases every lock it holds.
async fn finish<B: Backend, T>(
    connection: PoolConnection<B>,
    result: Result<T, DbError>,
) -> Result<T, DbError> {
    if result.is_err() {
        use sqlx::Connection as _;
        if let Err(error) = connection.detach().close().await {
            debug!(%error, "failed to close the migration connection cleanly");
        }
    }
    result
}

/// Merges independently embedded migration sets into one [`Migrator`].
///
/// An application's own migrations and those shipped by Forge crates (rate
/// limiting, cache, ...) share one `_sqlx_migrations` table, so they must be
/// applied through one migrator: SQLx refuses a set that does not know a version
/// already recorded. Versions must be unique across the sets (shipped sets use
/// timestamp versions of the form `YYYYMMDDHHMMSS`), otherwise the merge fails
/// instead of silently picking one.
///
/// Merging does not change ownership: the framework versions, listed in
/// [`FRAMEWORK_MIGRATIONS`], are never reverted by the application's
/// [`Database::rollback`](crate::Database::rollback) (ADR 0030).
///
/// ```
/// use forge_db::{Migrator, combine_migrations};
///
/// let combined = combine_migrations(&[&Migrator::DEFAULT, &Migrator::DEFAULT])
///     .expect("empty sets do not collide");
/// assert_eq!(combined.iter().count(), 0);
/// ```
pub fn combine_migrations(sets: &[&Migrator]) -> Result<Migrator, DbError> {
    let mut merged: Vec<sqlx::migrate::Migration> = Vec::new();
    for set in sets {
        check_reserved_versions(set)?;
        for migration in set.iter() {
            let clash = merged.iter().any(|existing| {
                existing.version == migration.version
                    && existing.migration_type.is_up_migration()
                        == migration.migration_type.is_up_migration()
            });
            if clash {
                return Err(DbError::configuration(format!(
                    "migration version {} appears in more than one migration set",
                    migration.version
                )));
            }
            merged.push(migration.clone());
        }
    }
    Ok(Migrator::with_migrations(merged))
}

#[cfg(test)]
mod tests {
    use sqlx::{
        SqlSafeStr,
        migrate::{Migration, MigrationType},
    };

    use super::*;

    fn set(versions: &[i64]) -> Migrator {
        Migrator::with_migrations(
            versions
                .iter()
                .flat_map(|version| {
                    let description = framework_migration(*version).map_or("m", |f| f.description);
                    [
                        Migration::new(
                            *version,
                            description.into(),
                            MigrationType::ReversibleUp,
                            "SELECT 1".into_sql_str(),
                            false,
                        ),
                        Migration::new(
                            *version,
                            description.into(),
                            MigrationType::ReversibleDown,
                            "SELECT 2".into_sql_str(),
                            false,
                        ),
                    ]
                })
                .collect(),
        )
    }

    #[test]
    fn combined_sets_are_ordered_by_version_and_keep_down_migrations() {
        let combined =
            combine_migrations(&[&set(&[1, 20_261_019_000_000]), &set(&[2])]).expect("merge");
        let ups: Vec<i64> = combined
            .iter()
            .filter(|m| m.migration_type.is_up_migration())
            .map(|m| m.version)
            .collect();
        assert_eq!(ups, vec![1, 2, 20_261_019_000_000]);
        assert_eq!(
            combined
                .iter()
                .filter(|m| m.migration_type.is_down_migration())
                .count(),
            3
        );
    }

    #[test]
    fn duplicate_versions_are_refused() {
        let error = combine_migrations(&[&set(&[1, 2]), &set(&[2])]).expect_err("clash");
        assert!(error.to_string().contains("version 2"), "{error}");
    }

    #[test]
    fn framework_versions_are_unique_and_described() {
        for (index, migration) in FRAMEWORK_MIGRATIONS.iter().enumerate() {
            assert!(!migration.description.is_empty() && migration.owner.starts_with("forge-"));
            assert!(
                FRAMEWORK_MIGRATIONS[index + 1..]
                    .iter()
                    .all(|other| other.version != migration.version),
                "{migration:?} is listed twice"
            );
        }
    }

    #[test]
    fn an_application_migration_on_a_reserved_version_is_refused() {
        let app = Migrator::with_migrations(vec![Migration::new(
            20_261_019_000_100,
            "create widgets".into(),
            MigrationType::ReversibleUp,
            "SELECT 1".into_sql_str(),
            false,
        )]);
        let error = check_reserved_versions(&app).expect_err("reserved");
        assert!(
            error
                .to_string()
                .contains("reserved for the cache migration of forge-cache"),
            "{error}"
        );
        let error = combine_migrations(&[&app]).expect_err("reserved");
        assert!(matches!(error, DbError::Configuration { .. }), "{error:?}");
        // The framework's own migration under its own description is accepted.
        assert!(check_reserved_versions(&set(&[1, 20_261_019_000_100])).is_ok());
    }
}
