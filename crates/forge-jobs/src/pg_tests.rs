//! PostgreSQL integration tests, run as the least-privilege runtime role.
//!
//! `#[ignore]`d unless requested: `FORGE_TEST_DATABASE_URL=postgres://...
//! cargo test -p forge-jobs -- --include-ignored`.

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use forge_auth::{Session, UnixTimestamp};
use forge_core::{Cancellation, Timestamp};
use forge_db::{DbError, PgDatabase};
use forge_security::{Permission, PrincipalId, RbacPolicy, Role};
use forge_tenancy::{Membership, TenantContext, TenantId};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::{
    Backoff, Cron, EnqueueOptions, Enqueued, Job, JobContext, JobError, JobHandler, JobId, Jobs,
    JobsError, Schedule, WorkerSettings,
    job::LeaseToken,
    postgres::{
        self, JobState, LeaseStatus, NewJob, PgJobStore, Pruning, TickOutcome,
        begin_job_transaction, complete_in, enqueue, enqueue_for_tenant, migrator,
    },
    testing::RlsTestDatabase,
};

const IGNORE: &str = "needs FORGE_TEST_DATABASE_URL";

async fn fresh() -> RlsTestDatabase {
    RlsTestDatabase::create(&[migrator()]).await.expect(IGNORE)
}

fn fast() -> WorkerSettings {
    WorkerSettings::default()
        .poll_interval(Duration::from_millis(50))
        .lease(Duration::from_secs(3))
        .drain_timeout(Duration::from_secs(5))
        .cancel_grace(Duration::from_millis(500))
}

/// Runs `jobs` until the returned token is cancelled.
fn spawn_runtime(jobs: Jobs, db: &PgDatabase) -> (Cancellation, JoinHandle<Result<(), JobsError>>) {
    let shutdown = Cancellation::new();
    let handle = tokio::spawn(jobs.run(PgJobStore::new(db.clone()), shutdown.clone()));
    (shutdown, handle)
}

async fn stop(shutdown: Cancellation, handle: JoinHandle<Result<(), JobsError>>) {
    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(30), handle)
        .await
        .expect("runtime stops in time")
        .expect("runtime task")
        .expect("runtime result");
}

