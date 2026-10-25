//! A worker whose database fails mid-attempt: every way the outcome cannot
//! be recorded, and every way a lease is lost, ends with the job running
//! again after the lease expires (never silently lost, never completed
//! twice) and with a log line that says what happened. The tests need
//! `FORGE_TEST_DATABASE_URL` and `--include-ignored`.
//!
//! The outage is deterministic: the job table is renamed away inside the
//! test database and back. One global subscriber collects the worker's log
//! lines; the tests run one at a time because they share it.

use std::{
    sync::{
        Arc, Mutex, Once, OnceLock,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use forge_core::Cancellation;
use forge_jobs::{
    Backoff, EnqueueOptions, Job, JobContext, JobError, JobHandler, JobId, Jobs, JobsError,
    WorkerSettings,
    postgres::{PgJobStore, migrator},
    testing::RlsTestDatabase,
};
use serde::{Deserialize, Serialize};
use tokio::{sync::Notify, task::JoinHandle};

const IGNORE: &str = "needs FORGE_TEST_DATABASE_URL";
const PATIENCE: Duration = Duration::from_secs(60);

static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
static LINES: OnceLock<Arc<Mutex<Vec<String>>>> = OnceLock::new();

/// A minimal subscriber: spans get identifiers, events become lines.
struct Collect {
    lines: Arc<Mutex<Vec<String>>>,
    next: AtomicU64,
}

struct Fields(String);

impl tracing::field::Visit for Fields {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.0.insert_str(0, &format!("{value:?} | "));
        } else {
            self.0.push_str(&format!("{}={value:?} ", field.name()));
        }
    }
}

impl tracing::Subscriber for Collect {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(self.next.fetch_add(1, Ordering::Relaxed) + 1)
    }

    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        let mut fields = Fields(String::new());
        event.record(&mut fields);
        self.lines.lock().expect("lock").push(fields.0);
    }

    fn enter(&self, _: &tracing::span::Id) {}

    fn exit(&self, _: &tracing::span::Id) {}
}

fn lines() -> &'static Arc<Mutex<Vec<String>>> {
    static INSTALL: Once = Once::new();
    let lines = LINES.get_or_init(Arc::default);
    INSTALL.call_once(|| {
        tracing::subscriber::set_global_default(Collect {
            lines: Arc::clone(lines),
            next: AtomicU64::new(0),
        })
        .expect("the only global subscriber of this binary");
    });
    lines
}

