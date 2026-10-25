//! A PostgreSQL test database with least-privilege roles (feature
//! `testing`).
//!
//! [`RlsTestDatabase::create`] provisions an isolated database through
//! [`TestDatabase`] (`FORGE_TEST_DATABASE_URL`), then creates two
//! uniquely named roles, exactly like ADR 0011 deployments:
//!
//! - a **migrator** that owns the database and applies the migrations;
//! - a **runtime** role: `NOSUPERUSER NOBYPASSRLS`, not an owner, named in
//!   the database's `forge.runtime_role` setting so the migrations grant it
//!   its privileges.
//!
//! Row-level security therefore applies to [`RlsTestDatabase::runtime`]
//! exactly as in production. [`RlsTestDatabase::create_with_worker`] also
//! creates a separate **worker** role named in `forge.worker_role`, which
//! receives the worker/operator functions instead of the runtime role. The database and both roles are removed by
//! [`RlsTestDatabase::cleanup`], or when the value is dropped (also during a
//! panic).

use std::time::Duration;

use forge_config::SecretString;
use forge_db::{DbError, Migrator, PgDatabase, testing::TestDatabase};
use serde::{Deserialize, Serialize};
use sqlx::{AssertSqlSafe, Postgres};
use uuid::Uuid;

use crate::{
    EnqueueOptions, Enqueued, Job, JobId,
    job::LeaseToken,
    postgres::{AfterFailure, JobState, LeaseStatus, PgJobStore, Pruning},
};

/// See the [module documentation](self).
#[derive(Debug)]
pub struct RlsTestDatabase {
    // Field order is drop order: the database goes before its roles.
    test: TestDatabase<Postgres>,
    migrator: PgDatabase,
    runtime: PgDatabase,
    runtime_role: String,
    worker: Option<PgDatabase>,
    roles: RoleGuard,
}

impl RlsTestDatabase {
    /// Provisions the database and roles and applies `migrators` in order
    /// as the migrator role.
    pub async fn create(migrators: &[Migrator]) -> Result<Self, DbError> {
        Self::provision(migrators, false).await
    }

    /// Like [`RlsTestDatabase::create`] with a separate worker role.
    pub async fn create_with_worker(migrators: &[Migrator]) -> Result<Self, DbError> {
        Self::provision(migrators, true).await
    }

    async fn provision(migrators: &[Migrator], separate_worker: bool) -> Result<Self, DbError> {
        let admin_url = std::env::var("FORGE_TEST_DATABASE_URL")
            .map(SecretString::new)
            .map_err(|_| configuration("FORGE_TEST_DATABASE_URL is not set"))?;
        let test = TestDatabase::<Postgres>::create_empty().await?;
        let suffix = Uuid::now_v7().simple().to_string();
        // The whole UUID: its leading digits are mostly the timestamp, and
        // fixtures created in the same millisecond must not collide.
        let migrator_role = format!("forge_test_migrator_{suffix}");
        let runtime_role = format!("forge_test_runtime_{suffix}");
        let worker_role = format!("forge_test_worker_{suffix}");
        let password = Uuid::now_v7().simple().to_string();
        let mut names = vec![migrator_role.clone(), runtime_role.clone()];
        if separate_worker {
            names.push(worker_role.clone());
        }
        let roles = RoleGuard {
            admin: admin_url,
            names,
            dropped: false,
        };
        let admin = test.database();
        let database: String = sqlx::query_scalar("SELECT current_database()")
            .fetch_one(admin.pool())
            .await?;
        // Identifiers and the password are generated above, never input.
        let mut statements = Vec::new();
        if separate_worker {
            statements.push(format!(
                "CREATE ROLE \"{worker_role}\" LOGIN PASSWORD '{password}' \
                 NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS"
            ));
            statements.push(format!(
                "ALTER DATABASE \"{database}\" SET forge.worker_role = '{worker_role}'"
            ));
        }
        for statement in [
            format!(
                "CREATE ROLE \"{migrator_role}\" LOGIN PASSWORD '{password}' \
                 NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS"
            ),
            format!(
                "CREATE ROLE \"{runtime_role}\" LOGIN PASSWORD '{password}' \
                 NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS"
            ),
            format!("ALTER DATABASE \"{database}\" OWNER TO \"{migrator_role}\""),
            format!("ALTER DATABASE \"{database}\" SET forge.runtime_role = '{runtime_role}'"),
        ]
        .into_iter()
        .chain(statements)
        {
            sqlx::query(AssertSqlSafe(statement))
                .execute(admin.pool())
                .await?;
        }
        let url = |role: &str| {
            with_credentials(test.url().expose(), role, &password)
                .map(SecretString::new)
                .ok_or_else(|| configuration("FORGE_TEST_DATABASE_URL is not a valid URL"))
        };
        let migrator = PgDatabase::connect(&url(&migrator_role)?).await?;
        for migrations in migrators {
            migrator.migrate(migrations).await?;
        }
        let runtime = PgDatabase::connect(&url(&runtime_role)?).await?;
        let worker = if separate_worker {
            Some(PgDatabase::connect(&url(&worker_role)?).await?)
        } else {
            None
        };
        Ok(Self {
            test,
            migrator,
            runtime,
            runtime_role,
            worker,
            roles,
        })
    }

