//! The lifecycle commands (`forge migrate`, `forge db ...`, `forge dev`) end
//! to end with the real `forge` binary, a fake `cargo` and a fake application:
//! no real build, no database. The fake `cargo build` prints the machine
//! readable message naming the fake application, which records its arguments
//! and the environment it received.
//!
//! Real builds against PostgreSQL are in `tests/scaffold.rs`.
#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const FORGE: &str = env!("CARGO_BIN_EXE_forge");

const FAKE_CARGO: &str = r#"#!/bin/sh
echo "cargo $*" >> "$RECORD"
printf '{"reason":"compiler-artifact","target":{"kind":["bin"],"name":"shop"},"executable":"%s"}\n' "$FAKE_APP"
exit "${CARGO_EXIT:-0}"
"#;

const FAKE_APP: &str = r#"#!/bin/sh
echo "app $*" >> "$RECORD"
echo "allow=${FORGE_ALLOW_DESTRUCTIVE-unset} url=${FORGE_DATABASE_URL-unset}" >> "$RECORD"
if [ "$1 $2" = "db exists" ]; then exit "${EXISTS_CODE:-0}"; fi
exit "${APP_EXIT:-0}"
"#;

/// A `psql` stand-in: records its arguments, the connection variables it was
/// given and the script on its standard input.
const FAKE_PSQL: &str = r#"#!/bin/sh
echo "psql $*" >> "$RECORD"
echo "env user=$PGUSER password=$PGPASSWORD host=$PGHOST port=$PGPORT db=$PGDATABASE service=${PGSERVICE-unset} forge_url=${FORGE_DATABASE_URL-unset}" >> "$RECORD"
if [ "$1 $2" != "-X -c" ] && [ "$#" -gt 3 ]; then cat >> "$RECORD.sql"; fi
exit "${PSQL_EXIT:-0}"
"#;

const RUNTIME_URL: &str = "postgres://app_runtime:rt-secret@127.0.0.1:5432/shop";
const MIGRATION_URL: &str = "postgres://app_migrator:mg-secret@127.0.0.1:5432/shop";

const INIT_SCRIPT: &str = "CREATE ROLE app_migrator LOGIN;\nCREATE DATABASE shop OWNER app_migrator;\n\n\\connect shop\n\nGRANT CONNECT ON DATABASE shop TO app_runtime;\n";

const SERVER_APP: &str = r#"#!/bin/sh
trap 'echo stopped >> "$RECORD"; exit 0' TERM
echo started >> "$RECORD"
while :; do sleep 0.05; done
"#;

struct Fixture {
    directory: tempfile::TempDir,
}

impl Fixture {
    fn new(app_script: &str) -> Self {
        let directory = tempfile::tempdir().expect("tempdir");
        let root = directory.path();
        fs::create_dir_all(root.join("shop/src")).expect("mkdir");
        fs::write(
            root.join("shop/Cargo.toml"),
            "[package]\nname = \"shop\"\nversion = \"0.1.0\"\n\n[dependencies]\nforge = { path = \"../../forge\" }\n",
        )
        .expect("manifest");
        fs::write(root.join("shop/src/main.rs"), "fn main() {}\n").expect("main");
        fs::create_dir_all(root.join("bin")).expect("mkdir");
        executable(&root.join("bin/cargo"), FAKE_CARGO);
        executable(&root.join("app"), app_script);
        Self { directory }
    }

    fn project(&self) -> PathBuf {
        self.directory.path().join("shop")
    }

    fn record(&self) -> PathBuf {
        self.directory.path().join("record")
    }

