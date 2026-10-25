//! `serve`, the application commands and the migration commands run
//! in-process, from the command line down to the signal that ends them.
//!
//! A shutdown signal is process-wide, so every test that installs the
//! handlers holds [`serialized`]. A signal is sent only after the server
//! answered a request: `serve` installs its handlers before it binds, so a
//! served request proves the signal is handled and cannot end the test
//! process.

use std::{
    io::Read as _,
    net::{TcpListener as StdListener, TcpStream},
    process::Command as Process,
    sync::{
        Arc, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant as StdInstant},
};

use forge_http::{Response, StatusCode};

use super::*;

static SIGNALS: Mutex<()> = Mutex::new(());

pub(super) fn serialized() -> MutexGuard<'static, ()> {
    SIGNALS.lock().unwrap_or_else(PoisonError::into_inner)
}

pub(super) fn args(values: &[&str]) -> Vec<OsString> {
    values.iter().map(OsString::from).collect()
}

pub(super) fn config(vars: &[(&str, &str)]) -> Result<AppConfig, ConfigError> {
    AppConfig::from_vars(
        vars.iter()
            .map(|(key, value)| (OsString::from(key), OsString::from(value))),
    )
}

pub(super) fn free_port() -> u16 {
    StdListener::bind("127.0.0.1:0")
        .expect("a free port")
        .local_addr()
        .expect("its address")
        .port()
}

/// What a raw HTTP/1.1 exchange returned.
pub(super) struct Reply {
    pub(super) status: u16,
    pub(super) headers: String,
    pub(super) body: String,
}

pub(super) fn request(port: u16, method: &str, path: &str) -> std::io::Result<Reply> {
    let address = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(2))?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    )?;
    let mut raw = String::new();
    stream.read_to_string(&mut raw)?;
    let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((&raw, ""));
    let status = head
        .get(9..12)
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| std::io::Error::other(format!("no status line in {head:?}")))?;
    Ok(Reply {
        status,
        headers: head.to_ascii_lowercase(),
        body: body.to_owned(),
    })
}

/// Requests `path` until the server answers, or fails when the process
/// exits first.
pub(super) fn wait_until_serving(running: &JoinHandle<ExitCode>, port: u16, path: &str) -> Reply {
    let deadline = StdInstant::now() + Duration::from_secs(60);
    loop {
        assert!(!running.is_finished(), "the process exited before serving");
        match request(port, "GET", path) {
            Ok(reply) => return reply,
            Err(_) if StdInstant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Err(error) => panic!("nothing answered on port {port}: {error}"),
        }
    }
}

pub(super) fn raise(signal: &str) {
    let status = Process::new("kill")
        .arg(format!("-{signal}"))
        .arg(std::process::id().to_string())
        .status()
        .expect("kill is installed");
    assert!(status.success(), "kill -{signal}");
}

pub(super) fn start(
    app: App,
    command: &'static [&'static str],
    vars: Vec<(&'static str, String)>,
) -> JoinHandle<ExitCode> {
    thread::spawn(move || {
        app.run_with(args(command), move || {
            let vars: Vec<(&str, &str)> = vars
                .iter()
                .map(|(key, value)| (*key, value.as_str()))
                .collect();
            config(&vars)
        })
    })
}

pub(super) fn marker_app() -> App {
    App::new("demo").version("9.9.9").routes(|router| {
        router.route(Method::GET, "/marker", |_| async {
            Response::new(StatusCode::OK, "marker")
        })?;
        Ok(())
    })
}

#[cfg(feature = "sqlite")]
fn sqlite_url(directory: &tempfile::TempDir) -> String {
    format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("app.db").display()
    )
}

