//! The PostgreSQL parts of the lifecycle: the `db` commands, application
//! commands with the migration credential and, with `jobs`, the operator
//! commands, `serve` running the job runtime and `jobs work`.
//!
//! The tests that need a server are `#[ignore]`d like the other database
//! tests (`FORGE_TEST_DATABASE_URL`, see `scripts/test-databases.sh`); the
//! ones without a reachable database run everywhere.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use super::lifecycle_tests::{
    args, config, free_port, raise, request, serialized, start, wait_until_serving,
};
use super::*;
use forge_http::{Response, StatusCode};

type TestDatabase = forge_db::testing::TestDatabase<forge_db::sqlx::Postgres>;

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Runtime::new().expect("test runtime")
}

/// The URL of another database on the server `url` names.
fn sibling(url: &str, database: &str) -> String {
    let (base, _) = url.split_once('?').unwrap_or((url, ""));
    let (server, _) = base.rsplit_once('/').expect("a database in the URL");
    format!("{server}/{database}")
}

fn run(app: App, command: &[&str], url: &str) -> ExitCode {
    app.run_with(args(command), || {
        config(&[
            ("FORGE_DATABASE_URL", url),
            ("FORGE_MIGRATION_DATABASE_URL", url),
            ("FORGE_LOG", "error"),
        ])
    })
}

#[test]
#[ignore = "needs FORGE_TEST_DATABASE_URL"]
fn db_commands_create_check_and_drop_a_postgres_database() {
    let runtime = runtime();
    let test = runtime
        .block_on(TestDatabase::create_empty())
        .expect("needs FORGE_TEST_DATABASE_URL");
    let name = format!("forge_cov_{}", uuid::Uuid::now_v7().simple());
    let url = sibling(test.url().expose(), &name);
    let app = || App::new("demo").postgres(forge_db::Migrator::DEFAULT);
    let missing = ExitCode::from(crate::database::EXIT_DATABASE_MISSING);

    assert_eq!(run(app(), &["db", "exists"], &url), missing);
    assert_eq!(run(app(), &["db", "create"], &url), ExitCode::SUCCESS);
    assert_eq!(
        run(app(), &["db", "create"], &url),
        ExitCode::SUCCESS,
        "creating twice is not an error"
    );
    assert_eq!(run(app(), &["db", "exists"], &url), ExitCode::SUCCESS);
    assert_eq!(run(app(), &["db", "drop"], &url), ExitCode::SUCCESS);
    assert_eq!(run(app(), &["db", "exists"], &url), missing);
    assert_eq!(
        run(app(), &["db", "drop"], &url),
        ExitCode::SUCCESS,
        "dropping a missing database is not an error"
    );
    runtime.block_on(test.cleanup()).expect("cleanup");
}

#[test]
fn db_commands_fail_cleanly_when_the_server_is_unreachable() {
    let url = "postgres://postgres:postgres@127.0.0.1:1/app";
    let app = || App::new("demo").postgres(forge_db::Migrator::DEFAULT);
    for command in [
        &["db", "create"][..],
        &["db", "exists"],
        &["db", "drop"],
        &["migrate"],
        &["migrate", "status"],
        &["rollback"],
    ] {
        assert_eq!(run(app(), command, url), ExitCode::FAILURE, "{command:?}");
    }
    // Production drops need the explicit flag, which this does not give.
    let production = app().run_with(args(&["db", "drop"]), || {
        config(&[
            ("FORGE_ENV", "production"),
            ("FORGE_MIGRATION_DATABASE_URL", url),
            ("FORGE_LOG", "error"),
        ])
    });
    assert_eq!(production, ExitCode::FAILURE);
}

#[test]
#[ignore = "needs FORGE_TEST_DATABASE_URL"]
fn an_application_command_uses_the_postgres_pool() {
    let runtime = runtime();
    let test = runtime
        .block_on(TestDatabase::create_empty())
        .expect("needs FORGE_TEST_DATABASE_URL");
    let answer = Arc::new(AtomicUsize::new(0));
    let recorded = Arc::clone(&answer);
    let app = App::new("demo")
        .postgres(forge_db::Migrator::DEFAULT)
        .command("answer", "Computes the answer", move |context| async move {
            let value: i32 = sqlx::query_scalar("SELECT 41 + 1")
                .fetch_one(context.postgres()?.pool())
                .await
                .map_err(|error| CommandError::caused_by("query failed", error))?;
            recorded.store(usize::try_from(value).unwrap_or(0), Ordering::SeqCst);
            Ok(())
        });

    assert_eq!(
        run(app, &["answer"], test.url().expose()),
        ExitCode::SUCCESS
    );
    assert_eq!(answer.load(Ordering::SeqCst), 42);
    runtime.block_on(test.cleanup()).expect("cleanup");
}