    fn lines(&self) -> Vec<String> {
        fs::read_to_string(self.record())
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// Only the `app ...` lines the fake application recorded.
    fn app_calls(&self) -> Vec<String> {
        self.lines()
            .into_iter()
            .filter_map(|line| line.strip_prefix("app ").map(str::to_owned))
            .collect()
    }

    fn command(&self, arguments: &[&str]) -> Command {
        let path = format!(
            "{}:{}",
            self.directory.path().join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut command = forge_command();
        command
            .args(arguments)
            .current_dir(self.project())
            .env("PATH", path)
            .env("RECORD", self.record())
            .env("FAKE_APP", self.directory.path().join("app"))
            .env_remove("FORGE_ENV")
            .env_remove("FORGE_ALLOW_DESTRUCTIVE")
            .env_remove("FORGE_DATABASE_URL")
            .stdin(Stdio::null());
        command
    }

    fn run(&self, arguments: &[&str]) -> Output {
        self.command(arguments).output().expect("forge runs")
    }
}

/// A `forge` command without the test harness's own `FORGE_TEST_*` variables
/// (whichever CI sets: database URLs, `FORGE_TEST_REQUIRE_SSHD`,
/// `FORGE_TEST_FAKE_DOCKER`, ...): the application's configuration loader
/// (and `forge doctor`) refuse unknown `FORGE_*` keys.
fn forge_command() -> Command {
    let mut command = Command::new(FORGE);
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("FORGE_TEST_") {
            command.env_remove(name);
        }
    }
    command
}

fn executable(path: &Path, content: &str) {
    write_executable(path, content);
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn migrate_rollback_seed_and_setup_run_the_applications_own_commands() {
    let fixture = Fixture::new(FAKE_APP);
    for arguments in [
        &["migrate"][..],
        &["migrate", "status"],
        &["rollback"],
        &["rollback", "--steps", "2"],
        &["seed"],
        &[
            "setup",
            "--tenant",
            "acme",
            "--email",
            "owner@example.com",
            "--organization",
            "Acme Inc",
        ],
    ] {
        let output = fixture.run(arguments);
        assert!(
            output.status.success(),
            "{arguments:?}: {}",
            stderr(&output)
        );
    }
    assert_eq!(
        fixture.app_calls(),
        [
            "migrate",
            "migrate status",
            "rollback",
            "rollback --steps 2",
            "seed",
            "setup --tenant acme --email owner@example.com --organization Acme Inc",
        ]
    );
    let builds: Vec<String> = fixture
        .lines()
        .into_iter()
        .filter(|line| line.starts_with("cargo "))
        .collect();
    assert_eq!(builds.len(), 6, "one build per forge invocation");
    assert!(
        builds.iter().all(|line| line
            == "cargo build --locked --message-format=json-render-diagnostics --bin shop"),
        "{builds:?}"
    );
}

#[test]
fn the_database_url_is_passed_through_and_the_clis_switch_is_not() {
    let fixture = Fixture::new(FAKE_APP);
    let output = fixture
        .command(&["db", "drop", "--force-production", "--yes"])
        .env("FORGE_ENV", "production")
        .env("FORGE_ALLOW_DESTRUCTIVE", "true")
        .env("FORGE_DATABASE_URL", "postgres://u:p@127.0.0.1/shop")
        .output()
        .expect("forge runs");
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(fixture.app_calls(), ["db drop --force-production"]);
    assert!(
        fixture
            .lines()
            .contains(&"allow=unset url=postgres://u:p@127.0.0.1/shop".to_owned()),
        "{:?}",
        fixture.lines()
    );
    // And the CLI printed nothing of the URL.
    let everything = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        stderr(&output)
    );
    assert!(!everything.contains("postgres://"), "{everything}");
}

#[test]
fn db_setup_reset_and_prepare_compose_the_applications_commands() {
    let fixture = Fixture::new(FAKE_APP);
    assert!(fixture.run(&["db", "setup"]).status.success());
    assert!(fixture.run(&["db", "reset"]).status.success());
    assert!(fixture.run(&["db", "prepare"]).status.success());
    assert_eq!(
        fixture.app_calls(),
        [
            "db create",
            "migrate",
            "seed",
            "db drop",
            "db create",
            "migrate",
            "seed",
            "db exists",
            "migrate",
        ]
    );

    let missing = Fixture::new(FAKE_APP);
    let output = missing
        .command(&["db", "prepare"])
        .env("EXISTS_CODE", "3")
        .output()
        .expect("forge runs");
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        missing.app_calls(),
        ["db exists", "db create", "migrate", "seed"]
    );
}

