//! The MySQL/MariaDB parts of the lifecycle: the `db` and migration
//! commands, application commands and `serve` over `App::mysql`. They need a
//! server (`FORGE_TEST_MYSQL_URL`) and are `#[ignore]`d like the other
//! database tests.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use super::lifecycle_tests::{
    RunBoxed as _, config, free_port, raise, request, serialized, start, wait_until_serving,
};
use super::*;
use forge_http::{Response, StatusCode};

type TestDatabase = forge_db::testing::TestDatabase<forge_db::sqlx::MySql>;

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Runtime::new().expect("test runtime")
}

fn migrations() -> forge_db::Migrator {
    sqlx::migrate!("tests/migrations/mysql")
}

fn run(app: App, command: &[&str], url: &str) -> ExitCode {
    app.run_boxed(command, || {
        config(&[
            ("FORGE_DATABASE_URL", url),
            ("FORGE_MIGRATION_DATABASE_URL", url),
        ])
    })
}

fn sibling(url: &str, database: &str) -> String {
    let (base, _) = url.split_once('?').unwrap_or((url, ""));
    let (server, _) = base.rsplit_once('/').expect("a database in the URL");
    format!("{server}/{database}")
}

async fn has_table(database: &forge_db::MySqlDatabase, table: &str) -> bool {
    sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM information_schema.tables \
         WHERE table_schema = DATABASE() AND table_name = ?",
    )
    .bind(table)
    .fetch_one(database.pool())
    .await
    .expect("query")
        == 1
}

#[test]
#[ignore = "needs FORGE_TEST_MYSQL_URL"]
fn db_commands_create_check_and_drop_a_mysql_database() {
    let runtime = runtime();
    let test = runtime
        .block_on(TestDatabase::create_empty())
        .expect("needs FORGE_TEST_MYSQL_URL");
    let name = format!("forge_cov_{}", uuid::Uuid::now_v7().simple());
    let url = sibling(test.url().expose(), &name);
    let app = || App::new("demo").mysql(migrations());
    let missing = ExitCode::from(crate::database::EXIT_DATABASE_MISSING);

    assert_eq!(run(app(), &["db", "exists"], &url), missing);
    assert_eq!(run(app(), &["db", "create"], &url), ExitCode::SUCCESS);
    assert_eq!(run(app(), &["db", "create"], &url), ExitCode::SUCCESS);
    assert_eq!(run(app(), &["db", "exists"], &url), ExitCode::SUCCESS);
    assert_eq!(run(app(), &["db", "drop"], &url), ExitCode::SUCCESS);
    assert_eq!(run(app(), &["db", "exists"], &url), missing);
    assert_eq!(run(app(), &["db", "drop"], &url), ExitCode::SUCCESS);
    runtime.block_on(test.cleanup()).expect("cleanup");
}

#[test]
#[ignore = "needs FORGE_TEST_MYSQL_URL"]
fn migrations_commands_and_application_commands_use_the_mysql_pool() {
    let runtime = runtime();
    let test = runtime
        .block_on(TestDatabase::create_empty())
        .expect("needs FORGE_TEST_MYSQL_URL");
    let url = test.url().expose().to_owned();
    let app = || App::new("demo").mysql(migrations());

    assert_eq!(run(app(), &["migrate", "status"], &url), ExitCode::SUCCESS);
    assert!(!runtime.block_on(has_table(test.database(), "notes")));
    assert_eq!(run(app(), &["migrate"], &url), ExitCode::SUCCESS);
    assert!(runtime.block_on(has_table(test.database(), "notes")));
    assert_eq!(run(app(), &["migrate", "status"], &url), ExitCode::SUCCESS);

    let answer = Arc::new(AtomicUsize::new(0));
    let recorded = Arc::clone(&answer);
    let command = App::new("demo").mysql(migrations()).command(
        "answer",
        "Computes the answer",
        move |context| async move {
            let value: i64 = sqlx::query_scalar("SELECT 41 + 1")
                .fetch_one(context.mysql()?.pool())
                .await
                .map_err(|error| CommandError::caused_by("query failed", error))?;
            recorded.store(usize::try_from(value).unwrap_or(0), Ordering::SeqCst);
            // The other backends were not declared.
            assert!(context.sqlite().is_err());
            Ok(())
        },
    );
    assert_eq!(run(command, &["answer"], &url), ExitCode::SUCCESS);
    assert_eq!(answer.load(Ordering::SeqCst), 42);

    assert_eq!(run(app(), &["rollback"], &url), ExitCode::SUCCESS);
    assert!(!runtime.block_on(has_table(test.database(), "notes")));
    runtime.block_on(test.cleanup()).expect("cleanup");
}

#[test]
#[ignore = "needs FORGE_TEST_MYSQL_URL"]
fn serve_over_mysql_answers_readiness_and_stops_on_sigterm() {
    let _serial = serialized();
    let runtime = runtime();
    let test = runtime
        .block_on(TestDatabase::create_empty())
        .expect("needs FORGE_TEST_MYSQL_URL");
    let port = free_port();
    let metrics = free_port();
    let app = App::new("demo")
        .mysql(migrations())
        .routes(|router| {
            router.route(Method::GET, "/marker", |_| async {
                Response::new(StatusCode::OK, "marker")
            })?;
            Ok(())
        })
        .compose(|composition| {
            composition.mysql()?;
            assert!(composition.sqlite().is_err());
            Ok(())
        });
    let running = start(
        app,
        &["serve"],
        vec![
            ("FORGE_BIND", format!("127.0.0.1:{port}")),
            (
                "FORGE_TELEMETRY_METRICS_ADDR",
                format!("127.0.0.1:{metrics}"),
            ),
            ("FORGE_DATABASE_URL", test.url().expose().to_owned()),
            ("FORGE_SHUTDOWN_GRACE_SECS", "2".to_owned()),
        ],
    );

    assert_eq!(wait_until_serving(&running, port, "/marker").status, 200);
    let ready = request(port, "GET", "/health/ready").expect("readiness");
    assert_eq!(
        (ready.status, ready.body.as_str()),
        (200, r#"{"status":"ready"}"#)
    );
    // The pool is observed under its database system's name.
    let scrape = wait_until_serving(&running, metrics, "/metrics");
    assert!(
        scrape.body.contains(r#"db_system_name="mysql""#),
        "{}",
        scrape.body
    );

    raise("TERM");
    assert_eq!(running.join().expect("serve thread"), ExitCode::SUCCESS);
    runtime.block_on(test.cleanup()).expect("cleanup");
}
