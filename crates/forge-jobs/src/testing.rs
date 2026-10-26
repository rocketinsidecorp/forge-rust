//! Test support for the PostgreSQL job store (feature `testing`).
//!
//! [`RlsTestDatabase`] is `forge_db::testing::RlsTestDatabase`, re-exported
//! here for the tests that already name it: an isolated database with a
//! migration role, a least-privilege runtime role named in
//! `forge.runtime_role` (so the job migrations grant it its privileges) and,
//! with `create_with_worker`, a worker role named in `forge.worker_role`.
//! Row-level security applies to its runtime role exactly as in production.
//! [`run_unavailable`] is the contract every job store keeps when its
//! database fails.

use std::time::Duration;

pub use forge_db::testing::RlsTestDatabase;
use serde::{Deserialize, Serialize};

use crate::{
    EnqueueOptions, Enqueued, Job, JobId,
    job::LeaseToken,
    postgres::{AfterFailure, JobState, LeaseStatus, PgJobStore, Pruning},
};

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
/// The store's database must not be shared with other tests: a panic between
/// `sabotage` and `restore` leaves the table renamed, and nothing puts it back.
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