impl Fixture {
    /// Puts the fake `psql` on the `PATH` and returns the command with both
    /// database URLs set.
    fn with_psql(&self, arguments: &[&str]) -> Command {
        executable(&self.directory.path().join("bin/psql"), FAKE_PSQL);
        let mut command = self.command(arguments);
        command
            .env("FORGE_DATABASE_URL", RUNTIME_URL)
            .env("FORGE_MIGRATION_DATABASE_URL", MIGRATION_URL)
            .env("PGSERVICE", "elsewhere");
        command
    }

    fn init_script(&self) {
        let path = self.project().join("docker/postgres/init.sql");
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        fs::write(path, INIT_SCRIPT).expect("init.sql");
    }
}

#[test]
fn db_console_runs_the_client_with_the_password_in_the_environment_only() {
    let fixture = Fixture::new(FAKE_APP);
    let output = fixture
        .with_psql(&["db", "console", "--", "-c", "select 1"])
        .output()
        .expect("forge runs");
    assert!(output.status.success(), "{}", stderr(&output));
    let lines = fixture.lines();
    assert_eq!(
        lines[0], "psql -X -c select 1",
        "arguments after -- reach the client"
    );
    assert_eq!(
        lines[1],
        "env user=app_migrator password=mg-secret host=127.0.0.1 port=5432 db=shop \
         service=unset forge_url=unset",
        "the migration role's credentials, the URL's database, no inherited PG* variable"
    );
    assert_eq!(
        fixture.app_calls(),
        Vec::<String>::new(),
        "the application is not built or run"
    );
    assert!(
        !lines[0].contains("secret"),
        "the password is not an argument: {}",
        lines[0]
    );
    let shown = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        stderr(&output)
    );
    assert!(!shown.contains("secret"), "{shown}");
}