/// Waits until a log line holds every text in `needles`.
async fn logged(needles: &[&str]) -> String {
    let deadline = tokio::time::Instant::now() + PATIENCE;
    loop {
        let found = lines()
            .lock()
            .expect("lock")
            .iter()
            .find(|line| needles.iter().all(|needle| line.contains(needle)))
            .cloned();
        if let Some(line) = found {
            return line;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "never logged {needles:?}: {:#?}",
            lines().lock().expect("lock")
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[derive(Serialize, Deserialize)]
struct Task;

impl Job for Task {
    const NAME: &'static str = "outage.task";
    const MAX_ATTEMPTS: u16 = 5;
    const BACKOFF: Backoff = Backoff::fixed(Duration::ZERO);
}

/// What the first attempt does once it runs; later attempts succeed at once.
#[derive(Clone, Copy)]
enum First {
    /// Waits for the test's go, then succeeds.
    Succeed,
    /// Waits for the test's go, then fails.
    Fail,
    /// Waits for the test's go, then panics.
    Panic,
    /// Waits for the test's go whatever happens, then succeeds.
    HoldThenSucceed,
    /// Runs until cancelled, then fails.
    UntilCancelled,
    /// Never returns and ignores cancellation.
    Forever,
}

struct Gate {
    first: First,
    started: Arc<Notify>,
    go: Arc<Notify>,
    runs: Arc<AtomicUsize>,
}

#[async_trait]
impl JobHandler for Gate {
    type Job = Task;

    async fn perform(&self, context: &JobContext, _: Task) -> Result<(), JobError> {
        if self.runs.fetch_add(1, Ordering::SeqCst) > 0 {
            return Ok(());
        }
        self.started.notify_one();
        match self.first {
            First::Succeed | First::HoldThenSucceed => {
                self.go.notified().await;
                Ok(())
            }
            First::Fail => {
                self.go.notified().await;
                Err(JobError::retry("the first attempt fails"))
            }
            First::Panic => {
                self.go.notified().await;
                panic!("the first attempt panics");
            }
            First::UntilCancelled => {
                context.cancellation().cancelled().await;
                Err(JobError::retry("cancelled"))
            }
            First::Forever => std::future::pending().await,
        }
    }
}

struct Rig {
    db: RlsTestDatabase,
    store: PgJobStore,
    shutdown: Cancellation,
    running: Option<JoinHandle<Result<(), JobsError>>>,
    started: Arc<Notify>,
    go: Arc<Notify>,
    runs: Arc<AtomicUsize>,
}

/// An attempt that honors cancellation gets a generous grace (a loaded
/// machine must not turn a prompt return into an abort); one that never
/// returns is aborted after a short one.
fn settings(first: First) -> WorkerSettings {
    let grace = if matches!(first, First::Forever) {
        Duration::from_millis(200)
    } else {
        Duration::from_secs(30)
    };
    WorkerSettings::default()
        .poll_interval(Duration::from_millis(50))
        .lease(Duration::from_secs(3))
        .drain_timeout(Duration::from_millis(200))
        .cancel_grace(grace)
}

async fn rig(first: First) -> Rig {
    lines().lock().expect("lock").clear();
    let db = RlsTestDatabase::create(&[migrator()]).await.expect(IGNORE);
    let store = PgJobStore::new(db.runtime().clone());
    let (started, go, runs) = (Arc::default(), Arc::default(), Arc::default());
    let mut jobs = Jobs::new();
    jobs.register(Gate {
        first,
        started: Arc::clone(&started),
        go: Arc::clone(&go),
        runs: Arc::clone(&runs),
    })
    .expect("register");
    jobs.settings(settings(first));
    let shutdown = Cancellation::new();
    let running = Some(tokio::spawn(jobs.run(store.clone(), shutdown.clone())));
    Rig {
        db,
        store,
        shutdown,
        running,
        started,
        go,
        runs,
    }
}

impl Rig {
    async fn enqueue(&self) -> JobId {
        self.store
            .enqueue(&Task, EnqueueOptions::new())
            .await
            .expect("enqueue")
            .id()
    }

    async fn rename(&self, from: &str, to: &str) {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "ALTER TABLE {from} RENAME TO {to}"
        )))
        .execute(self.db.admin().pool())
        .await
        .expect("rename the job table");
    }

    /// The database stops answering for the job tables.
    async fn sabotage(&self) {
        self.rename("forge_jobs", "forge_jobs_away").await;
    }

    async fn restore(&self) {
        self.rename("forge_jobs_away", "forge_jobs").await;
    }

    async fn state(&self, id: JobId) -> String {
        sqlx::query_scalar("SELECT state FROM forge_jobs WHERE id = $1")
            .bind(id.into_uuid())
            .fetch_one(self.db.admin().pool())
            .await
            .expect("state")
    }

    /// Waits until the job is completed (retrying through lease expiry).
    async fn completed(&self, id: JobId) {
        let deadline = tokio::time::Instant::now() + PATIENCE;
        while self.state(id).await != "completed" {
            assert!(tokio::time::Instant::now() < deadline, "never completed");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn stop(mut self) {
        self.shutdown.cancel();
        if let Some(running) = self.running.take() {
            tokio::time::timeout(PATIENCE, running)
                .await
                .expect("the runtime stops")
                .expect("runtime task")
                .expect("runtime result");
        }
        self.db.cleanup().await.expect("cleanup");
    }
}

#[tokio::test]
#[ignore = "needs FORGE_TEST_DATABASE_URL"]
async fn a_completion_that_could_not_be_recorded_runs_again_after_the_lease_expires() {
    let _serial = SERIAL.lock().await;
    let rig = rig(First::Succeed).await;
    let id = rig.enqueue().await;
    rig.started.notified().await;
    rig.sabotage().await;
    // Renewals fail while the database is down, and the attempt still finishes.
    logged(&["lease renewal failed"]).await;
    rig.go.notify_one();
    logged(&["could not record completion", &id.to_string()]).await;
    rig.restore().await;
    rig.completed(id).await;
    assert_eq!(
        rig.runs.load(Ordering::SeqCst),
        2,
        "at-least-once, not zero or three"
    );
    rig.stop().await;
}

#[tokio::test]
#[ignore = "needs FORGE_TEST_DATABASE_URL"]
async fn a_failure_that_could_not_be_recorded_runs_again_after_the_lease_expires() {
    let _serial = SERIAL.lock().await;
    let rig = rig(First::Fail).await;
    let id = rig.enqueue().await;
    rig.started.notified().await;
    rig.sabotage().await;
    rig.go.notify_one();
    logged(&["could not record the failure", &id.to_string()]).await;
    rig.restore().await;
    rig.completed(id).await;
    assert_eq!(rig.runs.load(Ordering::SeqCst), 2);
    rig.stop().await;
}

#[tokio::test]
#[ignore = "needs FORGE_TEST_DATABASE_URL"]
async fn a_panic_that_could_not_be_recorded_runs_again_after_the_lease_expires() {
    let _serial = SERIAL.lock().await;
    let rig = rig(First::Panic).await;
    let id = rig.enqueue().await;
    rig.started.notified().await;
    rig.sabotage().await;
    rig.go.notify_one();
    logged(&[
        "job panicked and the failure could not be recorded",
        &id.to_string(),
    ])
    .await;
    rig.restore().await;
    rig.completed(id).await;
    assert_eq!(rig.runs.load(Ordering::SeqCst), 2);
    rig.stop().await;
}

#[tokio::test]
#[ignore = "needs FORGE_TEST_DATABASE_URL"]
async fn interrupted_and_aborted_attempts_that_cannot_release_their_lease_say_so() {
    let _serial = SERIAL.lock().await;
    // Interrupted: the handler stops when cancelled, and the release fails.
    let rig_one = rig(First::UntilCancelled).await;
    let id = rig_one.enqueue().await;
    rig_one.started.notified().await;
    rig_one.sabotage().await;
    rig_one.shutdown.cancel();
    logged(&["could not release an interrupted job", &id.to_string()]).await;
    rig_one.restore().await;
    rig_one.stop().await;

    // Aborted: the handler ignores cancellation, so the task is aborted.
    let rig_two = rig(First::Forever).await;
    let id = rig_two.enqueue().await;
    rig_two.started.notified().await;
    rig_two.sabotage().await;
    rig_two.shutdown.cancel();
    logged(&["could not release an aborted job", &id.to_string()]).await;
    rig_two.restore().await;
    rig_two.stop().await;
}

#[tokio::test]
#[ignore = "needs FORGE_TEST_DATABASE_URL"]
async fn an_attempt_that_loses_its_lease_is_cancelled_and_cannot_record_anything() {
    let _serial = SERIAL.lock().await;
    // Another worker took the job: the lease token no longer matches.
    let steal = async |rig: &Rig| {
        sqlx::query("UPDATE forge_jobs SET lease_token = gen_random_uuid()")
            .execute(rig.db.admin().pool())
            .await
            .expect("steal the lease");
    };

    // The handler stops when cancelled and fails: the failure is refused.
    let rig_one = rig(First::UntilCancelled).await;
    let id = rig_one.enqueue().await;
    rig_one.started.notified().await;
    steal(&rig_one).await;
    logged(&["lease lost; cancelling the attempt", &id.to_string()]).await;
    logged(&["job failed after its lease was lost", &id.to_string()]).await;
    rig_one.completed(id).await;
    rig_one.stop().await;

    // The handler finishes anyway: the completion is refused.
    let rig_two = rig(First::HoldThenSucceed).await;
    let id = rig_two.enqueue().await;
    rig_two.started.notified().await;
    steal(&rig_two).await;
    logged(&["lease lost; cancelling the attempt", &id.to_string()]).await;
    rig_two.go.notify_one();
    logged(&["job finished after its lease was lost", &id.to_string()]).await;
    rig_two.completed(id).await;
    rig_two.stop().await;

    // The handler ignores the cancellation: it is aborted after the grace.
    let rig_three = rig(First::Forever).await;
    let id = rig_three.enqueue().await;
    rig_three.started.notified().await;
    steal(&rig_three).await;
    logged(&[
        "attempt ignored cancellation after losing its lease; aborted",
        &id.to_string(),
    ])
    .await;
    rig_three.completed(id).await;
    rig_three.stop().await;
}

#[tokio::test]
#[ignore = "needs FORGE_TEST_DATABASE_URL"]
async fn an_attempt_whose_lease_cannot_be_renewed_in_time_is_cancelled_locally() {
    let _serial = SERIAL.lock().await;
    let rig = rig(First::UntilCancelled).await;
    let id = rig.enqueue().await;
    rig.started.notified().await;
    rig.sabotage().await;
    logged(&[
        "lease could not be renewed in time; cancelling the attempt",
        &id.to_string(),
    ])
    .await;
    // The cancelled attempt fails, and the failure cannot be recorded either.
    logged(&["could not record the failure", &id.to_string()]).await;
    rig.restore().await;
    rig.completed(id).await;
    rig.stop().await;
}

#[tokio::test]
#[ignore = "needs FORGE_TEST_DATABASE_URL"]
async fn claiming_resumes_after_an_outage() {
    let _serial = SERIAL.lock().await;
    let rig = rig(First::Succeed).await;
    rig.sabotage().await;
    logged(&["claiming jobs failed"]).await;
    rig.restore().await;
    let id = rig.enqueue().await;
    rig.started.notified().await;
    rig.go.notify_one();
    rig.completed(id).await;
    assert_eq!(rig.runs.load(Ordering::SeqCst), 1);
    rig.stop().await;
}
