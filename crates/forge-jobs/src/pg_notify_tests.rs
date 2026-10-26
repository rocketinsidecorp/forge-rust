//! PostgreSQL tests of enqueue announcements, the workers' `LISTEN` and claim
//! batches (ADR 0046), run as the least-privilege runtime role.
//!
//! `#[ignore]`d unless requested: `FORGE_TEST_DATABASE_URL=postgres://...
//! cargo test -p forge-jobs -- --include-ignored`.

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use async_trait::async_trait;
use forge_config::SecretString;
use forge_core::Cancellation;
use forge_db::PgDatabase;
use serde::{Deserialize, Serialize};
use sqlx::postgres::PgListener;
use tokio::{
    sync::{Semaphore, mpsc},
    task::JoinHandle,
};

use crate::{
    EnqueueOptions, Enqueued, Job, JobContext, JobError, JobHandler, JobId, Jobs, JobsError,
    WorkerSettings,
    postgres::{
        JobSpec, NOTIFY_CHANNEL, NOTIFY_SETTING, NewJob, PgJobStore, TickOutcome, enqueue, migrator,
    },
    testing::RlsTestDatabase,
};

const IGNORE: &str = "needs FORGE_TEST_DATABASE_URL";
/// How long a test waits for something that should not happen.
const QUIET: Duration = Duration::from_millis(400);

async fn fresh() -> RlsTestDatabase {
    RlsTestDatabase::create(&[migrator()]).await.expect(IGNORE)
}

/// A runtime-role pool whose connections carry `forge.jobs_notify=on`, set
/// as the documented URL option.
async fn notifying(db: &RlsTestDatabase) -> PgDatabase {
    let url = db.runtime_url().expose();
    let separator = if url.contains('?') { '&' } else { '?' };
    let url = SecretString::new(format!(
        "{url}{separator}options=-c%20forge.jobs_notify%3Don"
    ));
    PgDatabase::connect(&url).await.expect("notifying pool")
}

/// Settings whose idle polls are far apart, so a quick claim needs an
/// announcement. The lease is long too: the worker loop also claims after
/// each lease renewal (every third of the lease).
fn slow_polling(poll: Duration) -> WorkerSettings {
    WorkerSettings::default()
        .poll_interval(poll)
        .lease(Duration::from_secs(180))
        .drain_timeout(Duration::from_secs(2))
        .cancel_grace(Duration::from_millis(200))
}

fn spawn_runtime(jobs: Jobs, db: &PgDatabase) -> (Cancellation, JoinHandle<Result<(), JobsError>>) {
    let shutdown = Cancellation::new();
    let handle = tokio::spawn(jobs.run(PgJobStore::new(db.clone()), shutdown.clone()));
    (shutdown, handle)
}

async fn stop(shutdown: Cancellation, handle: JoinHandle<Result<(), JobsError>>) -> Duration {
    let started = Instant::now();
    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(30), handle)
        .await
        .expect("runtime stops in time")
        .expect("runtime task")
        .expect("runtime result");
    started.elapsed()
}

