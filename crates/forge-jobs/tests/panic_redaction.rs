//! The redacting panic hook (own test binary: panic hooks are
//! process-global). The DB-gated test needs `FORGE_TEST_DATABASE_URL` and
//! `--include-ignored`.

use std::{
    sync::{
        Once,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use forge_core::Cancellation;
use forge_jobs::{
    EnqueueOptions, Job, JobContext, JobError, JobHandler, Jobs, WorkerSettings, panics,
    postgres::{PgJobStore, migrator},
    testing::RlsTestDatabase,
};
use serde::{Deserialize, Serialize};

/// Panics that reached the hook installed before Forge's (the "default").
static PREVIOUS_HOOK_CALLS: AtomicUsize = AtomicUsize::new(0);
static INSTALL: Once = Once::new();
/// Serializes the tests: they count panics on one process-wide hook.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn install() {
    INSTALL.call_once(|| {
        std::panic::set_hook(Box::new(|_| {
            PREVIOUS_HOOK_CALLS.fetch_add(1, Ordering::SeqCst);
        }));
        panics::redact_attempt_panics();
        panics::redact_attempt_panics(); // idempotent
    });
}

#[tokio::test]
async fn only_panics_inside_redacted_scopes_bypass_the_previous_hook() {
    let _serial = SERIAL.lock().await;
    install();
    let before = PREVIOUS_HOOK_CALLS.load(Ordering::SeqCst);
    let redacted = tokio::spawn(panics::redacted(async {
        panic!("secret payload field");
    }))
    .await;
    assert!(redacted.expect_err("panicked").is_panic());
    assert_eq!(
        PREVIOUS_HOOK_CALLS.load(Ordering::SeqCst),
        before,
        "withheld"
    );

    let ordinary = tokio::spawn(async {
        panic!("ordinary panic");
    })
    .await;
    assert!(ordinary.expect_err("panicked").is_panic());
    assert_eq!(
        PREVIOUS_HOOK_CALLS.load(Ordering::SeqCst),
        before + 1,
        "other panics still reach the previous hook"
    );
}

#[derive(Serialize, Deserialize)]
struct Explode {
    secret: String,
}

impl Job for Explode {
    const NAME: &'static str = "test.explode";
    const MAX_ATTEMPTS: u16 = 1;
}

struct Panics;

#[async_trait]
impl JobHandler for Panics {
    type Job = Explode;

    async fn perform(&self, _: &JobContext, job: Explode) -> Result<(), JobError> {
        panic!("handler bug quoting {}", job.secret);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs FORGE_TEST_DATABASE_URL"]
async fn job_handler_panics_are_withheld_from_the_previous_hook() {
    let _serial = SERIAL.lock().await;
    install();
    let db = RlsTestDatabase::create(&[migrator()])
        .await
        .expect("needs FORGE_TEST_DATABASE_URL");
    let store = PgJobStore::new(db.runtime().clone());
    let id = store
        .enqueue(
            &Explode {
                secret: "4111-1111-1111-1111".to_owned(),
            },
            EnqueueOptions::new(),
        )
        .await
        .expect("enqueue")
        .id();
    let before = PREVIOUS_HOOK_CALLS.load(Ordering::SeqCst);
    let mut jobs = Jobs::new();
    jobs.register(Panics).expect("register");
    jobs.settings(WorkerSettings::default().poll_interval(Duration::from_millis(50)));
    let shutdown = Cancellation::new();
    let running = tokio::spawn(jobs.run(store.clone(), shutdown.clone()));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let state: String = sqlx::query_scalar("SELECT state FROM forge_jobs WHERE id = $1")
            .bind(id.into_uuid())
            .fetch_one(db.admin().pool())
            .await
            .expect("state");
        if state == "dead" {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "the job never died");
        // real-clock: polls state written by a background task through a real database, until a deadline
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    shutdown.cancel();
    running.await.expect("join").expect("runtime");
    assert_eq!(
        PREVIOUS_HOOK_CALLS.load(Ordering::SeqCst),
        before,
        "the handler's panic message never reached the default hook"
    );
    db.cleanup().await.expect("cleanup");
}