#[cfg(feature = "jobs")]
mod jobs {
    use async_trait::async_trait;
    use forge_jobs::{
        EnqueueOptions, Job, JobContext, JobError, JobHandler, JobId, postgres::PgJobStore,
    };
    use serde::{Deserialize, Serialize};

    use super::*;
    use crate::{background::WorkerMode, cli::JobsCommand};

    fn background_app(mode: WorkerMode) -> App {
        let app = App::new("demo")
            .postgres(forge_db::Migrator::DEFAULT)
            .jobs(mode);
        #[cfg(feature = "events")]
        let app = app.events();
        app
    }

    #[derive(Serialize, Deserialize)]
    struct Greet;

    impl Job for Greet {
        const NAME: &'static str = "cov.greet";
    }

    struct Greeter(Arc<AtomicUsize>);

    #[async_trait]
    impl JobHandler for Greeter {
        type Job = Greet;

        async fn perform(&self, _: &JobContext, _: Greet) -> Result<(), JobError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[cfg(feature = "events")]
    #[derive(Serialize, Deserialize)]
    struct Greeted;

    #[cfg(feature = "events")]
    impl forge_events::IntegrationEvent for Greeted {
        const TYPE: &'static str = "cov.greeted";
    }

    #[cfg(feature = "events")]
    struct Listener(Arc<AtomicUsize>);