async fn wait_until<F, Fut>(what: &str, mut condition: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while !condition().await {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[derive(Debug)]
struct JobRow {
    state: String,
    attempts: i32,
    last_error: Option<String>,
}

async fn job_row(db: &RlsTestDatabase, id: JobId) -> JobRow {
    let row = sqlx::query("SELECT state, attempts, last_error FROM forge_jobs WHERE id = $1")
        .bind(id.into_uuid())
        .fetch_one(db.admin().pool())
        .await
        .expect("job row");
    JobRow {
        state: row.get("state"),
        attempts: row.get("attempts"),
        last_error: row.get("last_error"),
    }
}

async fn count_state(db: &RlsTestDatabase, state: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM forge_jobs WHERE state = $1")
        .bind(state)
        .fetch_one(db.admin().pool())
        .await
        .expect("count")
}

// ---------------------------------------------------------------- jobs

#[derive(Serialize, Deserialize)]
struct Count {
    n: u32,
}

impl Job for Count {
    const NAME: &'static str = "test.count";
}

#[derive(Clone, Default)]
struct Counter(Arc<Mutex<HashMap<JobId, usize>>>);

#[async_trait]
impl JobHandler for Counter {
    type Job = Count;

    async fn perform(&self, context: &JobContext, _: Count) -> Result<(), JobError> {
        tokio::time::sleep(Duration::from_millis(2)).await;
        *self
            .0
            .lock()
            .expect("lock")
            .entry(context.id())
            .or_default() += 1;
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
struct Flaky;

impl Job for Flaky {
    const NAME: &'static str = "test.flaky";
    const MAX_ATTEMPTS: u16 = 3;
    const BACKOFF: Backoff = Backoff::fixed(Duration::ZERO);
}

struct AlwaysFails(Arc<AtomicUsize>);

#[async_trait]
impl JobHandler for AlwaysFails {
    type Job = Flaky;

    async fn perform(&self, context: &JobContext, _: Flaky) -> Result<(), JobError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(JobError::retry(format!(
            "attempt {} failed",
            context.attempt()
        )))
    }
}

#[derive(Serialize, Deserialize)]
struct Slow;

impl Job for Slow {
    const NAME: &'static str = "test.slow";
    const MAX_ATTEMPTS: u16 = 1;
    const TIMEOUT: Duration = Duration::from_millis(200);
}

struct Sleeper;

#[async_trait]
impl JobHandler for Sleeper {
    type Job = Slow;

    async fn perform(&self, _: &JobContext, _: Slow) -> Result<(), JobError> {
        tokio::time::sleep(Duration::from_secs(60)).await;
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
struct Explode;

impl Job for Explode {
    const NAME: &'static str = "test.explode";
    const MAX_ATTEMPTS: u16 = 1;
}

struct Panics;

#[async_trait]
impl JobHandler for Panics {
    type Job = Explode;

    async fn perform(&self, _: &JobContext, _: Explode) -> Result<(), JobError> {
        panic!("handler bug with secret payload data");
    }
}

#[derive(Serialize, Deserialize)]
struct Long {
    cooperative: bool,
}

impl Job for Long {
    const NAME: &'static str = "test.long";
    const QUEUE: &'static str = "long";
}

struct Lingers(Arc<AtomicUsize>);

#[async_trait]
impl JobHandler for Lingers {
    type Job = Long;

    async fn perform(&self, context: &JobContext, job: Long) -> Result<(), JobError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        if job.cooperative {
            context.cancellation().cancelled().await;
            return Err(JobError::retry("cancelled"));
        }
        // Ignores cancellation: only an abort stops it.
        tokio::time::sleep(Duration::from_secs(600)).await;
        Ok(())
    }
}

// ---------------------------------------------------------------- tests

#[tokio::test]
#[ignore = "needs FORGE_TEST_DATABASE_URL"]
async fn schema_reverts_cleanly_and_grants_least_privilege() {
    let db = fresh().await;
    let runtime = db.runtime();

    // Direct writes beyond INSERT are denied; the migration history is hidden.
    for statement in [
        "UPDATE forge_jobs SET attempts = 0",
        "DELETE FROM forge_jobs",
        "SELECT * FROM _forge_jobs_migrations",
        "CREATE TABLE intruder (id int)",
    ] {
        let error = sqlx::query(sqlx::AssertSqlSafe(statement))
            .execute(runtime.pool())
            .await
            .map_err(DbError::from)
            .expect_err(statement);
        assert!(matches!(error, DbError::Query(_)), "{statement}: {error:?}");
    }
    let definer: Vec<String> = sqlx::query_scalar(
        "SELECT proname::text FROM pg_proc
         WHERE proname LIKE 'forge\\_jobs\\_%'
           AND (proconfig IS NULL
                OR (NOT prosecdef AND proname <> 'forge_jobs_require_worker_scope'))",
    )
    .fetch_all(db.admin().pool())
    .await
    .expect("functions");
    assert!(
        definer.is_empty(),
        "every function pins its search path: {definer:?}"
    );

    assert_eq!(
        db.migrator().rollback(&migrator()).await.expect("rollback"),
        Some(1)
    );
    let left: i64 =
        sqlx::query_scalar("SELECT count(*) FROM pg_class WHERE relname LIKE 'forge\\_job%'")
            .fetch_one(db.admin().pool())
            .await
            .expect("count");
    assert_eq!(left, 0);
    assert_eq!(
        db.migrator().migrate(&migrator()).await.expect("re-apply"),
        vec![1]
    );
    db.cleanup().await.expect("cleanup");
}

#[tokio::test]
#[ignore = "needs FORGE_TEST_DATABASE_URL"]
async fn enqueue_commits_with_the_transaction_and_honours_unique_keys() {
    let db = fresh().await;
    let runtime = db.runtime();

    let mut rolled_back = runtime.begin().await.expect("begin");
    let lost = enqueue(&mut rolled_back, &Count { n: 1 }, EnqueueOptions::new())
        .await
        .expect("enqueue");
    rolled_back.rollback().await.expect("rollback");
    let mut committed = runtime.begin().await.expect("begin");
    let kept = enqueue(
        &mut committed,
        &Count { n: 2 },
        EnqueueOptions::new().unique_key("k"),
    )
    .await
    .expect("enqueue");
    // Not visible to workers before commit.
    let store = PgJobStore::new(runtime.clone());
    let names = vec![Count::NAME.to_owned()];
    let early = store
        .claim(
            "default",
            &names,
            10,
            Duration::from_secs(5),
            LeaseToken::new(),
        )
        .await
        .expect("claim");
    assert!(early.is_empty(), "uncommitted jobs are invisible");
    committed.commit().await.expect("commit");

    let exists: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM forge_jobs WHERE id = $1)")
        .bind(lost.id().into_uuid())
        .fetch_one(db.admin().pool())
        .await
        .expect("query");
    assert!(!exists, "a rolled-back enqueue leaves nothing behind");

    let again = store
        .enqueue(&Count { n: 3 }, EnqueueOptions::new().unique_key("k"))
        .await
        .expect("duplicate");
    assert_eq!(again, Enqueued::Duplicate(kept.id()));
    assert!(matches!(kept, Enqueued::New(_)));

    let future = store
        .enqueue(
            &Count { n: 4 },
            EnqueueOptions::new().delay(Duration::from_secs(3600)),
        )
        .await
        .expect("delayed");
    let claimed = store
        .claim(
            "default",
            &names,
            10,
            Duration::from_secs(5),
            LeaseToken::new(),
        )
        .await
        .expect("claim");
    let ids: Vec<JobId> = claimed.iter().map(|job| job.id).collect();
    assert_eq!(ids, vec![kept.id()], "delayed jobs wait for their run time");
    assert_eq!(claimed[0].payload, r#"{"n": 2}"#);
    assert_eq!(job_row(&db, future.id()).await.state, "queued");
    db.cleanup().await.expect("cleanup");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs FORGE_TEST_DATABASE_URL"]
async fn many_workers_execute_every_job_exactly_once() {
    let db = fresh().await;
    let store = PgJobStore::new(db.runtime().clone());
    let total = 300;
    let mut transaction = db.runtime().begin().await.expect("begin");
    for n in 0..total {
        enqueue(&mut transaction, &Count { n }, EnqueueOptions::new())
            .await
            .expect("enqueue");
    }
    transaction.commit().await.expect("commit");

    let counter = Counter::default();
    let mut runtimes = Vec::new();
    for _ in 0..4 {
        let mut jobs = Jobs::new();
        jobs.register(counter.clone()).expect("register");
        jobs.concurrency("default", 8).expect("concurrency");
        jobs.settings(fast());
        runtimes.push(spawn_runtime(jobs, store.database()));
    }
    wait_until("all jobs to complete", || async {
        count_state(&db, "completed").await == i64::from(total)
    })
    .await;
    for (shutdown, handle) in runtimes {
        stop(shutdown, handle).await;
    }
    let executions = counter.0.lock().expect("lock").clone();
    assert_eq!(executions.len(), total as usize);
    assert!(
        executions.values().all(|runs| *runs == 1),
        "no job ran twice"
    );
    db.cleanup().await.expect("cleanup");
}

#[tokio::test]
#[ignore = "needs FORGE_TEST_DATABASE_URL"]
async fn expired_leases_are_reclaimed_and_stale_workers_are_fenced_out() {
    let db = fresh().await;
    let store = PgJobStore::new(db.runtime().clone());
    let names = vec![Count::NAME.to_owned()];
    let id = store
        .enqueue(&Count { n: 1 }, EnqueueOptions::new())
        .await
        .expect("enqueue")
        .id();
    let lease = Duration::from_secs(1);

    let stale = LeaseToken::new();
    let first = store
        .claim("default", &names, 1, lease, stale)
        .await
        .expect("claim");
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].attempt, 1);
    // A live lease is not reclaimable.
    let none = store
        .claim("default", &names, 1, lease, LeaseToken::new())
        .await
        .expect("claim");
    assert!(none.is_empty());

    tokio::time::sleep(Duration::from_millis(1300)).await;
    let fresh_token = LeaseToken::new();
    let second = store
        .claim("default", &names, 1, lease, fresh_token)
        .await
        .expect("reclaim");
    assert_eq!(second.len(), 1);
    assert_eq!((second[0].id, second[0].attempt), (id, 2));

    // The stale worker can no longer touch the job.
    assert!(!store.complete(id, stale).await.expect("complete"));
    assert_eq!(
        store
            .fail(id, stale, "late", Duration::ZERO, false)
            .await
            .expect("fail"),
        None
    );
    assert!(!store.release(id, stale).await.expect("release"));
    assert_eq!(
        store
            .heartbeat(&[(id, stale)], lease)
            .await
            .expect("heartbeat"),
        vec![(id, LeaseStatus::Lost)]
    );
    assert_eq!(
        store
            .heartbeat(&[(id, fresh_token)], lease)
            .await
            .expect("heartbeat"),
        vec![(id, LeaseStatus::Renewed)]
    );
    assert!(store.complete(id, fresh_token).await.expect("complete"));
    assert_eq!(job_row(&db, id).await.state, "completed");

    // A lease that expires on the final attempt dead-letters the job.
    #[derive(Serialize, Deserialize)]
    struct Once;
    impl Job for Once {
        const NAME: &'static str = "test.once";
        const MAX_ATTEMPTS: u16 = 1;
    }
    let once = store
        .enqueue(&Once, EnqueueOptions::new())
        .await
        .expect("enqueue")
        .id();
    let once_names = vec![Once::NAME.to_owned()];
    store
        .claim("default", &once_names, 1, lease, LeaseToken::new())
        .await
        .expect("claim");
    tokio::time::sleep(Duration::from_millis(1300)).await;
    let reclaimed = store
        .claim("default", &once_names, 1, lease, LeaseToken::new())
        .await
        .expect("claim");
    assert!(reclaimed.is_empty());
    let row = job_row(&db, once).await;
    assert_eq!((row.state.as_str(), row.attempts), ("dead", 1));
    assert_eq!(
        row.last_error.as_deref(),
        Some("lease expired before the job finished")
    );
    db.cleanup().await.expect("cleanup");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs FORGE_TEST_DATABASE_URL"]
async fn failures_retry_with_backoff_then_die_and_operators_can_retry_or_discard() {
    let db = fresh().await;
    let store = PgJobStore::new(db.runtime().clone());

    // The retry delay is applied by the database clock.
    let names = vec![Count::NAME.to_owned()];
    let delayed = store
        .enqueue(&Count { n: 0 }, EnqueueOptions::new())
        .await
        .expect("enqueue")
        .id();
    let token = LeaseToken::new();
    store
        .claim("default", &names, 1, Duration::from_secs(5), token)
        .await
        .expect("claim");
    let state = store
        .fail(delayed, token, "smtp down", Duration::from_secs(120), false)
        .await
        .expect("fail");
    assert_eq!(state, Some(postgres::AfterFailure::Retrying));
    let wait: f64 = sqlx::query_scalar(
        "SELECT extract(epoch FROM run_at - now())::float8 FROM forge_jobs WHERE id = $1",
    )
    .bind(delayed.into_uuid())
    .fetch_one(db.admin().pool())
    .await
    .expect("run_at");
    assert!((110.0..=121.0).contains(&wait), "{wait}");

    let attempts = Arc::new(AtomicUsize::new(0));
    let id = store
        .enqueue(&Flaky, EnqueueOptions::new())
        .await
        .expect("enqueue")
        .id();
    let mut jobs = Jobs::new();
    jobs.register(AlwaysFails(Arc::clone(&attempts)))
        .expect("register");
    jobs.settings(fast());
    let (shutdown, handle) = spawn_runtime(jobs, store.database());
    wait_until("the job to die", || async {
        job_row(&db, id).await.state == "dead"
    })
    .await;
    stop(shutdown, handle).await;

    assert_eq!(attempts.load(Ordering::SeqCst), 3);
    let row = job_row(&db, id).await;
    assert_eq!(row.attempts, 3);
    assert_eq!(row.last_error.as_deref(), Some("attempt 3 failed"));

    let dead = store.dead(10, None).await.expect("dead");
    assert_eq!(dead.len(), 1);
    assert_eq!((dead[0].id, dead[0].name.as_str()), (id, Flaky::NAME));
    assert!(dead[0].failed_at.is_some());
    let stats = store.stats().await.expect("stats");
    assert!(
        stats
            .iter()
            .any(|s| s.state == JobState::Dead && s.jobs == 1)
    );
    assert!(
        stats
            .iter()
            .any(|s| s.state == JobState::Queued && s.oldest_run_at.is_some())
    );

    assert!(store.retry(id).await.expect("retry"));
    assert!(
        !store.retry(id).await.expect("only dead jobs"),
        "already queued"
    );
    let row = job_row(&db, id).await;
    assert_eq!((row.state.as_str(), row.attempts), ("queued", 0));
    // Make it dead again and discard it.
    let token = LeaseToken::new();
    let flaky = vec![Flaky::NAME.to_owned()];
    store
        .claim("default", &flaky, 1, Duration::from_secs(5), token)
        .await
        .expect("claim");
    store
        .fail(id, token, "permanent", Duration::ZERO, true)
        .await
        .expect("discard");
    assert!(store.discard(id).await.expect("discard"));
    assert!(!store.discard(id).await.expect("gone"));
    db.cleanup().await.expect("cleanup");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs FORGE_TEST_DATABASE_URL"]
async fn timeouts_and_panics_fail_the_attempt_and_the_worker_keeps_running() {
    let db = fresh().await;
    let store = PgJobStore::new(db.runtime().clone());
    let slow = store
        .enqueue(&Slow, EnqueueOptions::new())
        .await
        .expect("enqueue")
        .id();
    let boom = store
        .enqueue(&Explode, EnqueueOptions::new())
        .await
        .expect("enqueue")
        .id();

    let counter = Counter::default();
    let mut jobs = Jobs::new();
    jobs.register(Sleeper).expect("register");
    jobs.register(Panics).expect("register");
    jobs.register(counter.clone()).expect("register");
    jobs.concurrency("default", 2).expect("concurrency");
    jobs.settings(fast());
    let (shutdown, handle) = spawn_runtime(jobs, store.database());

    wait_until("timeout and panic", || async {
        job_row(&db, slow).await.state == "dead" && job_row(&db, boom).await.state == "dead"
    })
    .await;
    assert_eq!(
        job_row(&db, slow).await.last_error.as_deref(),
        Some("the attempt exceeded its timeout")
    );
    let panicked = job_row(&db, boom).await;
    assert_eq!(panicked.last_error.as_deref(), Some("the job panicked"));

    // The same workers still run new jobs.
    let after = store
        .enqueue(&Count { n: 9 }, EnqueueOptions::new())
        .await
        .expect("enqueue")
        .id();
    wait_until("the next job", || async {
        job_row(&db, after).await.state == "completed"
    })
    .await;
    assert!(!handle.is_finished());
    stop(shutdown, handle).await;
    db.cleanup().await.expect("cleanup");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs FORGE_TEST_DATABASE_URL"]
async fn shutdown_drains_then_cancels_and_releases_leases() {
    let db = fresh().await;
    let store = PgJobStore::new(db.runtime().clone());
    let cooperative = store
        .enqueue(&Long { cooperative: true }, EnqueueOptions::new())
        .await
        .expect("enqueue")
        .id();
    let stubborn = store
        .enqueue(&Long { cooperative: false }, EnqueueOptions::new())
        .await
        .expect("enqueue")
        .id();
    let started = Arc::new(AtomicUsize::new(0));
    let mut jobs = Jobs::new();
    jobs.register(Lingers(Arc::clone(&started)))
        .expect("register");
    jobs.settings(
        fast()
            .drain_timeout(Duration::from_millis(300))
            .cancel_grace(Duration::from_millis(300)),
    );
    let (shutdown, handle) = spawn_runtime(jobs, store.database());
    wait_until("both jobs to start", || async {
        started.load(Ordering::SeqCst) == 2
    })
    .await;

    let begun = std::time::Instant::now();
    stop(shutdown, handle).await;
    assert!(
        begun.elapsed() < Duration::from_secs(5),
        "{:?}",
        begun.elapsed()
    );

    for id in [cooperative, stubborn] {
        let row = job_row(&db, id).await;
        assert_eq!(
            (row.state.as_str(), row.attempts),
            ("queued", 0),
            "interrupted jobs are released without consuming an attempt"
        );
    }
    let leases: i64 =
        sqlx::query_scalar("SELECT count(*) FROM forge_jobs WHERE lease_token IS NOT NULL")
            .fetch_one(db.admin().pool())
            .await
            .expect("count");
    assert_eq!(leases, 0);
    db.cleanup().await.expect("cleanup");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs FORGE_TEST_DATABASE_URL"]
async fn two_schedulers_fire_each_tick_exactly_once() {
    let db = fresh().await;
    let store = PgJobStore::new(db.runtime().clone());
    let every_minute: Cron = "* * * * *".parse().expect("cron");

    // Racing replicas on the same tick: one winner.
    let tick = every_minute
        .latest_at_or_before(Timestamp::now())
        .expect("tick");
    let schedule = Schedule::new("race", every_minute.clone(), &Count { n: 0 }).expect("schedule");
    let job = || schedule.job_for(tick).expect("job");
    let (a, b) = tokio::join!(
        store.fire_tick("race", tick, Duration::from_secs(300), job()),
        store.fire_tick("race", tick, Duration::from_secs(300), job())
    );
    let outcomes = [a.expect("a"), b.expect("b")];
    assert_eq!(
        outcomes
            .iter()
            .filter(|o| matches!(o, TickOutcome::Fired(_)))
            .count(),
        1
    );
    assert!(outcomes.contains(&TickOutcome::AlreadyHandled));
    // Replaying an old tick never fires again.
    let older = Timestamp::from_unix_micros(tick.unix_micros() - 60_000_000).expect("older");
    assert_eq!(
        store
            .fire_tick("race", older, Duration::from_secs(300), job())
            .await
            .expect("old"),
        TickOutcome::AlreadyHandled
    );
    // A first tick older than the allowed lateness is skipped.
    let stale = Timestamp::from_unix_micros(tick.unix_micros() - 3_600_000_000).expect("stale");
    assert_eq!(
        store
            .fire_tick("stale", stale, Duration::from_secs(60), job())
            .await
            .expect("stale"),
        TickOutcome::Skipped
    );

    // Two full runtimes with the same schedule enqueue the current tick once.
    let mut runtimes = Vec::new();
    for _ in 0..2 {
        let mut jobs = Jobs::new();
        jobs.register(Counter::default()).expect("register");
        jobs.schedule(
            Schedule::new("every-minute", every_minute.clone(), &Count { n: 1 }).expect("schedule"),
        )
        .expect("schedule");
        jobs.settings(fast());
        runtimes.push(spawn_runtime(jobs, store.database()));
    }
    wait_until("the schedule to fire", || async {
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM forge_job_schedules WHERE name = 'every-minute'",
        )
        .fetch_one(db.admin().pool())
        .await
        .expect("count")
            == 1
    })
    .await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    for (shutdown, handle) in runtimes {
        stop(shutdown, handle).await;
    }
    let fired: Vec<String> = sqlx::query_scalar(
        "SELECT unique_key FROM forge_jobs WHERE unique_key LIKE 'forge.schedule:every-minute:%'",
    )
    .fetch_all(db.admin().pool())
    .await
    .expect("jobs");
    // Normally one tick; two only if the test straddled a minute boundary,
    // and then the keys differ.
    assert!(!fired.is_empty() && fired.len() <= 2, "{fired:?}");
    let mut unique = fired.clone();
    unique.dedup();
    assert_eq!(unique.len(), fired.len());
    db.cleanup().await.expect("cleanup");
}

fn tenant_context() -> TenantContext {
    let principal = PrincipalId::new();
    let tenant = TenantId::new();
    let permission = Permission::new("reports:generate").expect("permission");
    let role = Role::new("member").expect("role");
    let membership = Membership::active(tenant, principal, vec![role.clone()]);
    let mut policy = RbacPolicy::new();
    policy.allow(role, permission.clone());
    let authenticated = Session::new(
        principal,
        UnixTimestamp::from_secs(1),
        UnixTimestamp::from_secs(10),
    )
    .expect("session")
    .authenticate(UnixTimestamp::from_secs(2))
    .expect("authenticated");
    TenantContext::authorize(authenticated, &membership, &policy, &permission).expect("context")
}

async fn scoped(
    db: &PgDatabase,
    context: Option<&TenantContext>,
) -> forge_db::DbTransaction<sqlx::Postgres> {
    let mut transaction = db.begin().await.expect("begin");
    if let Some(context) = context {
        sqlx::query(
            "SELECT set_config('forge.tenant_id', $1, true), set_config('forge.principal_id', $2, true)",
        )
        .bind(context.tenant_id().to_string())
        .bind(context.principal_id().to_string())
        .execute(transaction.connection())
        .await
        .expect("scope");
    }
    transaction
}

async fn visible(db: &PgDatabase, context: Option<&TenantContext>) -> Vec<Uuid> {
    let mut transaction = scoped(db, context).await;
    let ids = sqlx::query_scalar("SELECT id FROM forge_jobs ORDER BY id")
        .fetch_all(transaction.connection())
        .await
        .expect("select");
    transaction.rollback().await.expect("rollback");
    ids
}

#[derive(Serialize, Deserialize)]
struct Report;

impl Job for Report {
    const NAME: &'static str = "test.report";
}

/// The tenant setting and the visible job ids seen by one attempt.
type Observation = (Option<String>, Vec<Uuid>);

/// Records the tenant scope the handler's transaction sees.
struct ScopeProbe {
    database: PgDatabase,
    seen: Arc<Mutex<Vec<Observation>>>,
}

#[async_trait]
impl JobHandler for ScopeProbe {
    type Job = Report;

    async fn perform(&self, context: &JobContext, _: Report) -> Result<(), JobError> {
        let mut transaction = begin_job_transaction(&self.database, context).await?;
        let tenant: Option<String> =
            sqlx::query_scalar("SELECT nullif(current_setting('forge.tenant_id', true), '')")
                .fetch_one(transaction.connection())
                .await?;
        let ids: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM forge_jobs ORDER BY id")
            .fetch_all(transaction.connection())
            .await?;
        complete_in(&mut transaction, context).await?;
        transaction.commit().await?;
        self.seen.lock().expect("lock").push((tenant, ids));
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs FORGE_TEST_DATABASE_URL"]
async fn row_level_security_isolates_tenants_and_workers_restore_the_scope() {
    let db = fresh().await;
    let runtime = db.runtime();
    let (alpha, beta) = (tenant_context(), tenant_context());

    let mut transaction = scoped(runtime, Some(&alpha)).await;
    let alpha_job = enqueue_for_tenant(
        &mut transaction,
        &alpha,
        &Report,
        EnqueueOptions::new().unique_key("same"),
    )
    .await
    .expect("alpha enqueue")
    .id();
    transaction.commit().await.expect("commit");
    let mut transaction = scoped(runtime, Some(&beta)).await;
    // The same key in another tenant is a different job.
    let beta_job = enqueue_for_tenant(
        &mut transaction,
        &beta,
        &Report,
        EnqueueOptions::new().unique_key("same"),
    )
    .await
    .expect("beta enqueue");
    assert!(matches!(beta_job, Enqueued::New(_)));
    transaction.commit().await.expect("commit");
    let global_job = PgJobStore::new(runtime.clone())
        .enqueue(&Report, EnqueueOptions::new())
        .await
        .expect("global")
        .id();

    assert_eq!(
        visible(runtime, Some(&alpha)).await,
        vec![alpha_job.into_uuid()]
    );
    assert_eq!(
        visible(runtime, Some(&beta)).await,
        vec![beta_job.id().into_uuid()]
    );
    assert_eq!(visible(runtime, None).await, vec![global_job.into_uuid()]);

    // Enqueueing outside the tenant's scope is refused by the database.
    for context in [None, Some(&beta)] {
        let mut transaction = scoped(runtime, context).await;
        let refused =
            enqueue_for_tenant(&mut transaction, &alpha, &Report, EnqueueOptions::new()).await;
        assert!(
            matches!(refused, Err(JobsError::Database(_))),
            "{refused:?}"
        );
    }
    // A forged principal inside the right tenant is refused too.
    let mut transaction = scoped(runtime, Some(&alpha)).await;
    sqlx::query("SELECT set_config('forge.principal_id', $1, true)")
        .bind(PrincipalId::new().to_string())
        .execute(transaction.connection())
        .await
        .expect("forge principal");
    assert!(
        enqueue_for_tenant(&mut transaction, &alpha, &Report, EnqueueOptions::new())
            .await
            .is_err()
    );
    drop(transaction);

    // The worker claims across tenants, then runs each job in its own scope.
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut jobs = Jobs::new();
    jobs.register(ScopeProbe {
        database: runtime.clone(),
        seen: Arc::clone(&seen),
    })
    .expect("register");
    jobs.settings(fast());
    let (shutdown, handle) = spawn_runtime(jobs, runtime);
    wait_until("all reports", || async {
        count_state(&db, "completed").await == 3
    })
    .await;
    stop(shutdown, handle).await;

    let mut seen = seen.lock().expect("lock").clone();
    seen.sort();
    let mut expected = vec![
        (None, vec![global_job.into_uuid()]),
        (
            Some(alpha.tenant_id().to_string()),
            vec![alpha_job.into_uuid()],
        ),
        (
            Some(beta.tenant_id().to_string()),
            vec![beta_job.id().into_uuid()],
        ),
    ];
    expected.sort();
    assert_eq!(seen, expected);
    db.cleanup().await.expect("cleanup");
}

#[tokio::test]
#[ignore = "needs FORGE_TEST_DATABASE_URL"]
async fn transactional_completion_is_fenced_and_rolls_back() {
    let db = fresh().await;
    let store = PgJobStore::new(db.runtime().clone());
    let id = store
        .enqueue(&Count { n: 1 }, EnqueueOptions::new())
        .await
        .expect("enqueue")
        .id();
    let names = vec![Count::NAME.to_owned()];
    let token = LeaseToken::new();
    store
        .claim("default", &names, 1, Duration::from_secs(5), token)
        .await
        .expect("claim");
    let context = JobContext::new(
        id,
        Count::NAME,
        "default",
        1,
        10,
        None,
        token,
        Cancellation::new(),
    );

    let mut rolled_back = begin_job_transaction(store.database(), &context)
        .await
        .expect("begin");
    complete_in(&mut rolled_back, &context)
        .await
        .expect("complete");
    rolled_back.rollback().await.expect("rollback");
    assert_eq!(
        job_row(&db, id).await.state,
        "running",
        "rollback undoes the completion"
    );

    let stale = JobContext::new(
        id,
        Count::NAME,
        "default",
        1,
        10,
        None,
        LeaseToken::new(),
        Cancellation::new(),
    );
    let mut fenced = begin_job_transaction(store.database(), &stale)
        .await
        .expect("begin");
    assert!(matches!(
        complete_in(&mut fenced, &stale).await,
        Err(JobsError::LeaseLost)
    ));
    drop(fenced);

    let mut committed = begin_job_transaction(store.database(), &context)
        .await
        .expect("begin");
    complete_in(&mut committed, &context)
        .await
        .expect("complete");
    committed.commit().await.expect("commit");
    assert_eq!(job_row(&db, id).await.state, "completed");
    db.cleanup().await.expect("cleanup");
}

#[tokio::test]
#[ignore = "needs FORGE_TEST_DATABASE_URL"]
async fn pruning_is_bounded_and_respects_retention() {
    let db = fresh().await;
    let store = PgJobStore::new(db.runtime().clone());
    let names = vec![Count::NAME.to_owned()];
    for n in 0..25 {
        store
            .enqueue(&Count { n }, EnqueueOptions::new())
            .await
            .expect("enqueue");
    }
    let token = LeaseToken::new();
    for job in store
        .claim("default", &names, 25, Duration::from_secs(5), token)
        .await
        .expect("claim")
    {
        assert!(store.complete(job.id, token).await.expect("complete"));
    }
    let keep_all = Pruning::default();
    assert_eq!(
        store.prune(&keep_all).await.expect("prune"),
        0,
        "within retention"
    );
    let mut bounded = Pruning::default().completed_after(Duration::ZERO);
    bounded.batch = 10;
    bounded.max_batches = 2;
    assert_eq!(
        store.prune(&bounded).await.expect("prune"),
        20,
        "two batches of ten"
    );
    assert_eq!(store.prune(&bounded).await.expect("prune"), 5);
    assert_eq!(count_state(&db, "completed").await, 0);

    // Unique keys are reusable once their job is pruned.
    let first = store
        .enqueue(&Count { n: 1 }, EnqueueOptions::new().unique_key("u"))
        .await
        .expect("enqueue");
    let job = NewJob::of(
        &Count { n: 2 },
        &EnqueueOptions::new().unique_key("u"),
        None,
    )
    .expect("job");
    let mut transaction = db.runtime().begin().await.expect("begin");
    let duplicate = postgres::insert(transaction.connection(), &job)
        .await
        .expect("insert");
    assert_eq!(duplicate, Enqueued::Duplicate(first.id()));
    transaction.rollback().await.expect("rollback");
    db.cleanup().await.expect("cleanup");
}

#[tokio::test]
#[ignore = "needs FORGE_TEST_DATABASE_URL"]
async fn ticks_from_a_fast_replica_clock_are_refused_and_do_not_block_others() {
    let db = fresh().await;
    let store = PgJobStore::new(db.runtime().clone());
    let every_minute: Cron = "* * * * *".parse().expect("cron");
    let schedule =
        Schedule::new("skewed", every_minute.clone(), &Count { n: 0 }).expect("schedule");
    let now = store.database_now().await.expect("database clock");
    let current = every_minute.latest_at_or_before(now).expect("tick");
    // A replica whose clock runs an hour fast computes a future tick.
    let future =
        Timestamp::from_unix_micros(current.unix_micros() + 3_600_000_000).expect("future");
    let lateness = Duration::from_secs(300);
    assert_eq!(
        store
            .fire_tick(
                "skewed",
                future,
                lateness,
                schedule.job_for(future).expect("job")
            )
            .await
            .expect("future tick"),
        TickOutcome::Early
    );
    let recorded: i64 = sqlx::query_scalar("SELECT count(*) FROM forge_job_schedules")
        .fetch_one(db.admin().pool())
        .await
        .expect("count");
    assert_eq!(recorded, 0, "a future tick is not recorded");
    // The correctly clocked replica still fires the current tick.
    assert!(matches!(
        store
            .fire_tick(
                "skewed",
                current,
                lateness,
                schedule.job_for(current).expect("job")
            )
            .await
            .expect("current tick"),
        TickOutcome::Fired(_)
    ));
    assert_eq!(count_state(&db, "queued").await, 1);
    db.cleanup().await.expect("cleanup");
}

#[derive(Serialize, Deserialize)]
struct Wide(Vec<u8>);

impl Job for Wide {
    const NAME: &'static str = "test.wide";
}

#[tokio::test]
#[ignore = "needs FORGE_TEST_DATABASE_URL"]
async fn the_database_enforces_enqueue_columns_payload_size_and_reserved_keys() {
    let db = fresh().await;
    let runtime = db.runtime();
    let refused = |sql: &'static str| async move {
        sqlx::query(sql)
            .bind(JobId::new().into_uuid())
            .execute(runtime.pool())
            .await
            .map_err(DbError::from)
    };
    // State, attempts and finish times are not insertable by the runtime role.
    let forged = refused(
        "INSERT INTO forge_jobs (id, queue, job_name, payload, max_attempts, run_at, state, finished_at)
         VALUES ($1, 'default', 'test.count', '{}', 1, now(), 'completed', now())",
    )
    .await;
    assert!(matches!(forged, Err(DbError::Query(_))), "{forged:?}");
    // Schedule idempotency keys are reserved, in SQL and in the API.
    let squatted = refused(
        "INSERT INTO forge_jobs (id, queue, job_name, payload, max_attempts, run_at, unique_key)
         VALUES ($1, 'default', 'test.count', '{}', 1, now(), 'forge.schedule:nightly:60')",
    )
    .await;
    assert!(squatted.is_err(), "{squatted:?}");
    let store = PgJobStore::new(runtime.clone());
    assert!(matches!(
        store
            .enqueue(
                &Count { n: 1 },
                EnqueueOptions::new().unique_key("forge.schedule:x:1")
            )
            .await,
        Err(JobsError::InvalidSetting(_))
    ));
    // Schedule bookkeeping is reachable only through forge_jobs_fire_tick.
    let schedules =
        refused("INSERT INTO forge_job_schedules (name, last_tick_at) VALUES ($1::text, now())")
            .await;
    assert!(schedules.is_err());
    // 240 KB of compact JSON passes the client check but its stored form
    // ("1, 1, ...") exceeds the database cap.
    let wide = store
        .enqueue(&Wide(vec![1; 120_000]), EnqueueOptions::new())
        .await;
    assert!(
        matches!(wide, Err(JobsError::PayloadTooLarge { .. })),
        "{wide:?}"
    );
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM forge_jobs")
        .fetch_one(db.admin().pool())
        .await
        .expect("count");
    assert_eq!(total, 0);
    db.cleanup().await.expect("cleanup");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs FORGE_TEST_DATABASE_URL"]
async fn worker_functions_refuse_tenant_scope_and_can_be_split_from_the_runtime_role() {
    let db = RlsTestDatabase::create_with_worker(&[migrator()])
        .await
        .expect(IGNORE);
    let names = vec![Count::NAME.to_owned()];
    let lease = Duration::from_secs(5);

    // The request-serving runtime role enqueues but cannot claim or inspect.
    let serving = PgJobStore::new(db.runtime().clone());
    let first = serving
        .enqueue(&Count { n: 1 }, EnqueueOptions::new())
        .await
        .expect("enqueue")
        .id();
    assert!(
        serving
            .claim("default", &names, 1, lease, LeaseToken::new())
            .await
            .is_err()
    );
    assert!(serving.stats().await.is_err());
    assert!(serving.dead(10, None).await.is_err());

    // Even the worker role is refused inside a tenant-scoped transaction.
    let alpha = tenant_context();
    for sql in [
        "SELECT count(*) FROM forge_jobs_stats()",
        "SELECT count(*) FROM forge_jobs_dead(10, NULL)",
        "SELECT count(*) FROM forge_jobs_claim('default', ARRAY['test.count'], 1, '01a1567c-bf31-73d9-966c-0123456789ab', 5000)",
        "SELECT count(*) FROM forge_jobs_prune(0, NULL, 10)",
    ] {
        let mut transaction = scoped(db.worker(), Some(&alpha)).await;
        let refused = sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(sql))
            .fetch_one(transaction.connection())
            .await;
        assert!(refused.is_err(), "{sql} must be refused in tenant scope");
    }

    // The worker role claims; the runtime role still completes in its own
    // transaction (complete_in) with the lease token.
    let worker = PgJobStore::new(db.worker().clone());
    let token = LeaseToken::new();
    let claimed = worker
        .claim("default", &names, 1, lease, token)
        .await
        .expect("claim");
    assert_eq!(claimed[0].id, first);
    let context = JobContext::new(
        first,
        Count::NAME,
        "default",
        1,
        10,
        None,
        token,
        Cancellation::new(),
    );
    let mut transaction = begin_job_transaction(db.runtime(), &context)
        .await
        .expect("begin");
    complete_in(&mut transaction, &context)
        .await
        .expect("complete_in as runtime");
    transaction.commit().await.expect("commit");

    // A full runtime on the worker pool processes jobs.
    let second = serving
        .enqueue(&Count { n: 2 }, EnqueueOptions::new())
        .await
        .expect("enqueue")
        .id();
    let mut jobs = Jobs::new();
    jobs.register(Counter::default()).expect("register");
    jobs.settings(fast());
    let (shutdown, handle) = spawn_runtime(jobs, db.worker());
    wait_until("the worker role to run the job", || async {
        job_row(&db, second).await.state == "completed"
    })
    .await;
    stop(shutdown, handle).await;
    db.cleanup().await.expect("cleanup");
}

/// Runs a one-handler runtime on `database` and returns its result, which
/// must come back at once: the grant check runs before any loop starts.
async fn run_briefly(database: &PgDatabase) -> Result<(), JobsError> {
    let mut jobs = Jobs::new();
    jobs.register(Counter::default()).expect("register");
    jobs.settings(fast());
    let shutdown = Cancellation::new();
    let run = jobs.run(PgJobStore::new(database.clone()), shutdown.clone());
    match tokio::time::timeout(Duration::from_secs(5), run).await {
        Ok(result) => result,
        Err(_) => {
            shutdown.cancel();
            panic!("the runtime kept running instead of failing fast");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs FORGE_TEST_DATABASE_URL"]
async fn a_role_without_worker_grants_fails_fast_with_the_documented_message() {
    // The request-serving role of a split setup holds forge_jobs_complete
    // only: what `serve` meets when it runs with a role that was not
    // forge.runtime_role at `forge migrate`.
    let db = RlsTestDatabase::create_with_worker(&[migrator()])
        .await
        .expect(IGNORE);
    let error = run_briefly(db.runtime())
        .await
        .expect_err("a role that cannot claim must not start");
    assert!(
        matches!(&error, JobsError::WorkerNotGranted { role, .. } if role == db.runtime_role()),
        "{error:?}"
    );
    let message = error.to_string();
    for expected in [
        "cannot claim jobs",
        "lacks EXECUTE on",
        "forge_jobs_claim(",
        "GRANT EXECUTE ON FUNCTION forge_jobs_claim(",
        "PGOPTIONS='-c forge.runtime_role=",
    ] {
        assert!(message.contains(expected), "{message}");
    }
    assert!(!message.contains("forge_jobs_complete("), "{message}");
    // The GRANT the message names is the fix: with it the same role starts.
    let start = message.find("GRANT EXECUTE").expect("a GRANT statement");
    let end = start + message[start..].find(';').expect("statement end");
    sqlx::query(sqlx::AssertSqlSafe(message[start..end].to_owned()))
        .execute(db.admin().pool())
        .await
        .expect("the named GRANT applies");
    let mut jobs = Jobs::new();
    jobs.register(Counter::default()).expect("register");
    jobs.settings(fast());
    let (shutdown, handle) = spawn_runtime(jobs, db.runtime());
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!handle.is_finished(), "the granted role must keep running");
    stop(shutdown, handle).await;
    // The worker role holds every grant and starts.
    let mut jobs = Jobs::new();
    jobs.register(Counter::default()).expect("register");
    jobs.settings(fast());
    let (shutdown, handle) = spawn_runtime(jobs, db.worker());
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!handle.is_finished(), "the worker role must keep running");
    stop(shutdown, handle).await;
    db.cleanup().await.expect("cleanup");

    // Without the job schema there is nothing to grant: migrate first.
    let empty = RlsTestDatabase::create(&[]).await.expect(IGNORE);
    let message = run_briefly(empty.runtime())
        .await
        .expect_err("an unmigrated schema must not start")
        .to_string();
    assert!(
        message.contains("cannot claim jobs") && message.contains("run `forge migrate`"),
        "{message}"
    );
    empty.cleanup().await.expect("cleanup");
}

#[derive(Serialize, Deserialize)]
struct Holder {
    hold: bool,
}

impl Job for Holder {
    const NAME: &'static str = "test.holder";
}

#[derive(Default)]
struct HolderProbe {
    locked: AtomicUsize,
    committed: AtomicUsize,
    quick_done: AtomicUsize,
    cancelled_after_commit: AtomicUsize,
    lease: Mutex<Option<(JobId, LeaseToken)>>,
    /// Ends the hold: the test releases the transaction once it has seen
    /// what must happen while it is open, instead of the holder guessing how
    /// long that takes.
    release: tokio::sync::Notify,
}

/// `hold: true` completes in its own transaction and keeps it open, across
/// several heartbeats, until the probe's `release`; `hold: false` finishes
/// immediately.
struct HoldsCompletion {
    database: PgDatabase,
    probe: Arc<HolderProbe>,
}

#[async_trait]
impl JobHandler for HoldsCompletion {
    type Job = Holder;

    async fn perform(&self, context: &JobContext, job: Holder) -> Result<(), JobError> {
        if !job.hold {
            self.probe.quick_done.fetch_add(1, Ordering::SeqCst);
            return Ok(());
        }
        let mut transaction = begin_job_transaction(&self.database, context).await?;
        complete_in(&mut transaction, context).await?;
        *self.probe.lease.lock().expect("lock") = Some((context.id(), context.lease));
        self.probe.locked.fetch_add(1, Ordering::SeqCst);
        self.probe.release.notified().await;
        transaction.commit().await?;
        self.probe.committed.fetch_add(1, Ordering::SeqCst);
        // Heartbeats after the commit see the lease gone; that is not a loss.
        tokio::time::sleep(Duration::from_millis(2500)).await;
        if context.cancellation().is_cancelled() {
            self.probe
                .cancelled_after_commit
                .fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs FORGE_TEST_DATABASE_URL"]
async fn renewal_skips_rows_locked_by_transactional_completion() {
    let db = fresh().await;
    let store = PgJobStore::new(db.runtime().clone());
    let probe = Arc::new(HolderProbe::default());
    store
        .enqueue(&Holder { hold: true }, EnqueueOptions::new())
        .await
        .expect("enqueue");
    let mut jobs = Jobs::new();
    jobs.register(HoldsCompletion {
        database: db.runtime().clone(),
        probe: Arc::clone(&probe),
    })
    .expect("register");
    jobs.concurrency("default", 2).expect("concurrency");
    jobs.settings(fast()); // 3 s lease: renewal every second
    let (shutdown, handle) = spawn_runtime(jobs, store.database());
    wait_until("the completion transaction to hold its row", || async {
        probe.locked.load(Ordering::SeqCst) == 1
    })
    .await;
    // Renewing the locked row answers at once, as busy rather than lost.
    let held = probe.lease.lock().expect("lock").expect("lease");
    let renewal = tokio::time::timeout(
        Duration::from_millis(500),
        store.heartbeat(&[held], Duration::from_secs(3)),
    )
    .await
    .expect("renewal does not wait for the row lock")
    .expect("heartbeat");
    assert_eq!(renewal, vec![(held.0, LeaseStatus::Busy)]);
    // Let a renewal hit the locked row, then enqueue more work: the worker
    // loop must not be stuck behind that row lock.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    store
        .enqueue(&Holder { hold: false }, EnqueueOptions::new())
        .await
        .expect("enqueue");
    wait_until("the second job", || async {
        probe.quick_done.load(Ordering::SeqCst) == 1
    })
    .await;
    assert_eq!(
        probe.committed.load(Ordering::SeqCst),
        0,
        "the second job ran while the first transaction still held its row"
    );
    probe.release.notify_one();
    wait_until("the holder to finish", || async {
        count_state(&db, "completed").await == 2
    })
    .await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    stop(shutdown, handle).await;
    assert_eq!(probe.committed.load(Ordering::SeqCst), 1);
    assert_eq!(
        probe.cancelled_after_commit.load(Ordering::SeqCst),
        0,
        "a lease ended by complete_in is not reported as lost"
    );
    db.cleanup().await.expect("cleanup");
}

#[derive(Serialize, Deserialize)]
struct Quick;

impl Job for Quick {
    const NAME: &'static str = "test.quick";
    const QUEUE: &'static str = "long";
}

struct QuickHandler(Arc<AtomicUsize>);

#[async_trait]
impl JobHandler for QuickHandler {
    type Job = Quick;

    async fn perform(&self, _: &JobContext, _: Quick) -> Result<(), JobError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// Makes every lease renewal fail, like an unreachable database would.
async fn break_renewal(db: &RlsTestDatabase) {
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "REVOKE EXECUTE ON FUNCTION forge_jobs_heartbeat(uuid[], uuid[], bigint) FROM \"{}\"",
        db.runtime_role()
    )))
    .execute(db.admin().pool())
    .await
    .expect("revoke");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs FORGE_TEST_DATABASE_URL"]
async fn attempts_are_cancelled_when_their_lease_cannot_be_renewed() {
    let db = fresh().await;
    let store = PgJobStore::new(db.runtime().clone());
    let id = store
        .enqueue(&Long { cooperative: true }, EnqueueOptions::new())
        .await
        .expect("enqueue")
        .id();
    let started = Arc::new(AtomicUsize::new(0));
    let mut jobs = Jobs::new();
    jobs.register(Lingers(Arc::clone(&started)))
        .expect("register");
    jobs.settings(fast()); // 3 s lease
    let (shutdown, handle) = spawn_runtime(jobs, store.database());
    wait_until("the attempt to start", || async {
        started.load(Ordering::SeqCst) == 1
    })
    .await;
    break_renewal(&db).await;
    let begun = std::time::Instant::now();
    // The attempt stops before its 3 s lease could expire, and its failure
    // is recorded while the lease is still held.
    wait_until("the attempt to be cancelled", || async {
        job_row(&db, id).await.last_error.as_deref() == Some("cancelled")
    })
    .await;
    assert!(
        begun.elapsed() < Duration::from_secs(4),
        "{:?}",
        begun.elapsed()
    );
    stop(shutdown, handle).await;
    db.cleanup().await.expect("cleanup");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs FORGE_TEST_DATABASE_URL"]
async fn attempts_ignoring_cancellation_after_losing_the_lease_free_their_slot() {
    let db = fresh().await;
    let store = PgJobStore::new(db.runtime().clone());
    store
        .enqueue(&Long { cooperative: false }, EnqueueOptions::new())
        .await
        .expect("enqueue");
    let started = Arc::new(AtomicUsize::new(0));
    let quick = Arc::new(AtomicUsize::new(0));
    let mut jobs = Jobs::new();
    jobs.register(Lingers(Arc::clone(&started)))
        .expect("register");
    jobs.register(QuickHandler(Arc::clone(&quick)))
        .expect("register");
    jobs.concurrency("long", 1).expect("concurrency");
    jobs.settings(fast());
    let (shutdown, handle) = spawn_runtime(jobs, store.database());
    wait_until("the stubborn attempt to start", || async {
        started.load(Ordering::SeqCst) == 1
    })
    .await;
    break_renewal(&db).await;
    store
        .enqueue(&Quick, EnqueueOptions::new())
        .await
        .expect("enqueue");
    // The stubborn attempt (TIMEOUT 5 minutes) is cancelled, ignores it, and
    // is aborted after the cancel grace: the only slot runs the next job.
    wait_until("the queued job to get the slot", || async {
        quick.load(Ordering::SeqCst) == 1
    })
    .await;
    stop(shutdown, handle).await;
    db.cleanup().await.expect("cleanup");
}

// ----------------------------------------------------------- telemetry

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs FORGE_TEST_DATABASE_URL"]
async fn attempts_are_traced_and_measured_by_outcome() {
    use forge_telemetry::{
        Options, SpanSink, Telemetry,
        testing::{Behavior, Receiver},
    };

    let receiver = Receiver::start(Behavior::Accept).await;
    let mut options = Options::new("worker");
    options.spans = Some(SpanSink::Otlp(receiver.client(
        http::HeaderMap::new(),
        false,
        Duration::from_secs(10),
    )));
    let telemetry = Telemetry::new(options).expect("telemetry");
    telemetry.start();

    let db = fresh().await;
    let store = PgJobStore::new(db.runtime().clone());
    let ok = store
        .enqueue(&Count { n: 1 }, EnqueueOptions::new())
        .await
        .expect("enqueue")
        .id();
    let flaky = store
        .enqueue(&Flaky, EnqueueOptions::new())
        .await
        .expect("enqueue")
        .id();
    let boom = store
        .enqueue(&Explode, EnqueueOptions::new())
        .await
        .expect("enqueue")
        .id();
    let mut jobs = Jobs::new();
    jobs.register(Counter::default()).expect("register");
    jobs.register(AlwaysFails(Arc::new(AtomicUsize::new(0))))
        .expect("register");
    jobs.register(Panics).expect("register");
    jobs.settings(fast());
    jobs.instrument(telemetry.jobs().expect("instruments"));
    let (shutdown, handle) = spawn_runtime(jobs, store.database());
    wait_until("every job to settle", || async {
        job_row(&db, ok).await.state == "completed"
            && job_row(&db, flaky).await.state == "dead"
            && job_row(&db, boom).await.state == "dead"
    })
    .await;
    stop(shutdown, handle).await;
    telemetry.force_flush(Duration::from_secs(10)).await;

    let text = forge_telemetry::prometheus::render(telemetry.metrics());
    for (kind, outcome, count) in [
        ("test.count", "succeeded", 1),
        ("test.flaky", "retry", 2),
        ("test.flaky", "dead", 1),
        ("test.explode", "dead", 1),
    ] {
        let line = format!(
            "forge_queue_job_duration_seconds_count{{messaging_destination_name=\"default\",forge_job_kind=\"{kind}\",forge_job_outcome=\"{outcome}\"}} {count}"
        );
        assert!(text.contains(&line), "{line}\n{text}");
    }
    let spans = receiver
        .received("/v1/traces")
        .iter()
        .map(|request| request.body.to_string())
        .collect::<String>();
    assert!(spans.contains("\"process default\""), "{spans}");
    assert!(
        spans.contains(&ok.to_string()),
        "messaging.message.id: {spans}"
    );
    assert!(spans.contains("\"forge.job.outcome\""), "{spans}");
    assert!(!spans.contains("secret payload"), "{spans}");
    db.cleanup().await.expect("cleanup");
}

#[tokio::test]
#[ignore = "needs FORGE_TEST_DATABASE_URL"]
async fn an_unavailable_database_is_an_error_never_a_lost_lease() {
    let db = fresh().await;
    let store = PgJobStore::new(db.runtime().clone());
    let rename = async |from: &str, to: &str| {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "ALTER TABLE {from} RENAME TO {to}"
        )))
        .execute(db.admin().pool())
        .await
        .expect("rename the job table");
    };
    crate::testing::run_unavailable(
        &store,
        async || rename("forge_jobs", "forge_jobs_away").await,
        async || rename("forge_jobs_away", "forge_jobs").await,
    )
    .await;
    db.cleanup().await.expect("cleanup");
}