    /// A superuser connection to the test database, for inspection.
    #[must_use]
    pub fn admin(&self) -> &PgDatabase {
        self.test.database()
    }

    /// The schema owner's pool (the role that applied the migrations).
    #[must_use]
    pub const fn migrator(&self) -> &PgDatabase {
        &self.migrator
    }

    /// The least-privilege runtime pool.
    #[must_use]
    pub const fn runtime(&self) -> &PgDatabase {
        &self.runtime
    }

    /// The pool of the role that runs workers and operator commands: the
    /// separate worker role with [`RlsTestDatabase::create_with_worker`],
    /// the runtime role otherwise.
    #[must_use]
    pub fn worker(&self) -> &PgDatabase {
        self.worker.as_ref().unwrap_or(&self.runtime)
    }

    /// The runtime role's name.
    #[must_use]
    pub fn runtime_role(&self) -> &str {
        &self.runtime_role
    }

    /// Closes the pools, drops the database and then the roles.
    pub async fn cleanup(self) -> Result<(), DbError> {
        let Self {
            test,
            migrator,
            runtime,
            runtime_role: _,
            worker,
            mut roles,
        } = self;
        migrator.close().await;
        runtime.close().await;
        if let Some(worker) = worker {
            worker.close().await;
        }
        test.cleanup().await?;
        roles.drop_roles().await
    }
}

/// Drops the roles, from a helper thread when dropped without `cleanup`.
#[derive(Debug)]
struct RoleGuard {
    admin: SecretString,
    names: Vec<String>,
    dropped: bool,
}

impl RoleGuard {
    async fn drop_roles(&mut self) -> Result<(), DbError> {
        if self.dropped {
            return Ok(());
        }
        self.dropped = true;
        let admin = PgDatabase::connect(&self.admin).await?;
        let mut outcome = Ok(());
        for name in &self.names {
            let statement = format!("DROP ROLE IF EXISTS \"{name}\"");
            if let Err(error) = sqlx::query(AssertSqlSafe(statement))
                .execute(admin.pool())
                .await
            {
                outcome = Err(DbError::from(error));
            }
        }
        admin.close().await;
        outcome
    }
}

impl Drop for RoleGuard {
    fn drop(&mut self) {
        if self.dropped {
            return;
        }
        let mut guard = Self {
            admin: self.admin.clone(),
            names: std::mem::take(&mut self.names),
            dropped: false,
        };
        self.dropped = true;
        // A `Drop` cannot await and the test runtime may be shutting down.
        let joined = std::thread::Builder::new()
            .name("forge-test-role-cleanup".to_owned())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|_| configuration("could not start the cleanup runtime"))?;
                runtime.block_on(guard.drop_roles())
            })
            .map(std::thread::JoinHandle::join);
        if !matches!(joined, Ok(Ok(Ok(())))) {
            tracing::warn!("failed to drop the test roles");
        }
    }
}

macro_rules! kit_job {
    ($name:ident, $text:literal) => {
        #[derive(Serialize, Deserialize)]
        struct $name;

        impl Job for $name {
            const NAME: &'static str = $text;
            const MAX_ATTEMPTS: u16 = 3;
        }
    };
}

kit_job!(Pending, "kit.pending");
kit_job!(Dead, "kit.dead");
kit_job!(Done, "kit.done");
kit_job!(Leased, "kit.leased");

fn must<T>(what: &str, outcome: Result<T, crate::JobsError>) -> T {
    outcome.unwrap_or_else(|error| panic!("{what}: {error}"))
}

async fn enqueue_new<J: Job>(store: &PgJobStore, job: &J) -> JobId {
    match must("enqueue", store.enqueue(job, EnqueueOptions::new()).await) {
        Enqueued::New(id) => id,
        Enqueued::Duplicate(id) => panic!("enqueue of {} found the duplicate {id:?}", J::NAME),
    }
}

async fn claim_one(store: &PgJobStore, name: &str) -> (JobId, LeaseToken) {
    let token = LeaseToken::new();
    let claimed = must(
        "claim",
        store
            .claim(
                "default",
                &[name.to_owned()],
                1,
                Duration::from_secs(3600),
                token,
            )
            .await,
    );
    match claimed.as_slice() {
        [job] => (job.id, token),
        other => panic!("claim of {name} returned {} jobs, want 1", other.len()),
    }
}