    #[cfg(feature = "events")]
    #[async_trait]
    impl forge_events::EventSubscriber for Listener {
        async fn handle(
            &self,
            _: &forge_events::EventEnvelope,
        ) -> Result<(), forge_events::PublishError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    /// The composition hook of a job application: one handler and, with
    /// `events`, one subscriber, counting what they receive.
    fn counting_hook(
        greeted: Arc<AtomicUsize>,
        heard: Arc<AtomicUsize>,
    ) -> impl FnOnce(&mut Composition<'_>) -> Result<(), ComposeError> + Send + 'static {
        move |composition| {
            composition
                .jobs()?
                .register(Greeter(greeted))
                .map_err(ComposeError::other)?;
            #[cfg(feature = "events")]
            {
                let mut subscribers = forge_events::Subscribers::new();
                subscribers
                    .subscribe::<Greeted>(Listener(heard))
                    .map_err(ComposeError::other)?;
                composition.events()?.publish_to(subscribers);
            }
            #[cfg(not(feature = "events"))]
            drop(heard);
            Ok(())
        }
    }

    /// Migrates the framework and application schemas of `url`.
    fn migrate(url: &str) {
        assert_eq!(
            run(background_app(WorkerMode::Dedicated), &["migrate"], url),
            ExitCode::SUCCESS
        );
    }

    /// Enqueues a job and, with `events`, appends an event, both for the
    /// runtime to find.
    fn produce(runtime: &tokio::runtime::Runtime, test: &TestDatabase) {
        runtime.block_on(async {
            PgJobStore::new(test.database().clone())
                .enqueue(&Greet, EnqueueOptions::new())
                .await
                .expect("enqueue");
            #[cfg(feature = "events")]
            {
                let mut transaction = test.database().begin().await.expect("begin");
                forge_events::postgres::append(&mut transaction, &Greeted)
                    .await
                    .expect("append");
                transaction.commit().await.expect("commit");
            }
        });
    }

    /// Waits until the handler and the subscriber ran.
    fn wait_for_work(
        running: &std::thread::JoinHandle<ExitCode>,
        greeted: &AtomicUsize,
        heard: &AtomicUsize,
    ) {
        let expected_events = usize::from(cfg!(feature = "events"));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while greeted.load(Ordering::SeqCst) == 0 || heard.load(Ordering::SeqCst) < expected_events
        {
            assert!(!running.is_finished(), "the process exited early");
            assert!(
                std::time::Instant::now() < deadline,
                "the runtime did not run the work"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    #[test]
    #[ignore = "needs FORGE_TEST_DATABASE_URL"]
    fn serve_runs_the_job_runtime_next_to_the_server_until_sigterm() {
        let _serial = serialized();
        let runtime = runtime();
        let test = runtime
            .block_on(TestDatabase::create_empty())
            .expect("needs FORGE_TEST_DATABASE_URL");
        let url = test.url().expose().to_owned();
        migrate(&url);
        let (greeted, heard) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let port = free_port();
        let app = background_app(WorkerMode::InServe)
            .routes(|router| {
                router.route(Method::GET, "/marker", |_| async {
                    Response::new(StatusCode::OK, "marker")
                })?;
                Ok(())
            })
            .compose(counting_hook(Arc::clone(&greeted), Arc::clone(&heard)));
        let running = start(
            app,
            &["serve"],
            vec![
                ("FORGE_BIND", format!("127.0.0.1:{port}")),
                ("FORGE_DATABASE_URL", url),
                ("FORGE_SHUTDOWN_GRACE_SECS", "2".to_owned()),
                ("FORGE_LOG", "error".to_owned()),
            ],
        );
        assert_eq!(wait_until_serving(&running, port, "/marker").status, 200);
        let ready = request(port, "GET", "/health/ready").expect("readiness");
        assert_eq!(ready.status, 200, "the database answers");

        produce(&runtime, &test);
        wait_for_work(&running, &greeted, &heard);

        raise("TERM");
        assert_eq!(running.join().expect("serve thread"), ExitCode::SUCCESS);
        runtime.block_on(test.cleanup()).expect("cleanup");
    }

    #[test]
    #[ignore = "needs FORGE_TEST_DATABASE_URL"]
    fn a_dedicated_worker_process_runs_the_jobs_and_its_metrics_until_sigterm() {
        let _serial = serialized();
        let runtime = runtime();
        let test = runtime
            .block_on(TestDatabase::create_empty())
            .expect("needs FORGE_TEST_DATABASE_URL");
        let url = test.url().expose().to_owned();
        migrate(&url);
        let (greeted, heard) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let metrics = free_port();
        let app = background_app(WorkerMode::Dedicated)
            .compose(counting_hook(Arc::clone(&greeted), Arc::clone(&heard)));
        produce(&runtime, &test);
        let running = start(
            app,
            &["jobs", "work"],
            vec![
                ("FORGE_DATABASE_URL", url),
                (
                    "FORGE_TELEMETRY_METRICS_ADDR",
                    format!("127.0.0.1:{metrics}"),
                ),
                ("FORGE_SHUTDOWN_GRACE_SECS", "2".to_owned()),
                ("FORGE_LOG", "error".to_owned()),
            ],
        );

        // `jobs work` serves no HTTP, only the metrics listener.
        let scrape = wait_until_serving(&running, metrics, "/metrics");
        assert_eq!(scrape.status, 200);
        wait_for_work(&running, &greeted, &heard);
        assert!(request(free_port(), "GET", "/marker").is_err());

        raise("TERM");
        assert_eq!(running.join().expect("work thread"), ExitCode::SUCCESS);
        assert!(
            request(metrics, "GET", "/metrics").is_err(),
            "the listener is released"
        );
        runtime.block_on(test.cleanup()).expect("cleanup");
    }

    #[test]
    #[ignore = "needs FORGE_TEST_DATABASE_URL"]
    fn a_dedicated_mode_serve_does_not_run_the_workers() {
        let _serial = serialized();
        let runtime = runtime();
        let test = runtime
            .block_on(TestDatabase::create_empty())
            .expect("needs FORGE_TEST_DATABASE_URL");
        let url = test.url().expose().to_owned();
        migrate(&url);
        let (greeted, heard) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let port = free_port();
        let app = background_app(WorkerMode::Dedicated)
            .routes(|router| {
                router.route(Method::GET, "/marker", |_| async {
                    Response::new(StatusCode::OK, "marker")
                })?;
                Ok(())
            })
            .compose(counting_hook(Arc::clone(&greeted), heard));
        produce(&runtime, &test);
        let running = start(
            app,
            &["serve"],
            vec![
                ("FORGE_BIND", format!("127.0.0.1:{port}")),
                ("FORGE_DATABASE_URL", url),
                ("FORGE_LOG", "error".to_owned()),
            ],
        );
        assert_eq!(wait_until_serving(&running, port, "/marker").status, 200);
        // The job stays queued: `serve` only enqueues in this mode.
        let queued: i64 = runtime
            .block_on(
                sqlx::query_scalar("SELECT count(*) FROM forge_jobs WHERE state = 'queued'")
                    .fetch_one(test.database().pool()),
            )
            .expect("count");
        assert_eq!(queued, 1);
        assert_eq!(greeted.load(Ordering::SeqCst), 0);

        raise("TERM");
        assert_eq!(running.join().expect("serve thread"), ExitCode::SUCCESS);
        runtime.block_on(test.cleanup()).expect("cleanup");
    }

    #[test]
    fn the_jobs_commands_need_a_declaration_and_a_reachable_database() {
        // Not declared.
        let undeclared = App::new("demo").postgres(forge_db::Migrator::DEFAULT);
        assert_eq!(
            run(
                undeclared,
                &["jobs", "status"],
                "postgres://u:p@127.0.0.1:1/app"
            ),
            ExitCode::FAILURE
        );
        // A URL for another engine is a configuration error.
        assert_eq!(
            run(
                background_app(WorkerMode::InServe),
                &["jobs", "status"],
                "mysql://u:p@h/app"
            ),
            ExitCode::from(EXIT_CONFIG)
        );
        // Unreachable: the operator commands and the worker fail, not hang.
        for command in [&["jobs", "status"][..], &["jobs", "dead"]] {
            assert_eq!(
                run(
                    background_app(WorkerMode::InServe),
                    command,
                    "postgres://postgres:postgres@127.0.0.1:1/app"
                ),
                ExitCode::FAILURE,
                "{command:?}"
            );
        }
    }

    #[test]
    #[ignore = "needs FORGE_TEST_DATABASE_URL"]
    fn operator_commands_list_requeue_and_discard_dead_jobs_and_events() {
        let runtime = runtime();
        let test = runtime
            .block_on(TestDatabase::create_empty())
            .expect("needs FORGE_TEST_DATABASE_URL");
        migrate(test.url().expose());
        let database = test.database();
        let operate =
            |command: JobsCommand| runtime.block_on(crate::operator::run(command, database));
        let text = |command: JobsCommand| operate(command).expect("the command works");

        // Nothing yet.
        assert_eq!(text(JobsCommand::Work), "", "`work` is the caller's");
        assert_eq!(text(JobsCommand::Status), "no jobs\n");
        assert_eq!(text(JobsCommand::Dead { limit: 10 }), "no dead jobs\n");
        #[cfg(feature = "events")]
        assert_eq!(
            text(JobsCommand::DeadEvents { limit: 10 }),
            "no dead events\n"
        );

        let (dead, tenant_dead, queued) = (JobId::new(), JobId::new(), JobId::new());
        let tenant = uuid::Uuid::now_v7();
        runtime.block_on(async {
            for (id, state, tenant, error) in [
                (dead, "dead", None, Some("boom")),
                (tenant_dead, "dead", Some(tenant), Some("tenant boom")),
                (queued, "queued", None, None),
            ] {
                sqlx::query(
                    "INSERT INTO forge_jobs
                        (id, tenant_id, queue, job_name, payload, max_attempts, attempts,
                         run_at, state, finished_at, last_error)
                     VALUES ($1, $2, 'default', 'cov.job', '{}', 3,
                             CASE WHEN $3 = 'dead' THEN 3 ELSE 0 END,
                             now() - interval '1 minute', $3,
                             CASE WHEN $3 = 'dead' THEN now() END, $4)",
                )
                .bind(id.into_uuid())
                .bind(tenant)
                .bind(state)
                .bind(error)
                .execute(database.pool())
                .await
                .expect("insert");
            }
        });

        let status = text(JobsCommand::Status);
        assert!(status.starts_with("QUEUE"), "{status}");
        assert!(status.contains("OLDEST_DUE"), "{status}");
        assert!(
            status
                .lines()
                .any(|line| line.contains("queued") && !line.ends_with('-')),
            "{status}"
        );
        assert!(
            status
                .lines()
                .any(|line| line.contains("dead") && line.ends_with('-')),
            "{status}"
        );

        let listing = text(JobsCommand::Dead { limit: 10 });
        assert_eq!(listing.lines().count(), 2, "{listing}");
        assert!(
            listing.contains(&dead.to_string()) && listing.contains("error=boom"),
            "{listing}"
        );
        assert!(
            listing.contains("attempts=3/3") && listing.contains("cov.job"),
            "{listing}"
        );
        assert!(
            listing.contains(&format!("tenant={tenant}")) && listing.contains("tenant=-"),
            "{listing}"
        );
        assert_eq!(text(JobsCommand::Dead { limit: 1 }).lines().count(), 1);
        assert!(!listing.contains("{}"), "payloads are never printed");

        assert_eq!(
            text(JobsCommand::Retry(dead)),
            format!("requeued job {dead}\n")
        );
        assert_eq!(
            text(JobsCommand::Discard(tenant_dead)),
            format!("discarded job {tenant_dead}\n")
        );
        for command in [
            JobsCommand::Retry(dead),
            JobsCommand::Discard(tenant_dead),
            JobsCommand::Retry(queued),
        ] {
            let error = operate(command).expect_err("no such dead job");
            assert!(error.to_string().starts_with("no dead job"), "{error}");
        }
        assert_eq!(text(JobsCommand::Dead { limit: 10 }), "no dead jobs\n");

        #[cfg(feature = "events")]
        {
            let event = forge_events::EventId::new();
            runtime
                .block_on(
                    sqlx::query(
                        "INSERT INTO forge_outbox_events
                        (id, event_type, version, payload, occurred_at, state, attempts, last_error)
                     VALUES ($1, 'cov.failed', 2, '{}', now(), 'dead', 5, 'delivery failed')",
                    )
                    .bind(event.into_uuid())
                    .execute(database.pool()),
                )
                .expect("insert event");
            let listing = text(JobsCommand::DeadEvents { limit: 10 });
            assert_eq!(
                listing,
                format!("{event}  cov.failed  v2  attempts=5  error=delivery failed\n")
            );
            assert_eq!(
                text(JobsCommand::RetryEvent(event)),
                format!("requeued event {event}\n")
            );
            let error = operate(JobsCommand::RetryEvent(event)).expect_err("no longer dead");
            assert!(error.to_string().starts_with("no dead event"), "{error}");
            assert_eq!(
                text(JobsCommand::DeadEvents { limit: 10 }),
                "no dead events\n"
            );
        }
        runtime.block_on(test.cleanup()).expect("cleanup");
    }

    #[test]
    #[ignore = "needs FORGE_TEST_DATABASE_URL"]
    fn the_operator_commands_run_from_the_command_line_with_the_runtime_url() {
        let runtime = runtime();
        let test = runtime
            .block_on(TestDatabase::create_empty())
            .expect("needs FORGE_TEST_DATABASE_URL");
        let url = test.url().expose().to_owned();
        migrate(&url);
        let dead = JobId::new();
        runtime
            .block_on(
                sqlx::query(
                    "INSERT INTO forge_jobs (id, queue, job_name, payload, max_attempts, attempts,
                                             run_at, state, finished_at, last_error)
                     VALUES ($1, 'default', 'cov.job', '{}', 1, 1, now(), 'dead', now(), 'boom')",
                )
                .bind(dead.into_uuid())
                .execute(test.database().pool()),
            )
            .expect("insert");

        let id = dead.to_string();
        for command in [
            vec!["jobs", "status"],
            vec!["jobs", "dead", "5"],
            vec!["jobs", "retry", id.as_str()],
            vec!["jobs", "discard", id.as_str()],
        ] {
            let expected = if command[1] == "discard" {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            };
            assert_eq!(
                run(background_app(WorkerMode::Dedicated), &command, &url),
                expected,
                "{command:?}"
            );
        }
        runtime.block_on(test.cleanup()).expect("cleanup");
    }

    /// Without a publisher the outbox accumulates and no dispatcher runs; a
    /// job runtime with nothing registered is a normal starting point.
    #[test]
    fn a_runtime_is_built_without_handlers_publishers_or_telemetry_failures() {
        let runtime = runtime();
        let _enter = runtime.enter();
        let url = forge_config::SecretString::new("postgres://app:secret@127.0.0.1:1/app");
        let database = forge_db::PgDatabase::connect_lazy(&url).expect("lazy pool");
        let telemetry = forge_telemetry::Telemetry::new(forge_telemetry::Options::new("demo"))
            .expect("telemetry");
        // Instruments of those names already registered as something else:
        // the runtime still starts, only without its metrics.
        let conflicting = forge_telemetry::Telemetry::new(forge_telemetry::Options::new("demo"))
            .expect("telemetry");
        for name in ["forge.queue.job.duration", "forge.outbox.dispatch.duration"] {
            conflicting
                .metrics()
                .counter(name, "taken", "1", &[])
                .expect("registers first");
        }
        let mut declaration = crate::background::JobsDeclaration::new(WorkerMode::InServe);
        #[cfg(feature = "events")]
        {
            declaration.events = true;
        }
        let _ = &mut declaration;

        let log = crate::test_support::warnings(|| {
            for telemetry in [None, Some(&telemetry), Some(&conflicting)] {
                #[cfg_attr(not(feature = "events"), allow(unused_mut))]
                let mut parts = crate::background::JobsParts::new(declaration);
                // Only the first runtime goes without a publisher.
                #[cfg(feature = "events")]
                if let (Some(events), Some(_)) = (parts.events.as_mut(), telemetry) {
                    events.publish_to(forge_events::Subscribers::new());
                }
                let background = parts.into_background(database.clone(), telemetry);
                assert!(format!("{background:?}").contains("Background"));
            }
        });
        assert!(log.contains("job telemetry is not recorded"), "{log}");
        #[cfg(feature = "events")]
        {
            assert!(log.contains("no composition hook set a publisher"), "{log}");
            assert!(log.contains("outbox telemetry is not recorded"), "{log}");
        }
    }
}
