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
        self, JobState, NewJob, PgJobStore, Pruning, TickOutcome, begin_job_transaction,
        complete_in, enqueue, enqueue_for_tenant, migrator,
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
         WHERE proname LIKE 'forge\\_jobs\\_%' AND (NOT prosecdef OR proconfig IS NULL)",
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
    assert!(
        store
            .heartbeat(&[(id, stale)], lease)
            .await
            .expect("heartbeat")
            .is_empty()
    );
    assert_eq!(
        store
            .heartbeat(&[(id, fresh_token)], lease)
            .await
            .expect("heartbeat"),
        vec![id]
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
