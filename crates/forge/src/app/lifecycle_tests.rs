//! `serve`, the application commands and the migration commands run
//! in-process, from the command line down to the signal that ends them.
//!
//! A shutdown signal is process-wide, so every test that installs the
//! handlers holds [`serialized`]. A signal is sent only after the server
//! answered a request: `serve` installs its handlers before it binds, so a
//! served request proves the signal is handled and cannot end the test
//! process.

use std::{
    cell::RefCell,
    io::Read as _,
    net::{SocketAddr, TcpListener as StdListener, TcpStream},
    process::Command as Process,
    sync::{
        Arc, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
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

/// Runs an application through one instantiation of `run_with` whatever
/// closure loads the configuration: every test that goes through it adds to
/// the coverage of the same generic functions, instead of each closure type
/// getting an instantiation of its own.
pub(super) trait RunBoxed {
    fn run_boxed<'a>(
        self,
        command: &[&str],
        load: impl FnOnce() -> Result<AppConfig, ConfigError> + 'a,
    ) -> ExitCode;
}

impl RunBoxed for App {
    fn run_boxed<'a>(
        self,
        command: &[&str],
        load: impl FnOnce() -> Result<AppConfig, ConfigError> + 'a,
    ) -> ExitCode {
        let load: Box<dyn FnOnce() -> Result<AppConfig, ConfigError> + 'a> = Box::new(load);
        self.run_with(args(command), load)
    }
}

pub(super) fn config(vars: &[(&str, &str)]) -> Result<AppConfig, ConfigError> {
    AppConfig::from_vars(
        vars.iter()
            .map(|(key, value)| (OsString::from(key), OsString::from(value))),
    )
}

/// The variable and value with which a test asks for an ephemeral metrics
/// listener (see [`start`]); the HTTP one takes `127.0.0.1:0` as it is.
pub(super) const METRICS_ADDR: &str = "FORGE_TELEMETRY_METRICS_ADDR";
pub(super) const EPHEMERAL: &str = "127.0.0.1:0";

/// The listeners an application binds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Bound {
    Http,
    Metrics,
}

thread_local! {
    /// Where the application running on this thread reports what it bound
    /// (installed by [`start`], which runs it on a thread of its own).
    static REPORT: RefCell<Option<Sender<(Bound, SocketAddr)>>> = const { RefCell::new(None) };
}

/// Called by `serve` and by the metrics binding right after they bound. Tests
/// start applications on port 0 and read the address from here: probing a
/// free port, closing it and binding it later races with every other
/// process that binds a port (the "address already in use" flake).
pub(super) fn listening(listener: Bound, address: SocketAddr) {
    REPORT.with(|report| {
        if let Some(report) = report.borrow().as_ref() {
            let _ = report.send((listener, address));
        }
    });
}

/// An application running on a thread of its own, with the addresses it
/// bound.
pub(super) struct Running {
    thread: JoinHandle<ExitCode>,
    reports: Receiver<(Bound, SocketAddr)>,
    seen: RefCell<Vec<(Bound, SocketAddr)>>,
}

impl Running {
    pub(super) fn is_finished(&self) -> bool {
        self.thread.is_finished()
    }

    pub(super) fn join(self) -> thread::Result<ExitCode> {
        self.thread.join()
    }

    /// The address `listener` bound: waits for it, and fails when the process
    /// exits first (a bind that failed) or nothing is bound within a minute.
    pub(super) fn port(&self, listener: Bound) -> u16 {
        let deadline = StdInstant::now() + Duration::from_secs(60);
        loop {
            self.drain(Duration::from_millis(10));
            if let Some((_, address)) = self
                .seen
                .borrow()
                .iter()
                .find(|(kind, _)| *kind == listener)
            {
                return address.port();
            }
            assert!(
                !self.thread.is_finished(),
                "the process exited before binding {listener:?}"
            );
            assert!(
                StdInstant::now() < deadline,
                "{listener:?} was not bound in time"
            );
        }
    }

    /// Whether `listener` has been bound (not waiting for it).
    pub(super) fn bound(&self, listener: Bound) -> bool {
        self.drain(Duration::ZERO);
        self.seen.borrow().iter().any(|(kind, _)| *kind == listener)
    }