#[cfg(feature = "sqlite")]
#[test]
fn serve_runs_until_sigterm_and_stops_what_it_started() {
    let _serial = serialized();
    let directory = tempfile::tempdir().expect("tempdir");
    let (port, metrics) = (free_port(), free_port());
    let stopped = Arc::new(AtomicBool::new(false));
    let seen = Arc::clone(&stopped);
    let app = marker_app()
        .sqlite(forge_db::Migrator::DEFAULT)
        .compose(move |composition| {
            composition.worker("watcher", move |mut shutdown| async move {
                shutdown.requested().await;
                seen.store(true, Ordering::SeqCst);
            });
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
            ("FORGE_DATABASE_URL", sqlite_url(&directory)),
            ("FORGE_SHUTDOWN_GRACE_SECS", "2".to_owned()),
            ("FORGE_LOG", "error".to_owned()),
        ],
    );

    let marker = wait_until_serving(&running, port, "/marker");
    assert_eq!((marker.status, marker.body.as_str()), (200, "marker"));
    let ready = request(port, "GET", "/health/ready").expect("readiness");
    assert_eq!(
        (ready.status, ready.body.as_str()),
        (200, r#"{"status":"ready"}"#)
    );
    // The document is built from the finished route table and carries the
    // version the application declared.
    let document = request(port, "GET", "/openapi.json").expect("document");
    assert_eq!(document.status, 200);
    assert!(document.body.contains("9.9.9"), "{}", document.body);

    // The metrics listener serves `GET` and `HEAD /metrics`, and nothing else.
    let scrape = wait_until_serving(&running, metrics, "/metrics");
    assert_eq!(scrape.status, 200);
    assert!(
        scrape
            .headers
            .contains("content-type: text/plain; version=0.0.4"),
        "{}",
        scrape.headers
    );
    assert!(scrape.headers.contains("cache-control: no-store"));
    assert!(scrape.body.contains("# TYPE"), "{}", scrape.body);
    let head = request(metrics, "HEAD", "/metrics").expect("HEAD");
    assert_eq!((head.status, head.body.as_str()), (200, ""));
    assert_eq!(
        request(metrics, "GET", "/marker")
            .expect("other path")
            .status,
        404
    );
    assert_eq!(
        request(port, "GET", "/metrics")
            .expect("not on the app")
            .status,
        404
    );

    raise("TERM");
    assert_eq!(running.join().expect("serve thread"), ExitCode::SUCCESS);
    assert!(stopped.load(Ordering::SeqCst), "workers were told to stop");
    assert!(
        request(port, "GET", "/marker").is_err(),
        "the port is released"
    );
    assert!(request(metrics, "GET", "/metrics").is_err());
}

#[test]
fn serve_also_stops_on_sigint() {
    let _serial = serialized();
    let port = free_port();
    let running = start(
        marker_app(),
        &["serve"],
        vec![
            ("FORGE_BIND", format!("127.0.0.1:{port}")),
            ("FORGE_LOG", "error".to_owned()),
        ],
    );
    assert_eq!(wait_until_serving(&running, port, "/marker").status, 200);

    raise("INT");
    assert_eq!(running.join().expect("serve thread"), ExitCode::SUCCESS);
}

#[test]
fn serve_fails_when_an_address_is_taken() {
    let _serial = serialized();
    let taken = StdListener::bind("127.0.0.1:0").expect("a listener");
    let taken_port = taken.local_addr().expect("address").port();
    let quiet = ("FORGE_LOG", "error".to_owned());

    let main = start(
        marker_app(),
        &["serve"],
        vec![
            ("FORGE_BIND", format!("127.0.0.1:{taken_port}")),
            quiet.clone(),
        ],
    );
    assert_eq!(main.join().expect("serve thread"), ExitCode::FAILURE);

    let metrics = start(
        marker_app(),
        &["serve"],
        vec![
            ("FORGE_BIND", format!("127.0.0.1:{}", free_port())),
            (
                "FORGE_TELEMETRY_METRICS_ADDR",
                format!("127.0.0.1:{taken_port}"),
            ),
            quiet,
        ],
    );
    assert_eq!(metrics.join().expect("serve thread"), ExitCode::FAILURE);
}

#[test]
fn composition_failures_stop_startup_before_listening() {
    let duplicate = App::new("demo").routes(|router| {
        router.route(Method::GET, "/health/live", |_| async {
            Response::empty(StatusCode::OK)
        })?;
        Ok(())
    });
    assert_eq!(
        duplicate.run_with(args(&["serve"]), || config(&[])),
        ExitCode::FAILURE,
        "a duplicate route"
    );
    let failing = App::new("demo").compose(|_| Err(ComposeError::other(io::Error::other("no"))));
    assert_eq!(
        failing.run_with(args(&["serve"]), || config(&[])),
        ExitCode::FAILURE,
        "a composition hook"
    );
    let failing = App::new("demo").compose(|_| Err(ComposeError::other(io::Error::other("no"))));
    assert_eq!(
        failing.run_with(args(&["routes"]), || config(&[])),
        ExitCode::FAILURE,
        "inspection composes the application too"
    );
}

#[test]
fn an_invalid_log_filter_is_a_configuration_error_for_every_command() {
    let vars = [("FORGE_LOG", "demo=loudest")];
    let app = || {
        App::new("demo")
            .command("seed", "x", |_| async { Ok(()) })
            .routes(|router| {
                router.route(Method::GET, "/x", |_| async {
                    Response::empty(StatusCode::OK)
                })?;
                Ok(())
            })
    };
    for command in [
        &["serve"][..],
        &["migrate"],
        &["routes"],
        &["openapi"],
        &["seed"],
    ] {
        assert_eq!(
            app().run_with(args(command), || config(&vars)),
            ExitCode::from(EXIT_CONFIG),
            "{command:?}"
        );
    }
}

#[cfg(feature = "sqlite")]
mod sqlite {
    use std::sync::Mutex;

    use super::*;

    fn migrations() -> forge_db::Migrator {
        sqlx::migrate!("tests/migrations/sqlite")
    }

    fn run(app: App, command: &[&str], url: &str) -> ExitCode {
        app.run_with(args(command), || {
            config(&[
                ("FORGE_MIGRATION_DATABASE_URL", url),
                ("FORGE_LOG", "error"),
            ])
        })
    }

    #[test]
    fn a_command_uses_the_database_with_the_migration_credential() {
        let directory = tempfile::tempdir().expect("tempdir");
        let url = sqlite_url(&directory);
        let answer = Arc::new(Mutex::new(None));
        let recorded = Arc::clone(&answer);
        let app = App::new("demo").sqlite(migrations()).command(
            "answer",
            "Prints the answer",
            move |mut context| async move {
                assert_eq!(context.app_name(), "demo");
                assert!(context.config().database.migration_url.is_some());
                assert!(format!("{context:?}").contains("answer"));
                context.say("the answer follows")?;
                let value: i64 = sqlx::query_scalar("SELECT 41 + 1")
                    .fetch_one(context.sqlite()?.pool())
                    .await
                    .map_err(|error| CommandError::caused_by("query failed", error))?;
                *recorded.lock().expect("lock") = Some(value);
                Ok(())
            },
        );

        assert_eq!(run(app, &["answer"], &url), ExitCode::SUCCESS);
        assert_eq!(*answer.lock().expect("lock"), Some(42));

        let wrong = App::new("demo").sqlite(migrations()).command(
            "wrong",
            "Asks for PostgreSQL",
            |context| async move {
                let error = context.postgres().map(|_| ()).expect_err("undeclared");
                assert!(error.to_string().contains("App::postgres"), "{error}");
                Err(error)
            },
        );
        assert_eq!(run(wrong, &["wrong"], &url), ExitCode::FAILURE);
    }

    #[test]
    fn an_unreachable_database_fails_every_command_that_connects() {
        let directory = tempfile::tempdir().expect("tempdir");
        // SQLite does not create missing directories.
        let url = format!(
            "sqlite://{}",
            directory.path().join("missing").join("app.db").display()
        );
        let app = || {
            App::new("demo")
                .sqlite(migrations())
                .command("seed", "x", |_| async { Ok(()) })
        };
        for command in [
            &["seed"][..],
            &["migrate"],
            &["migrate", "status"],
            &["rollback"],
            &["db", "create"],
        ] {
            assert_eq!(run(app(), command, &url), ExitCode::FAILURE, "{command:?}");
        }
    }

    #[test]
    fn a_failing_migration_fails_the_command_and_status_still_reads() {
        let directory = tempfile::tempdir().expect("tempdir");
        let url = sqlite_url(&directory);
        let app = || App::new("demo").sqlite(sqlx::migrate!("tests/migrations/sqlite_broken"));

        assert_eq!(run(app(), &["migrate"], &url), ExitCode::FAILURE);
        // The failed migration was rolled back with its transaction.
        assert_eq!(run(app(), &["migrate", "status"], &url), ExitCode::SUCCESS);
        assert_eq!(run(app(), &["rollback"], &url), ExitCode::SUCCESS);
    }

    #[test]
    fn a_missing_runtime_url_is_a_configuration_error_for_serve_and_commands() {
        let app = || {
            App::new("demo")
                .sqlite(migrations())
                .command("seed", "x", |_| async { Ok(()) })
        };
        assert_eq!(
            app().run_with(args(&["serve"]), || config(&[])),
            ExitCode::from(EXIT_CONFIG)
        );
        assert_eq!(
            app().run_with(args(&["routes"]), || config(&[])),
            ExitCode::from(EXIT_CONFIG)
        );
        assert_eq!(
            app().run_with(args(&["seed"]), || config(&[])),
            ExitCode::from(EXIT_CONFIG)
        );
    }
}

#[test]
fn the_debug_form_lists_what_was_declared() {
    let app = App::new("demo")
        .command("seed", "x", |_| async { Ok(()) })
        .routes(|_| Ok(()));
    #[cfg(feature = "sqlite")]
    let app = app.sqlite(forge_db::Migrator::DEFAULT);

    let debug = format!("{app:?}");
    assert!(debug.contains(r#"name: "demo""#), "{debug}");
    assert!(debug.contains("route_registrations: 1"), "{debug}");
    assert!(debug.contains(r#"commands: ["seed"]"#), "{debug}");
    #[cfg(feature = "sqlite")]
    assert!(debug.contains("App::sqlite"), "{debug}");
}

#[test]
fn the_openapi_command_prints_the_document_and_reports_what_it_cannot_do() {
    let app = || {
        App::new("demo").routes(|router| {
            router.route(Method::GET, "/plain", |_| async {
                Response::empty(StatusCode::OK)
            })?;
            Ok(())
        })
    };
    assert_eq!(
        app().run_with(args(&["openapi"]), || config(&[])),
        ExitCode::SUCCESS,
        "to standard output"
    );

    let directory = tempfile::tempdir().expect("tempdir");
    let missing_directory = directory.path().join("missing").join("openapi.json");
    let missing_directory = missing_directory.to_string_lossy().into_owned();
    assert_eq!(
        app().run_with(args(&["openapi", "--output", &missing_directory]), || {
            config(&[])
        }),
        ExitCode::FAILURE,
        "an unwritable file"
    );
    assert_eq!(
        app().run_with(
            args(&["openapi", "--check", &missing_directory]),
            || config(&[])
        ),
        ExitCode::FAILURE,
        "an unreadable file"
    );

    // A description that contradicts itself stops the command, like startup.
    let invalid = App::new("demo").routes(|router| {
        router.route(
            Method::GET,
            "/x",
            forge_http::describe(
                |_| async { Response::empty(StatusCode::OK) },
                forge_openapi::Operation::new(),
            ),
        )?;
        Ok(())
    });
    assert_eq!(
        invalid.run_with(args(&["openapi"]), || config(&[])),
        ExitCode::FAILURE
    );
}

#[test]
fn serve_refuses_to_start_with_an_inaccurate_description() {
    let invalid = App::new("demo").routes(|router| {
        router.route(
            Method::GET,
            "/x",
            forge_http::describe(
                |_| async { Response::empty(StatusCode::OK) },
                forge_openapi::Operation::new(),
            ),
        )?;
        Ok(())
    });
    assert_eq!(
        invalid.run_with(args(&["serve"]), || config(&[("FORGE_LOG", "error")])),
        ExitCode::FAILURE
    );
}

#[test]
fn legacy_migration_handlers_report_their_failures_and_absence() {
    let url = [(
        "FORGE_MIGRATION_DATABASE_URL",
        "postgres://u:p@127.0.0.1:1/app",
    )];
    let failing = App::new("demo").migrations(
        |_| async { Err::<(), _>(io::Error::other("migration exploded")) },
        |_| async { Err::<(), _>(io::Error::other("rollback exploded")) },
    );
    assert_eq!(
        failing.run_with(args(&["migrate"]), || config(&url)),
        ExitCode::FAILURE
    );
    let failing = App::new("demo").migrations(
        |_| async { Ok::<(), io::Error>(()) },
        |_| async { Err::<(), _>(io::Error::other("rollback exploded")) },
    );
    assert_eq!(
        failing.run_with(args(&["rollback"]), || config(&url)),
        ExitCode::FAILURE
    );
    // Neither declared nor registered: nothing can migrate.
    for command in [&["migrate"][..], &["rollback"]] {
        assert_eq!(
            App::new("demo").run_with(args(command), || config(&url)),
            ExitCode::FAILURE,
            "{command:?}"
        );
    }
    // And without the credential the command cannot even start.
    assert_eq!(
        App::new("demo").run_with(args(&["migrate"]), || config(&[])),
        ExitCode::from(EXIT_CONFIG)
    );
}

#[cfg(feature = "sqlite")]
#[test]
fn inspection_commands_compose_over_the_declared_database_and_close_it() {
    let directory = tempfile::tempdir().expect("tempdir");
    let url = sqlite_url(&directory);
    let app = || {
        marker_app()
            .sqlite(forge_db::Migrator::DEFAULT)
            .compose(|composition| {
                composition.sqlite()?;
                Ok(())
            })
    };
    for command in [&["routes"][..], &["openapi"]] {
        let code = app().run_with(args(command), || {
            config(&[("FORGE_DATABASE_URL", &url), ("FORGE_LOG", "error")])
        });
        assert_eq!(code, ExitCode::SUCCESS, "{command:?}");
    }
}