#[test]
fn db_console_names_a_missing_client_and_a_missing_url_without_leaking_either() {
    let fixture = Fixture::new(FAKE_APP);
    let mut command = fixture.command(&["db", "console"]);
    command
        .env("FORGE_DATABASE_URL", RUNTIME_URL)
        .env("PATH", fixture.directory.path().join("bin"));
    let output = command.output().expect("forge runs");
    assert!(!output.status.success());
    let text = stderr(&output);
    assert!(
        text.contains("psql was not found on PATH: install the postgres client tools"),
        "{text}"
    );
    assert!(!text.contains("secret"), "{text}");

    let output = fixture.run(&["db", "console"]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("set FORGE_DATABASE_URL"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn db_console_returns_the_clients_failure() {
    let fixture = Fixture::new(FAKE_APP);
    let output = fixture
        .with_psql(&["db", "console"])
        .env("PSQL_EXIT", "2")
        .output()
        .expect("forge runs");
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("psql exited with status 2"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn db_reset_restores_the_init_script_privileges_after_creating_the_database() {
    let fixture = Fixture::new(FAKE_APP);
    fixture.init_script();
    let output = fixture
        .with_psql(&["db", "reset"])
        .output()
        .expect("forge runs");
    assert!(output.status.success(), "{}", stderr(&output));
    let order: Vec<String> = fixture
        .lines()
        .into_iter()
        .filter(|line| line.starts_with("app ") || line.starts_with("psql "))
        .collect();
    assert_eq!(
        order,
        [
            "app db drop",
            "app db create",
            "psql -X -q -w -v ON_ERROR_STOP=1 -1 -f -",
            "app migrate",
            "app seed",
        ]
    );
    let script = fs::read_to_string(format!("{}.sql", fixture.record().display())).expect("script");
    assert_eq!(script, "\nGRANT CONNECT ON DATABASE shop TO app_runtime;\n");
    assert!(
        fixture
            .lines()
            .iter()
            .any(|line| line.contains("user=app_migrator password=mg-secret"))
    );
    let shown = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        stderr(&output)
    );
    assert!(
        shown.contains("restored the privileges of docker/postgres/init.sql"),
        "{shown}"
    );
    assert!(!shown.contains("secret"), "{shown}");
}

#[test]
fn db_reset_without_psql_refuses_before_dropping_the_database() {
    let fixture = Fixture::new(FAKE_APP);
    fixture.init_script();
    let mut command = fixture.command(&["db", "reset"]);
    command
        .env("FORGE_DATABASE_URL", RUNTIME_URL)
        .env("FORGE_MIGRATION_DATABASE_URL", MIGRATION_URL)
        .env("PATH", fixture.directory.path().join("bin"));
    let output = command.output().expect("forge runs");
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("psql was not found on PATH"),
        "{}",
        stderr(&output)
    );
    assert!(!stderr(&output).contains("secret"));
    assert_eq!(
        fixture.app_calls(),
        Vec::<String>::new(),
        "nothing was dropped, nothing built"
    );
}

#[test]
fn the_development_environment_files_reach_the_application_and_the_process_wins() {
    let fixture = Fixture::new(FAKE_APP);
    fs::write(
        fixture.project().join(".env.development"),
        "# committed\nFORGE_DATABASE_URL=postgres://dev@127.0.0.1/shop_development\n",
    )
    .expect("env file");
    fs::write(
        fixture.project().join(".env.local"),
        "export FORGE_DATABASE_URL='postgres://local@127.0.0.1/shop_development'\n",
    )
    .expect("env file");
    let output = fixture.run(&["migrate"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let output = fixture
        .command(&["migrate"])
        .env("FORGE_DATABASE_URL", "postgres://process@127.0.0.1/shop")
        .output()
        .expect("forge runs");
    assert!(output.status.success(), "{}", stderr(&output));
    let urls: Vec<String> = fixture
        .lines()
        .into_iter()
        .filter(|line| line.starts_with("allow="))
        .collect();
    assert_eq!(
        urls,
        [
            "allow=unset url=postgres://local@127.0.0.1/shop_development",
            "allow=unset url=postgres://process@127.0.0.1/shop",
        ]
    );

    // A file can make the environment production, never confirm a
    // destructive command.
    fs::write(
        fixture.project().join(".env.local"),
        "FORGE_ENV=production\nFORGE_ALLOW_DESTRUCTIVE=true\n",
    )
    .expect("env file");
    let output = fixture.run(&["db", "drop", "--force-production", "--yes"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("--yes here also needs FORGE_ALLOW_DESTRUCTIVE=true"),
        "{}",
        stderr(&output)
    );

    // A malformed file stops the command before anything runs, without
    // showing the line.
    fs::write(fixture.project().join(".env.local"), "TOKEN hunter2\n").expect("env file");
    let calls = fixture.app_calls().len();
    let output = fixture.run(&["migrate"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains(".env.local: line 1: expected KEY=VALUE"),
        "{}",
        stderr(&output)
    );
    assert!(!stderr(&output).contains("hunter2"));
    assert_eq!(fixture.app_calls().len(), calls);
}

#[test]
fn production_refuses_destructive_commands_before_building_anything() {
    for arguments in [
        &["db", "drop"][..],
        &["db", "reset"],
        &["db", "rollback"],
        &["db", "rollback", "--steps", "2"],
    ] {
        let fixture = Fixture::new(FAKE_APP);
        let output = fixture
            .command(arguments)
            .env("FORGE_ENV", "production")
            .output()
            .expect("forge runs");
        assert_eq!(output.status.code(), Some(1), "{arguments:?}");
        let message = stderr(&output);
        assert!(
            message.contains("refusing to ")
                && message.contains("FORGE_ENV is production")
                && message.contains("pass --force-production, and confirm"),
            "{arguments:?}: {message}"
        );
        assert!(
            !fixture.record().exists(),
            "{arguments:?}: neither cargo nor the application ran"
        );
    }
}

#[test]
fn force_production_needs_confirmation_and_yes_needs_the_environment() {
    let fixture = Fixture::new(FAKE_APP);
    // Not a terminal, no --yes: refused.
    let output = fixture
        .command(&["db", "drop", "--force-production"])
        .env("FORGE_ENV", "production")
        .output()
        .expect("forge runs");
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("needs an interactive terminal"),
        "{}",
        stderr(&output)
    );

    // --yes without FORGE_ALLOW_DESTRUCTIVE=true: refused.
    let output = fixture
        .command(&["db", "drop", "--force-production", "--yes"])
        .env("FORGE_ENV", "production")
        .output()
        .expect("forge runs");
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("FORGE_ALLOW_DESTRUCTIVE=true"),
        "{}",
        stderr(&output)
    );
    assert!(!fixture.record().exists());

    // --yes alone is a usage error: it only means something with --force-production.
    let output = fixture.run(&["db", "drop", "--yes"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        stderr(&output).contains("--force-production"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn a_failing_application_command_fails_the_cli() {
    let fixture = Fixture::new(FAKE_APP);
    let output = fixture
        .command(&["migrate"])
        .env("APP_EXIT", "78")
        .output()
        .expect("forge runs");
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("application command \"migrate\" failed with status 78"),
        "{}",
        stderr(&output)
    );

    let output = fixture
        .command(&["migrate"])
        .env("CARGO_EXIT", "101")
        .output()
        .expect("forge runs");
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("`cargo build` failed with exit code 101"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn lifecycle_commands_refuse_directories_that_are_not_forge_applications() {
    let directory = tempfile::tempdir().expect("tempdir");
    let output = Command::new(FORGE)
        .args(["db", "setup"])
        .current_dir(directory.path())
        .output()
        .expect("forge runs");
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("no Cargo.toml here: run this inside a Forge application"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn openapi_runs_the_applications_command_with_absolute_files() {
    let fixture = Fixture::new(FAKE_APP);
    for arguments in [
        &["openapi"][..],
        &["openapi", "--output", "openapi.json"],
        &["openapi", "--check", "openapi.json"],
    ] {
        let output = fixture.run(arguments);
        assert!(
            output.status.success(),
            "{arguments:?}: {}",
            stderr(&output)
        );
    }
    let file = fixture
        .project()
        .canonicalize()
        .expect("project")
        .join("openapi.json");
    assert_eq!(
        fixture.app_calls(),
        [
            "openapi".to_owned(),
            format!("openapi --output {}", file.display()),
            format!("openapi --check {}", file.display()),
        ]
    );
    let failed = fixture
        .command(&["openapi", "--check", "openapi.json"])
        .env("APP_EXIT", "1")
        .output()
        .expect("forge runs");
    assert_eq!(failed.status.code(), Some(1), "{}", stderr(&failed));
}

#[test]
fn routes_runs_the_applications_command() {
    let fixture = Fixture::new(FAKE_APP);
    let output = fixture.run(&["routes"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(fixture.app_calls(), ["routes".to_owned()]);
    let failed = fixture
        .command(&["routes"])
        .env("APP_EXIT", "1")
        .output()
        .expect("forge runs");
    assert_eq!(failed.status.code(), Some(1), "{}", stderr(&failed));
    let extra = fixture.run(&["routes", "--json"]);
    assert_eq!(extra.status.code(), Some(2), "{}", stderr(&extra));
}

#[test]
fn argument_errors_are_usage_errors() {
    let fixture = Fixture::new(FAKE_APP);
    for arguments in [
        &["rollback", "--steps", "0"][..],
        &["rollback", "--steps", "many"],
        &["setup", "--tenant", "acme"],
        &["migrate", "now"],
        &["db"],
        &["db", "explode"],
        &["dev", "--poll-interval", "10ms"],
        &["dev", "--poll-interval", "fast"],
        &["dev", "--debounce", "2m"],
        &["openapi", "--output", "a.json", "--check", "b.json"],
    ] {
        let output = fixture.run(arguments);
        assert_eq!(
            output.status.code(),
            Some(2),
            "{arguments:?}: {}",
            stderr(&output)
        );
    }
    assert!(!fixture.record().exists(), "nothing was built");
}

#[test]
fn version_prints_the_cli_version() {
    let output = Command::new(FORGE)
        .arg("version")
        .output()
        .expect("forge runs");
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!("forge {}\n", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn doctor_reports_an_unreachable_database_without_leaking_the_url() {
    // Bound and never listening: connecting is refused, and unlike a probed
    // port that was closed, nobody else can take it while the test runs.
    let refusing = tokio::net::TcpSocket::new_v4().expect("socket");
    refusing
        .bind("127.0.0.1:0".parse().expect("address"))
        .expect("bind");
    let free = refusing.local_addr().expect("address").port();
    let url = format!("postgres://app_runtime:hunter2@127.0.0.1:{free}/shop");
    let output = forge_command()
        .arg("doctor")
        .env("FORGE_DATABASE_URL", &url)
        .env_remove("FORGE_ENV")
        .output()
        .expect("forge runs");
    assert_eq!(output.status.code(), Some(1));
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(text.contains("configuration: ok"), "{text}");
    assert!(
        text.contains(&format!(
            "database: postgres database \"shop\" on 127.0.0.1:{free}"
        )),
        "{text}"
    );
    assert!(
        text.contains("database reachability: unreachable"),
        "{text}"
    );
    assert!(
        !text.contains("hunter2") && !text.contains("app_runtime"),
        "{text}"
    );

    let json = forge_command()
        .args(["doctor", "--json"])
        .env("FORGE_DATABASE_URL", &url)
        .env_remove("FORGE_ENV")
        .output()
        .expect("forge runs");
    assert_eq!(json.status.code(), Some(1));
    let text = String::from_utf8_lossy(&json.stdout).into_owned();
    assert!(text.contains("\"schema_version\":1") && text.contains("\"database.unreachable\""));
    assert!(!text.contains("hunter2"), "{text}");
}

#[test]
fn doctor_is_healthy_with_a_listening_database_and_no_database() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("address").port();
    let output = forge_command()
        .arg("doctor")
        .env(
            "FORGE_DATABASE_URL",
            format!("postgres://u:p@127.0.0.1:{port}/shop"),
        )
        .env_remove("FORGE_ENV")
        .output()
        .expect("forge runs");
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        stderr(&output)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("the server accepts connections"));
    drop(listener);

    let output = forge_command()
        .args(["doctor", "--json"])
        .env_remove("FORGE_DATABASE_URL")
        .env_remove("FORGE_ENV")
        .output()
        .expect("forge runs");
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        r#"{"database":"none","diagnostics":[],"environment":"development","ok":true,"schema_version":1}"#
    );
}

fn wait_for(fixture: &Fixture, line: &str, count: usize) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while fixture.lines().iter().filter(|l| *l == line).count() < count {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {count} x {line:?}: {:?}",
            fixture.lines()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn signal(child: &Child, name: &str) {
    let status = Command::new("kill")
        .args([&format!("-{name}"), &child.id().to_string()])
        .status()
        .expect("kill runs");
    assert!(status.success());
}

#[test]
fn dev_rebuilds_and_restarts_on_change_and_stops_its_child_on_sigterm() {
    let fixture = Fixture::new(SERVER_APP);
    let mut forge = fixture
        .command(&["dev", "--poll-interval", "50ms", "--debounce", "0"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("forge dev starts");

    wait_for(&fixture, "started", 1);
    // An edit builds again; the new process replaces the old one.
    fs::write(
        fixture.project().join("src/main.rs"),
        "fn main() { println!(\"changed\"); }\n",
    )
    .expect("edit");
    wait_for(&fixture, "started", 2);
    wait_for(&fixture, "stopped", 1);
    let builds = fixture
        .lines()
        .iter()
        .filter(|line| line.starts_with("cargo "))
        .count();
    assert_eq!(builds, 2, "{:?}", fixture.lines());

    // A change outside the watched files does nothing.
    fs::write(fixture.project().join("README.md"), "notes\n").expect("write");
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(
        fixture.lines().iter().filter(|l| *l == "started").count(),
        2
    );

    signal(&forge, "TERM");
    let status = forge.wait().expect("forge dev ends");
    assert!(status.success(), "{status:?}");
    // The application got SIGTERM and finished: nothing is left running.
    wait_for(&fixture, "stopped", 2);
    let log = {
        let mut text = String::new();
        std::io::Read::read_to_string(&mut forge.stderr.take().expect("stderr"), &mut text)
            .expect("read stderr");
        text
    };
    assert!(log.contains("forge dev: building"), "{log}");
    assert!(log.contains("forge dev: changed: src/main.rs"), "{log}");
    assert!(log.contains("forge dev: restarting"), "{log}");
}

#[test]
fn dev_keeps_the_running_application_when_a_build_fails() {
    let fixture = Fixture::new(SERVER_APP);
    let mut forge = fixture
        .command(&["dev", "--poll-interval", "50ms", "--debounce", "0"])
        .env("RECORD", fixture.record())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("forge dev starts");
    wait_for(&fixture, "started", 1);

    // The next build fails: the first process must keep running.
    executable(
        &fixture.directory.path().join("bin/cargo"),
        &FAKE_CARGO.replace("exit \"${CARGO_EXIT:-0}\"", "exit 101"),
    );
    fs::write(
        fixture.project().join("src/main.rs"),
        "fn main() { broken }\n",
    )
    .expect("edit");
    wait_for(
        &fixture,
        "cargo build --locked --message-format=json-render-diagnostics --bin shop",
        2,
    );
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        fixture.lines().iter().filter(|l| *l == "started").count(),
        1,
        "no replacement"
    );
    assert_eq!(
        fixture.lines().iter().filter(|l| *l == "stopped").count(),
        0,
        "the old one was not stopped"
    );

    signal(&forge, "INT");
    let status = forge.wait().expect("forge dev ends");
    assert!(status.success(), "{status:?}");
    wait_for(&fixture, "stopped", 1);
}

#[test]
fn the_remaining_database_commands_and_setup_options_reach_the_application() {
    let fixture = Fixture::new(FAKE_APP);
    for arguments in [
        &["db", "create"][..],
        &["db", "migrate"],
        &["db", "status"],
        &["db", "seed"],
        &["db", "rollback", "--steps", "2"],
        &[
            "setup",
            "--tenant",
            "acme",
            "--email",
            "owner@example.com",
            "--password-stdin",
        ],
    ] {
        let output = fixture.run(arguments);
        assert!(
            output.status.success(),
            "{arguments:?}: {}",
            stderr(&output)
        );
    }
    assert_eq!(
        fixture.app_calls(),
        [
            "db create",
            "migrate",
            "migrate status",
            "seed",
            "rollback --steps 2",
            "setup --tenant acme --email owner@example.com --password-stdin",
        ]
    );
}

#[test]
fn dev_without_reload_builds_and_runs_once() {
    let fixture = Fixture::new(FAKE_APP);
    let output = fixture.run(&["dev", "--no-reload"]);
    assert!(output.status.success(), "{}", stderr(&output));
    // The application ran without arguments, in the foreground.
    assert_eq!(fixture.app_calls(), [""]);

    let failing = fixture
        .command(&["dev", "--no-reload"])
        .env("APP_EXIT", "3")
        .output()
        .expect("forge runs");
    assert_eq!(failing.status.code(), Some(1));
    assert!(
        stderr(&failing).contains("application command \"shop\" failed with status 3"),
        "{}",
        stderr(&failing)
    );
}

#[test]
fn dev_refuses_binary_names_cargo_would_not_accept() {
    let fixture = Fixture::new(FAKE_APP);
    for name in ["bad/name", "-flag", "has space", &"x".repeat(65)] {
        let output = fixture.run(&["dev", "--no-reload", "--", name]);
        assert_eq!(
            output.status.code(),
            Some(1),
            "{name:?}: {}",
            stderr(&output)
        );
        assert!(
            stderr(&output).contains("invalid binary name"),
            "{name:?}: {}",
            stderr(&output)
        );
    }
    assert!(fixture.lines().is_empty(), "nothing was built");
    // A valid name selects the binary to build and run.
    let output = fixture.run(&["dev", "--no-reload", "shop"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        fixture
            .lines()
            .iter()
            .any(|line| line.ends_with("--bin shop")),
        "{:?}",
        fixture.lines()
    );
}

#[test]
fn a_signal_during_the_build_stops_cargo_and_the_command() {
    let fixture = Fixture::new(FAKE_APP);
    executable(
        &fixture.directory.path().join("bin/cargo"),
        "#!/bin/sh\necho \"cargo $*\" >> \"$RECORD\"\nexec sleep 30\n",
    );
    let mut forge = fixture
        .command(&["migrate"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("forge starts");
    wait_for(
        &fixture,
        "cargo build --locked --message-format=json-render-diagnostics --bin shop",
        1,
    );
    signal(&forge, "TERM");
    let status = forge.wait().expect("forge ends");
    assert_eq!(status.code(), Some(1), "{status:?}");
    let mut log = String::new();
    std::io::Read::read_to_string(&mut forge.stderr.take().expect("stderr"), &mut log)
        .expect("stderr");
    assert!(log.contains("interrupted"), "{log}");
    assert!(fixture.app_calls().is_empty(), "the application never ran");
}

#[test]
fn a_signal_while_the_application_runs_stops_it_and_ends_the_command() {
    let fixture = Fixture::new(SERVER_APP);
    let mut forge = fixture
        .command(&["migrate"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("forge starts");
    wait_for(&fixture, "started", 1);
    signal(&forge, "TERM");
    let status = forge.wait().expect("forge ends");
    assert_eq!(status.code(), Some(1), "{status:?}");
    wait_for(&fixture, "stopped", 1);
    let mut log = String::new();
    std::io::Read::read_to_string(&mut forge.stderr.take().expect("stderr"), &mut log)
        .expect("stderr");
    assert!(log.contains("interrupted"), "{log}");
}

#[test]
fn doctor_says_whether_the_sqlite_file_exists_yet() {
    let directory = tempfile::tempdir().expect("tempdir");
    let run = || {
        forge_command()
            .arg("doctor")
            .current_dir(directory.path())
            .env("FORGE_DATABASE_URL", "sqlite://shop.db")
            .env_remove("FORGE_ENV")
            .output()
            .expect("forge runs")
    };
    let before = run();
    assert!(
        before.status.success(),
        "{}",
        String::from_utf8_lossy(&before.stdout)
    );
    let text = String::from_utf8_lossy(&before.stdout).into_owned();
    assert!(text.contains("the file does not exist yet"), "{text}");
    fs::write(directory.path().join("shop.db"), "").expect("database file");
    let after = String::from_utf8_lossy(&run().stdout).into_owned();
    assert!(after.contains("the file exists"), "{after}");
}

#[test]
fn doctor_reports_a_toolchain_that_cannot_be_run() {
    let directory = tempfile::tempdir().expect("tempdir");
    let bin = directory.path().join("bin");
    fs::create_dir(&bin).expect("bin");
    // `cargo` runs but fails; there is no `rustc` at all.
    executable(&bin.join("cargo"), "#!/bin/sh\nexit 1\n");
    let output = forge_command()
        .args(["doctor", "--json"])
        .current_dir(directory.path())
        .env("PATH", &bin)
        .env_remove("FORGE_DATABASE_URL")
        .env_remove("FORGE_ENV")
        .output()
        .expect("forge runs");
    assert_eq!(output.status.code(), Some(1));
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(text.contains("\"toolchain.cargo_missing\""), "{text}");
    assert!(text.contains("\"toolchain.rustc_missing\""), "{text}");
}

/// Writes an executable (mode 0755) script at `path` without this process
/// ever holding a write descriptor on it.
///
/// Writing the file here (`std::fs::write`) leaves a writable descriptor open
/// in this process for a moment. Any other test thread that forks in that
/// window (`Command::spawn`) inherits the descriptor until it execs, and
/// while any process holds the inode open for writing `execve` of it fails
/// with `ETXTBSY` ("Text file busy"). `O_CLOEXEC` only closes the descriptor
/// at exec, so it does not close that window. Creating the file from a
/// short-lived child keeps the writer out of this process, so forks on other
/// threads cannot inherit it, and the child has exited before this returns.
fn write_executable(path: &Path, body: &str) {
    use std::io::Write as _;
    use std::process::{Command, Stdio};

    let mut child = Command::new("/bin/sh")
        .args(["-c", "umask 022; cat > \"$1\" && chmod 755 \"$1\"", "sh"])
        .arg(path)
        .stdin(Stdio::piped())
        .spawn()
        .expect("spawn sh to write the executable");
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(body.as_bytes())
        .expect("send the script body");
    let status = child.wait().expect("wait for sh");
    assert!(
        status.success(),
        "writing {} failed: {status}",
        path.display()
    );
}