/// What the job store must do when its database fails (the Forge-Go
/// `storetest.RunUnavailable`): every operation returns the infrastructure
/// error, never an outcome that means something to a worker (a lost lease, a
/// missing job, an unknown or duplicate job), and a failed write changes
/// nothing, so the lease a worker held is still good when the database
/// returns.
///
/// `store` must be connected with the runtime (or worker) role to a migrated
/// database. `sabotage` makes the job tables unavailable as a renamed table, a
/// revoked grant or a lost connection would; `restore` undoes it. Both run
/// with a role that can change the schema (for example
/// [`RlsTestDatabase::admin`]).
///
/// # Panics
///
/// With a message naming the first operation that succeeded, or reported an
/// ordinary outcome, during the outage, or the first state that changed.
pub async fn run_unavailable<S, R>(store: &PgJobStore, sabotage: S, restore: R)
where
    S: AsyncFnOnce(),
    R: AsyncFnOnce(),
{
    let hour = Duration::from_secs(3600);
    let pending = enqueue_new(store, &Pending).await;
    let dead = enqueue_new(store, &Dead).await;
    let done = enqueue_new(store, &Done).await;
    let leased = enqueue_new(store, &Leased).await;
    let (id, token) = claim_one(store, Dead::NAME).await;
    assert_eq!(id, dead, "claimed the wrong job");
    let state = must(
        "fail",
        store.fail(id, token, "boom", Duration::ZERO, true).await,
    );
    assert_eq!(state, Some(AfterFailure::Dead), "the job is not dead");
    let (id, token) = claim_one(store, Done::NAME).await;
    assert_eq!(id, done, "claimed the wrong job");
    assert!(must("complete", store.complete(id, token).await));
    let (id, lease) = claim_one(store, Leased::NAME).await;
    assert_eq!(id, leased, "claimed the wrong job");
    let before = must("stats", store.stats().await);

    sabotage().await;
    let retry = Duration::from_secs(1);
    let outcomes = [
        (
            "enqueue",
            store
                .enqueue(&Pending, EnqueueOptions::new())
                .await
                .is_err(),
        ),
        (
            "enqueue with a unique key",
            store
                .enqueue(&Pending, EnqueueOptions::new().unique_key("kit"))
                .await
                .is_err(),
        ),
        (
            "claim",
            store
                .claim(
                    "default",
                    &[Pending::NAME.to_owned()],
                    1,
                    hour,
                    LeaseToken::new(),
                )
                .await
                .is_err(),
        ),
        (
            "heartbeat",
            store.heartbeat(&[(leased, lease)], hour).await.is_err(),
        ),
        ("complete", store.complete(leased, lease).await.is_err()),
        (
            "fail",
            store.fail(leased, lease, "x", retry, false).await.is_err(),
        ),
        (
            "fail to dead",
            store.fail(leased, lease, "x", retry, true).await.is_err(),
        ),
        ("release", store.release(leased, lease).await.is_err()),
        ("stats", store.stats().await.is_err()),
        ("dead", store.dead(10, None).await.is_err()),
        ("retry", store.retry(dead).await.is_err()),
        ("discard", store.discard(dead).await.is_err()),
        (
            "prune",
            store
                .prune(&Pruning::default().completed_after(Duration::ZERO))
                .await
                .is_err(),
        ),
    ];
    for (name, failed) in outcomes {
        assert!(
            failed,
            "{name} did not fail while the database was unavailable; a worker would act on a wrong answer"
        );
    }
    restore().await;

    // Nothing changed, and the lease is still good.
    assert_eq!(
        must("stats", store.stats().await),
        before,
        "the counts changed during the outage"
    );
    let dead_jobs = must("dead", store.dead(10, None).await);
    assert_eq!(
        dead_jobs.iter().map(|job| job.id).collect::<Vec<_>>(),
        [dead],
        "the dead job changed"
    );
    assert!(
        before
            .iter()
            .any(|stats| stats.state == JobState::Running && stats.jobs == 1),
        "the leased job is not running"
    );
    let renewed = must("heartbeat", store.heartbeat(&[(leased, lease)], hour).await);
    assert_eq!(
        renewed,
        [(leased, LeaseStatus::Renewed)],
        "the lease did not survive the outage"
    );
    assert!(
        must("complete", store.complete(leased, lease).await),
        "the lease did not survive the outage"
    );
    let (queued, _) = claim_one(store, Pending::NAME).await;
    assert_eq!(queued, pending, "the queued job changed");
}

fn configuration(reason: &str) -> DbError {
    DbError::Configuration {
        reason: reason.to_owned(),
    }
}

/// Replaces the user and password of a `scheme://user:pass@host/...` URL.
fn with_credentials(url: &str, user: &str, password: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let location = rest.rsplit_once('@').map_or(rest, |(_, location)| location);
    Some(format!("{scheme}://{user}:{password}@{location}"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn credentials_are_replaced() {
        assert_eq!(
            super::with_credentials("postgres://postgres:pw@127.0.0.1:5432/db?x=1", "u", "p")
                .as_deref(),
            Some("postgres://u:p@127.0.0.1:5432/db?x=1")
        );
        assert_eq!(
            super::with_credentials("postgres://127.0.0.1/db", "u", "p").as_deref(),
            Some("postgres://u:p@127.0.0.1/db")
        );
        assert_eq!(super::with_credentials("nonsense", "u", "p"), None);
    }
}