async fn wait_until<F, Fut>(what: &str, limit: Duration, mut condition: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = Instant::now() + limit;
    while !condition().await {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Backends of this test's database whose last statement subscribed to the
/// job channel.
async fn listen_backends(db: &RlsTestDatabase) -> Vec<i32> {
    sqlx::query_scalar(
        "SELECT pid FROM pg_stat_activity
         WHERE datname = current_database() AND pid <> pg_backend_pid()
           AND query = 'LISTEN \"forge_jobs\"'
         ORDER BY pid",
    )
    .fetch_all(db.admin().pool())
    .await
    .expect("pg_stat_activity")
}

async fn admin_listener(db: &RlsTestDatabase) -> PgListener {
    let mut listener = PgListener::connect_with(db.admin().pool())
        .await
        .expect("listener");
    listener.listen(NOTIFY_CHANNEL).await.expect("LISTEN");
    listener
}

/// The next announcement's payload, or `None` when none arrives in `wait`.
async fn next_announcement(listener: &mut PgListener, wait: Duration) -> Option<String> {
    tokio::time::timeout(wait, listener.recv())
        .await
        .ok()
        .map(|notification| notification.expect("notification").payload().to_owned())
}

#[derive(Serialize, Deserialize)]
struct Ping;

impl Job for Ping {
    const NAME: &'static str = "notify.ping";
}

#[derive(Serialize, Deserialize)]
struct Mail;

impl Job for Mail {
    const NAME: &'static str = "notify.mail";
    const QUEUE: &'static str = "mail";
}

/// Reports when each attempt starts.
struct Started(mpsc::UnboundedSender<Instant>);

#[async_trait]
impl JobHandler for Started {
    type Job = Ping;

    async fn perform(&self, _: &JobContext, _: Ping) -> Result<(), JobError> {
        let _ = self.0.send(Instant::now());
        Ok(())
    }
}

fn started_runtime(settings: WorkerSettings) -> (Jobs, mpsc::UnboundedReceiver<Instant>) {
    let (sender, receiver) = mpsc::unbounded_channel();
    let mut jobs = Jobs::new();
    jobs.register(Started(sender))
        .expect("register")
        .settings(settings)
        .pruning(None);
    (jobs, receiver)
}

async fn enqueue_ping(store: &PgJobStore) -> Instant {
    let enqueued = Instant::now();
    assert!(matches!(
        store.enqueue(&Ping, EnqueueOptions::new()).await,
        Ok(Enqueued::New(_))
    ));
    enqueued
}

async fn next_start(started: &mut mpsc::UnboundedReceiver<Instant>, wait: Duration) -> Instant {
    tokio::time::timeout(wait, started.recv())
        .await
        .expect("the job started in time")
        .expect("handler alive")
}

#[tokio::test]
#[ignore = "needs FORGE_TEST_DATABASE_URL"]
async fn only_due_committed_jobs_are_announced_by_sessions_that_opt_in() {
    let db = fresh().await;
    let database = notifying(&db).await;
    let store = PgJobStore::new(database.clone());
    let mut listener = admin_listener(&db).await;

    // A due job, committed: announced with its queue as the payload.
    let mut transaction = database.begin().await.expect("begin");
    enqueue(
        &mut transaction,
        &Ping,
        EnqueueOptions::new().unique_key("once"),
    )
    .await
    .expect("enqueue");
    assert_eq!(next_announcement(&mut listener, QUIET).await, None);
    transaction.commit().await.expect("commit");
    assert_eq!(
        next_announcement(&mut listener, Duration::from_secs(2)).await,
        Some("default".to_owned())
    );

    // A duplicate stores nothing and announces nothing; neither does a job
    // due later (polling finds it), nor one rolled back.
    assert!(matches!(
        store
            .enqueue(&Ping, EnqueueOptions::new().unique_key("once"))
            .await,
        Ok(Enqueued::Duplicate(_))
    ));
    store
        .enqueue(
            &Ping,
            EnqueueOptions::new().delay(Duration::from_secs(3600)),
        )
        .await
        .expect("delayed");
    let mut transaction = database.begin().await.expect("begin");
    enqueue(&mut transaction, &Mail, EnqueueOptions::new())
        .await
        .expect("enqueue");
    transaction.rollback().await.expect("rollback");
    assert_eq!(next_announcement(&mut listener, QUIET).await, None);

    // One announcement per queue and transaction, however many jobs.
    let mut transaction = database.begin().await.expect("begin");
    for _ in 0..3 {
        enqueue(&mut transaction, &Mail, EnqueueOptions::new())
            .await
            .expect("enqueue");
    }
    transaction.commit().await.expect("commit");
    assert_eq!(
        next_announcement(&mut listener, Duration::from_secs(2)).await,
        Some("mail".to_owned())
    );
    assert_eq!(next_announcement(&mut listener, QUIET).await, None);

    // Schedule ticks are enqueued by forge_jobs_fire_tick: announced too.
    let tick = store.database_now().await.expect("now");
    let job = NewJob::from_parts(
        JobSpec::of::<Ping>(),
        "null".to_owned(),
        &EnqueueOptions::new().unique_key("forge.schedule:notify:1"),
        None,
    )
    .expect("tick job");
    assert!(matches!(
        store
            .fire_tick("notify", tick, Duration::from_secs(60), job)
            .await,
        Ok(TickOutcome::Fired(_))
    ));
    assert_eq!(
        next_announcement(&mut listener, Duration::from_secs(2)).await,
        Some("default".to_owned())
    );

    // Without the setting nothing is announced, also within a transaction
    // that turns it off; a transaction can turn it on by itself.
    let plain = PgJobStore::new(db.runtime().clone());
    plain
        .enqueue(&Ping, EnqueueOptions::new())
        .await
        .expect("plain enqueue");
    let mut transaction = database.begin().await.expect("begin");
    sqlx::query("SELECT set_config($1, 'off', true)")
        .bind(NOTIFY_SETTING)
        .execute(transaction.connection())
        .await
        .expect("off");
    enqueue(&mut transaction, &Ping, EnqueueOptions::new())
        .await
        .expect("enqueue");
    transaction.commit().await.expect("commit");
    assert_eq!(next_announcement(&mut listener, QUIET).await, None);
    let mut transaction = db.runtime().begin().await.expect("begin");
    sqlx::query("SELECT set_config($1, 'TRUE', true)")
        .bind(NOTIFY_SETTING)
        .execute(transaction.connection())
        .await
        .expect("on");
    enqueue(&mut transaction, &Mail, EnqueueOptions::new())
        .await
        .expect("enqueue");
    transaction.commit().await.expect("commit");
    assert_eq!(
        next_announcement(&mut listener, Duration::from_secs(2)).await,
        Some("mail".to_owned())
    );

    assert!(store.notifies().await.expect("notifies"));
    assert!(!plain.notifies().await.expect("notifies"));

    drop(listener);
    database.close().await;
    db.cleanup().await.expect("cleanup");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs FORGE_TEST_DATABASE_URL"]
async fn an_announcement_wakes_an_idle_worker_well_before_its_poll() {
    let db = fresh().await;
    let database = notifying(&db).await;
    let store = PgJobStore::new(database.clone());
    let (jobs, mut started) = started_runtime(slow_polling(Duration::from_secs(60)));
    let (shutdown, handle) = spawn_runtime(jobs, &database);
    wait_until("the listener", Duration::from_secs(10), || async {
        listen_backends(&db).await.len() == 1
    })
    .await;
    // The catch-up claim after subscribing found nothing.
    tokio::time::sleep(Duration::from_millis(200)).await;

    for _ in 0..3 {
        let enqueued = enqueue_ping(&store).await;
        let at = next_start(&mut started, Duration::from_secs(5)).await;
        let latency = at.duration_since(enqueued);
        assert!(
            latency < Duration::from_secs(2),
            "an announced job started after {latency:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    stop(shutdown, handle).await;
    database.close().await;
    db.cleanup().await.expect("cleanup");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs FORGE_TEST_DATABASE_URL"]
async fn polling_continues_while_the_listener_is_down_and_it_reconnects() {
    let db = fresh().await;
    let database = notifying(&db).await;
    let store = PgJobStore::new(database.clone());
    let (jobs, mut started) = started_runtime(slow_polling(Duration::from_millis(300)));
    let (shutdown, handle) = spawn_runtime(jobs, &database);
    wait_until("the listener", Duration::from_secs(10), || async {
        listen_backends(&db).await.len() == 1
    })
    .await;
    let first = listen_backends(&db).await;

    let terminated: bool = sqlx::query_scalar("SELECT pg_terminate_backend($1)")
        .bind(first[0])
        .fetch_one(db.admin().pool())
        .await
        .expect("terminate");
    assert!(terminated);
    wait_until("the listener to go", Duration::from_secs(5), || async {
        listen_backends(&db).await.is_empty()
    })
    .await;
    // No subscription now: polling alone runs the job.
    enqueue_ping(&store).await;
    next_start(&mut started, Duration::from_secs(5)).await;

    // The listener comes back on its own after 0.5 to 1 s, on a new backend.
    let lost = Instant::now();
    wait_until(
        "the listener to return",
        Duration::from_secs(10),
        || async {
            let now = listen_backends(&db).await;
            now.len() == 1 && now != first
        },
    )
    .await;
    assert!(
        lost.elapsed() < Duration::from_secs(5),
        "{:?}",
        lost.elapsed()
    );
    enqueue_ping(&store).await;
    next_start(&mut started, Duration::from_secs(5)).await;

    stop(shutdown, handle).await;
    database.close().await;
    db.cleanup().await.expect("cleanup");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs FORGE_TEST_DATABASE_URL"]
async fn shutdown_closes_an_active_listener_promptly_and_plain_runtimes_never_listen() {
    let db = fresh().await;
    let database = notifying(&db).await;
    let (jobs, _started) = started_runtime(slow_polling(Duration::from_secs(60)));
    let (shutdown, handle) = spawn_runtime(jobs, &database);
    wait_until("the listener", Duration::from_secs(10), || async {
        listen_backends(&db).await.len() == 1
    })
    .await;
    let took = stop(shutdown, handle).await;
    assert!(took < Duration::from_secs(2), "shutdown took {took:?}");
    wait_until(
        "the LISTEN session to end",
        Duration::from_secs(2),
        || async { listen_backends(&db).await.is_empty() },
    )
    .await;

    // Without forge.jobs_notify the runtime only polls.
    let (jobs, _started) = started_runtime(slow_polling(Duration::from_secs(60)));
    let (shutdown, handle) = spawn_runtime(jobs, db.runtime());
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(listen_backends(&db).await.is_empty());
    stop(shutdown, handle).await;

    database.close().await;
    db.cleanup().await.expect("cleanup");
}

// ---------------------------------------------------------------- batches

#[derive(Serialize, Deserialize)]
struct Held;

impl Job for Held {
    const NAME: &'static str = "notify.held";
}

/// Holds every attempt until the test hands out a permit.
struct Gate {
    permits: Arc<Semaphore>,
    running: Arc<AtomicUsize>,
    most: Arc<AtomicUsize>,
}

#[async_trait]
impl JobHandler for Gate {
    type Job = Held;

    async fn perform(&self, _: &JobContext, _: Held) -> Result<(), JobError> {
        let now = self.running.fetch_add(1, Ordering::SeqCst) + 1;
        self.most.fetch_max(now, Ordering::SeqCst);
        if let Ok(permit) = self.permits.acquire().await {
            permit.forget();
        }
        self.running.fetch_sub(1, Ordering::SeqCst);
        Ok(())
    }
}

/// Jobs per lease token among running jobs: each claim leases its jobs
/// under one token.
async fn claim_sizes(db: &RlsTestDatabase) -> Vec<i64> {
    sqlx::query_scalar(
        "SELECT count(*) FROM forge_jobs WHERE state = 'running'
         GROUP BY lease_token ORDER BY 1 DESC",
    )
    .fetch_all(db.admin().pool())
    .await
    .expect("claims")
}

async fn completed(db: &RlsTestDatabase) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM forge_jobs WHERE state = 'completed'")
        .fetch_one(db.admin().pool())
        .await
        .expect("count")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs FORGE_TEST_DATABASE_URL"]
async fn claims_never_exceed_the_batch_or_the_free_slots() {
    let db = fresh().await;
    let store = PgJobStore::new(db.runtime().clone());
    for (concurrency, batch, total) in [(5_u16, 2_u16, 12_usize), (3, 10, 7)] {
        let mut ids: Vec<JobId> = Vec::new();
        for _ in 0..total {
            if let Ok(Enqueued::New(id)) = store.enqueue(&Held, EnqueueOptions::new()).await {
                ids.push(id);
            }
        }
        assert_eq!(ids.len(), total);
        let permits = Arc::new(Semaphore::new(0));
        let most = Arc::new(AtomicUsize::new(0));
        let mut jobs = Jobs::new();
        jobs.register(Gate {
            permits: Arc::clone(&permits),
            running: Arc::new(AtomicUsize::new(0)),
            most: Arc::clone(&most),
        })
        .expect("register")
        .settings(
            slow_polling(Duration::from_secs(60))
                .default_concurrency(concurrency)
                .batch(batch),
        )
        .pruning(None);
        let (shutdown, handle) = spawn_runtime(jobs, db.runtime());

        let slots = usize::from(concurrency);
        wait_until("every slot to fill", Duration::from_secs(10), || async {
            most.load(Ordering::SeqCst) == slots
        })
        .await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        let sizes = claim_sizes(&db).await;
        assert_eq!(
            sizes.iter().sum::<i64>(),
            i64::from(concurrency),
            "{sizes:?}"
        );
        let bound = i64::from(batch.min(concurrency));
        assert!(sizes.iter().all(|size| *size <= bound), "{sizes:?}");
        assert_eq!(sizes[0], bound, "a claim fills a batch: {sizes:?}");

        // One free slot: the next claim asks for one job, not a batch.
        permits.add_permits(1);
        wait_until("a completion", Duration::from_secs(5), || async {
            completed(&db).await >= 1
        })
        .await;
        wait_until("the slot to refill", Duration::from_secs(5), || async {
            claim_sizes(&db).await.iter().sum::<i64>() == i64::from(concurrency)
        })
        .await;
        let sizes = claim_sizes(&db).await;
        assert!(sizes.iter().all(|size| *size <= bound), "{sizes:?}");
        assert!(sizes.contains(&1), "{sizes:?}");

        // A full batch is followed by another claim at once, not by an idle
        // poll (a minute here): every job runs.
        permits.add_permits(total);
        wait_until("every job", Duration::from_secs(10), || async {
            usize::try_from(completed(&db).await).unwrap_or(0) == total
        })
        .await;
        assert_eq!(most.load(Ordering::SeqCst), slots, "never above the slots");
        stop(shutdown, handle).await;
        sqlx::query("DELETE FROM forge_jobs")
            .execute(db.admin().pool())
            .await
            .expect("reset");
    }
    db.cleanup().await.expect("cleanup");
}

/// Waits until `parties` attempts run at once, so a worker that claimed one
/// batch and then idled would never let them finish.
struct Rendezvous(Arc<tokio::sync::Barrier>);

#[async_trait]
impl JobHandler for Rendezvous {
    type Job = Held;

    async fn perform(&self, _: &JobContext, _: Held) -> Result<(), JobError> {
        self.0.wait().await;
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs FORGE_TEST_DATABASE_URL"]
async fn a_full_batch_is_followed_by_another_claim_at_once() {
    let db = fresh().await;
    let store = PgJobStore::new(db.runtime().clone());
    for _ in 0..4 {
        store
            .enqueue(&Held, EnqueueOptions::new())
            .await
            .expect("enqueue");
    }
    let mut jobs = Jobs::new();
    jobs.register(Rendezvous(Arc::new(tokio::sync::Barrier::new(4))))
        .expect("register")
        .settings(
            slow_polling(Duration::from_secs(60))
                .default_concurrency(4)
                .batch(1),
        )
        .pruning(None);
    let (shutdown, handle) = spawn_runtime(jobs, db.runtime());
    wait_until("four one-job claims", Duration::from_secs(5), || async {
        completed(&db).await == 4
    })
    .await;
    stop(shutdown, handle).await;
    db.cleanup().await.expect("cleanup");
}