    fn drain(&self, wait: Duration) {
        match self.reports.recv_timeout(wait) {
            Ok(report) => {
                self.seen.borrow_mut().push(report);
                while let Ok(report) = self.reports.try_recv() {
                    self.seen.borrow_mut().push(report);
                }
            }
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {}
        }
    }
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
pub(super) fn wait_until_serving(running: &Running, port: u16, path: &str) -> Reply {
    let deadline = StdInstant::now() + Duration::from_secs(60);
    loop {
        assert!(!running.is_finished(), "the process exited before serving");
        match request(port, "GET", path) {
            Ok(reply) => return reply,
            // real-clock: polls a real TCP listener or connection, which a fake clock cannot advance, until a deadline
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
) -> Running {
    let (report, reports) = mpsc::channel();
    let thread = thread::spawn(move || {
        REPORT.with(|slot| *slot.borrow_mut() = Some(report));
        app.run_boxed(command, move || {
            // FORGE_TELEMETRY_METRICS_ADDR=127.0.0.1:0 asks the kernel for a
            // port, read back from the bound listener.
            let vars: Vec<(&str, &str)> = vars
                .iter()
                .map(|(key, value)| (*key, value.as_str()))
                .collect();
            config(&vars)
        })
    });
    Running {
        thread,
        reports,
        seen: RefCell::new(Vec::new()),
    }
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
            ("FORGE_HTTP_ADDR", "127.0.0.1:0".to_owned()),
            (METRICS_ADDR, EPHEMERAL.to_owned()),
            ("FORGE_DATABASE_URL", sqlite_url(&directory)),
            ("FORGE_HTTP_SHUTDOWN_TIMEOUT", "2s".to_owned()),
        ],
    );
    let (port, metrics) = (running.port(Bound::Http), running.port(Bound::Metrics));

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
    let running = start(
        marker_app(),
        &["serve"],
        vec![("FORGE_HTTP_ADDR", "127.0.0.1:0".to_owned())],
    );
    let port = running.port(Bound::Http);
    assert_eq!(wait_until_serving(&running, port, "/marker").status, 200);

    raise("INT");
    assert_eq!(running.join().expect("serve thread"), ExitCode::SUCCESS);
}

#[test]
fn serve_fails_when_an_address_is_taken() {
    let _serial = serialized();
    let taken = StdListener::bind("127.0.0.1:0").expect("a listener");
    let taken_port = taken.local_addr().expect("address").port();

    let main = start(
        marker_app(),
        &["serve"],
        vec![("FORGE_HTTP_ADDR", format!("127.0.0.1:{taken_port}"))],
    );
    assert_eq!(main.join().expect("serve thread"), ExitCode::FAILURE);

    let metrics = start(
        marker_app(),
        &["serve"],
        vec![
            ("FORGE_HTTP_ADDR", "127.0.0.1:0".to_owned()),
            (
                "FORGE_TELEMETRY_METRICS_ADDR",
                format!("127.0.0.1:{taken_port}"),
            ),
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
        duplicate.run_boxed(&["serve"], || config(&[])),
        ExitCode::FAILURE,
        "a duplicate route"
    );
    let failing = App::new("demo").compose(|_| Err(ComposeError::other(io::Error::other("no"))));
    assert_eq!(
        failing.run_boxed(&["serve"], || config(&[])),
        ExitCode::FAILURE,
        "a composition hook"
    );
    let failing = App::new("demo").compose(|_| Err(ComposeError::other(io::Error::other("no"))));
    assert_eq!(
        failing.run_boxed(&["routes"], || config(&[])),
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
            app().run_boxed(command, || config(&vars)),
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
        app.run_boxed(command, || config(&[("FORGE_MIGRATION_DATABASE_URL", url)]))
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
            app().run_boxed(&["serve"], || config(&[])),
            ExitCode::from(EXIT_CONFIG)
        );
        assert_eq!(
            app().run_boxed(&["routes"], || config(&[])),
            ExitCode::from(EXIT_CONFIG)
        );
        assert_eq!(
            app().run_boxed(&["seed"], || config(&[])),
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
        app().run_boxed(&["openapi"], || config(&[])),
        ExitCode::SUCCESS,
        "to standard output"
    );

    let directory = tempfile::tempdir().expect("tempdir");
    let missing_directory = directory.path().join("missing").join("openapi.json");
    let missing_directory = missing_directory.to_string_lossy().into_owned();
    assert_eq!(
        app().run_boxed(&["openapi", "--output", &missing_directory], || {
            config(&[])
        }),
        ExitCode::FAILURE,
        "an unwritable file"
    );
    assert_eq!(
        app().run_boxed(&["openapi", "--check", &missing_directory], || config(&[])),
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
        invalid.run_boxed(&["openapi"], || config(&[])),
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
        invalid.run_boxed(&["serve"], || config(&[])),
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
        failing.run_boxed(&["migrate"], || config(&url)),
        ExitCode::FAILURE
    );
    let failing = App::new("demo").migrations(
        |_| async { Ok::<(), io::Error>(()) },
        |_| async { Err::<(), _>(io::Error::other("rollback exploded")) },
    );
    assert_eq!(
        failing.run_boxed(&["rollback"], || config(&url)),
        ExitCode::FAILURE
    );
    // Neither declared nor registered: nothing can migrate.
    for command in [&["migrate"][..], &["rollback"]] {
        assert_eq!(
            App::new("demo").run_boxed(command, || config(&url)),
            ExitCode::FAILURE,
            "{command:?}"
        );
    }
    // And without the credential the command cannot even start.
    assert_eq!(
        App::new("demo").run_boxed(&["migrate"], || config(&[])),
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
        let code = app().run_boxed(command, || config(&[("FORGE_DATABASE_URL", &url)]));
        assert_eq!(code, ExitCode::SUCCESS, "{command:?}");
    }
}

#[test]
fn a_configuration_that_cannot_be_loaded_is_a_configuration_error_for_every_command() {
    let unloadable = || {
        Err(ConfigError::NotUnicode {
            key: "FORGE_HTTP_ADDR",
        })
    };
    let app = || {
        let app = App::new("demo").command("seed", "x", |_| async { Ok(()) });
        #[cfg(feature = "sqlite")]
        let app = app.sqlite(forge_db::Migrator::DEFAULT);
        app
    };
    let mut commands: Vec<&[&str]> = vec![
        &["serve"],
        &["migrate"],
        &["migrate", "status"],
        &["rollback"],
        &["db", "create"],
        &["routes"],
        &["openapi"],
        &["seed"],
    ];
    #[cfg(feature = "jobs")]
    commands.extend([&["jobs", "status"][..], &["jobs", "work"]]);
    for command in commands {
        assert_eq!(
            app().run_boxed(command, unloadable),
            ExitCode::from(EXIT_CONFIG),
            "{command:?}"
        );
    }
}

#[cfg(feature = "jobs")]
#[test]
fn an_invalid_log_filter_also_stops_the_jobs_commands() {
    let app = || {
        App::new("demo")
            .postgres(forge_db::Migrator::DEFAULT)
            .jobs(crate::background::WorkerMode::InServe)
    };
    let vars = [("FORGE_LOG", "demo=loudest")];
    for command in [&["jobs", "status"][..], &["jobs", "work"]] {
        assert_eq!(
            app().run_boxed(command, || config(&vars)),
            ExitCode::from(EXIT_CONFIG),
            "{command:?}"
        );
    }
}

#[cfg(feature = "postgres")]
#[test]
fn a_database_url_the_driver_cannot_parse_stops_startup_and_the_db_commands() {
    // The scheme is right, so configuration accepts it; the port is not a number.
    let url = "postgres://user:secret@localhost:notaport/app";
    let vars = [
        ("FORGE_DATABASE_URL", url),
        ("FORGE_MIGRATION_DATABASE_URL", url),
    ];
    let app = || App::new("demo").postgres(forge_db::Migrator::DEFAULT);
    for command in [
        &["serve"][..],
        &["migrate"],
        &["db", "create"],
        &["db", "exists"],
        &["db", "drop"],
    ] {
        assert_eq!(
            app().run_boxed(command, || config(&vars)),
            ExitCode::FAILURE,
            "{command:?}"
        );
    }
}

#[cfg(feature = "jobs")]
#[test]
fn a_composition_hook_that_fails_stops_the_job_worker_before_it_starts() {
    let app = App::new("demo")
        .postgres(forge_db::Migrator::DEFAULT)
        .jobs(crate::background::WorkerMode::Dedicated)
        .compose(|composition| {
            // The registry is there, and the hook still refuses to compose.
            composition.jobs()?;
            Err(ComposeError::other(io::Error::other("adapter missing")))
        });
    let vars = [("FORGE_DATABASE_URL", "postgres://u:p@127.0.0.1:1/app")];

    assert_eq!(
        app.run_boxed(&["jobs", "work"], || config(&vars)),
        ExitCode::FAILURE
    );
}

#[test]
fn environments_have_their_names() {
    assert_eq!(environment_name(Environment::Development), "development");
    assert_eq!(environment_name(Environment::Test), "test");
    assert_eq!(environment_name(Environment::Production), "production");
}

#[cfg(feature = "sqlite")]
#[test]
fn a_bookkeeping_table_that_cannot_be_read_fails_the_status_command() {
    let directory = tempfile::tempdir().expect("tempdir");
    let url = sqlite_url(&directory);
    let file = directory.path().join("app.db");
    // Another tool's table of the same name: not sqlx's bookkeeping.
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    runtime.block_on(async {
        let database = forge_db::SqliteDatabase::connect(&SecretString::new(&url))
            .await
            .expect("create the file");
        sqlx::query("CREATE TABLE _sqlx_migrations (unrelated TEXT)")
            .execute(database.pool())
            .await
            .expect("table");
        database.close().await;
    });
    assert!(file.exists());
    let app = || App::new("demo").sqlite(sqlx::migrate!("tests/migrations/sqlite"));
    let run = |command: &[&str]| {
        app().run_boxed(command, || {
            config(&[("FORGE_MIGRATION_DATABASE_URL", &url)])
        })
    };

    assert_eq!(run(&["migrate", "status"]), ExitCode::FAILURE);
    assert_eq!(run(&["migrate"]), ExitCode::FAILURE);
}

#[cfg(all(feature = "postgres", feature = "sqlite"))]
#[test]
fn an_application_declaring_two_databases_is_refused_by_every_command() {
    let app = || {
        App::new("demo")
            .postgres(forge_db::Migrator::DEFAULT)
            .sqlite(forge_db::Migrator::DEFAULT)
    };
    for command in [
        &["serve"][..],
        &["migrate"],
        &["migrate", "status"],
        &["rollback"],
        &["db", "exists"],
    ] {
        assert_eq!(
            app().run_boxed(command, || config(&[])),
            ExitCode::FAILURE,
            "{command:?}"
        );
    }
}

#[cfg(all(feature = "jobs", feature = "sqlite"))]
#[test]
fn jobs_need_postgresql_and_the_application_is_refused_on_any_other_backend() {
    let jobs = crate::background::WorkerMode::InServe;
    assert_eq!(
        App::new("demo")
            .sqlite(forge_db::Migrator::DEFAULT)
            .jobs(jobs)
            .run_boxed(&["serve"], || config(&[])),
        ExitCode::FAILURE
    );
    // And without any database there is nothing to keep the jobs in.
    assert_eq!(
        App::new("demo")
            .jobs(jobs)
            .run_boxed(&["serve"], || config(&[])),
        ExitCode::FAILURE
    );
}

#[test]
fn database_commands_without_a_declared_database_say_what_is_missing() {
    let url = "postgres://u:p@127.0.0.1:1/app";
    let with_url = || config(&[("FORGE_MIGRATION_DATABASE_URL", url)]);
    let app = || App::new("demo");

    // `db` administers a declared database.
    assert_eq!(
        app().run_boxed(&["db", "exists"], || config(&[])),
        ExitCode::FAILURE
    );
    // Migrations need a credential, from the environment when nothing is declared.
    assert_eq!(
        app().run_boxed(&["migrate"], || config(&[])),
        ExitCode::from(EXIT_CONFIG)
    );
    // Without a declaration only the application's own handlers can migrate,
    // and they know neither `status` nor several steps.
    for command in [
        &["migrate", "status"][..],
        &["rollback", "--steps", "2"],
        // No handler was registered either.
        &["migrate"],
        &["rollback"],
    ] {
        assert_eq!(
            app().run_boxed(command, with_url),
            ExitCode::FAILURE,
            "{command:?}"
        );
    }
}
