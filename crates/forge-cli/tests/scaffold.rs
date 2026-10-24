//! `forge generate scaffold` end to end: generate a PostgreSQL, a SQLite and
//! a MySQL application, scaffold a global and a tenant-scoped resource in
//! each (the tenant-scoped one and a second global one with `--broadcast`
//! live updates), apply the printed wiring exactly as printed, and run the
//! application's own gates: `cargo fmt --check`, Clippy with warnings denied
//! and `cargo test`.
//!
//! With `FORGE_TEST_DATABASE_URL` the PostgreSQL application also runs its
//! ignored repository, row-level-security and authentication tests, applies
//! and reverts the scaffold migrations with its own `migrate`/`rollback`
//! commands, creates the first owner with `setup`, loads seeds, and a browser
//! session signs in, uses the tenant-scoped scaffold pages and signs out
//! against the running server (with `FORGE_TEST_MYSQL_URL` the MySQL
//! repository tests run too). A PostgreSQL application stripped of its
//! sign-in pages regains them with `forge generate authentication`, and
//! another PostgreSQL application runs its whole lifecycle through the `forge`
//! CLI: `doctor`, `db prepare|status|reset|drop`, `migrate`, `rollback` and
//! `setup`, against a database the CLI itself creates.
//!
//! These tests build whole applications and resolve dependencies with
//! `cargo generate-lockfile`, so they are ignored by default:
//!
//! ```sh
//! cargo test -p forge-cli --test scaffold -- --ignored
//! ```

use std::fs;
use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::Duration;

use forge_db::sqlx;
use forge_db::testing::TestDatabase;

const POST: &[&str] = &[
    "post",
    "title:string",
    "body:text",
    "published:boolean",
    "--global",
];
const EVENT: &[&str] = &[
    "event",
    "name:string",
    "notes:text?",
    "starts_at:timestamp",
    "ends_at:timestamp?",
    "capacity:integer",
    "attendees:integer?",
    "public:boolean",
    "featured:boolean?",
    "venue:uuid",
    "organizer:uuid?",
    "--tenant-scoped",
    "--broadcast",
];
/// A second broadcasting resource: it reuses the streams module the event
/// scaffold wrote.
const NOTE: &[&str] = &["note", "body:text", "--global", "--broadcast"];

fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn run(
    directory: &Path,
    program: &str,
    arguments: &[&str],
    environment: &[(&str, &str)],
) -> Output {
    let output = run_unchecked(directory, program, arguments, environment, None);
    assert!(
        output.status.success(),
        "{program} {arguments:?} failed in {}:\n{}\n{}",
        directory.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

/// Runs `program` with `stdin` piped in, whatever its exit status.
fn run_unchecked(
    directory: &Path,
    program: &str,
    arguments: &[&str],
    environment: &[(&str, &str)],
    stdin: Option<&str>,
) -> Output {
    let mut command = command(directory, program, arguments, environment);
    command
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .unwrap_or_else(|error| panic!("{program} {arguments:?} did not start: {error}"));
    if let (Some(input), Some(mut pipe)) = (stdin, child.stdin.take()) {
        pipe.write_all(input.as_bytes())
            .unwrap_or_else(|error| panic!("stdin: {error}"));
    }
    child
        .wait_with_output()
        .unwrap_or_else(|error| panic!("{program} {arguments:?}: {error}"))
}

fn command(
    directory: &Path,
    program: &str,
    arguments: &[&str],
    environment: &[(&str, &str)],
) -> Command {
    let mut command = Command::new(program);
    command.args(arguments).current_dir(directory).env(
        "CARGO_TARGET_DIR",
        Path::new(env!("CARGO_TARGET_TMPDIR")).join("scaffold-apps"),
    );
    if program != "cargo" {
        // A generated binary refuses unknown FORGE_* keys at startup.
        for name in [
            "FORGE_TEST_DATABASE_URL",
            "FORGE_TEST_MYSQL_URL",
            "FORGE_TEST_MARIADB_URL",
            "FORGE_TEST_REQUIRE_SSHD",
        ] {
            command.env_remove(name);
        }
    }
    for (name, value) in environment {
        command.env(name, value);
    }
    command
}

/// Applies the printed wiring like a careful developer: each `pub mod`
/// line goes into the sorted block of its file, the statements go at the
/// end of the function the wiring names (`fn compose` or `fn jobs_configure`).
fn apply_wiring(root: &Path, printed: &str) {
    let mut declarations = Vec::new();
    let mut statements = Vec::new();
    let mut commands = Vec::new();
    let mut function = String::from("compose");
    let mut section = 0;
    for line in printed.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("1. Declare") {
            section = 1;
        } else if trimmed.starts_with("2. In src/bootstrap/mod.rs") {
            section = 2;
            if let Some(name) = trimmed
                .split("at the end of `fn ")
                .nth(1)
                .and_then(|rest| rest.split('`').next())
            {
                name.clone_into(&mut function);
            }
        } else if trimmed.starts_with("3. In src/bootstrap/mod.rs, in `fn run`") {
            section = 4;
        } else if section == 4 {
            if line.starts_with("    ") {
                commands.push(line.to_owned());
            } else if !commands.is_empty() {
                section = 5;
            }
        } else if section == 1 {
            if let Some((file, declaration)) = trimmed.split_once(" pub mod ") {
                declarations.push((file.trim().to_owned(), format!("pub mod {declaration}")));
            }
        } else if section == 2 {
            if line.starts_with("    ") {
                statements.push(line.to_owned());
            } else if !statements.is_empty() {
                section = 3;
            }
        }
    }
    assert!(
        !declarations.is_empty() && !statements.is_empty(),
        "{printed}"
    );
    // Every printed line was applied: nothing of section 2 may be lost.
    assert!(section >= 2, "{printed}");

    for (file, declaration) in declarations {
        declare(root, &file, declaration);
    }

    // The role example printed for PostgreSQL tenant resources, applied
    // too, so it is proven to compile.
    let mut grant = Vec::new();
    let mut in_example = false;
    for line in printed.lines() {
        if line.contains("`let mut policy`, for example:") {
            in_example = true;
        } else if in_example && line.starts_with("    ") {
            grant.push(line.to_owned());
        } else if in_example && !grant.is_empty() {
            break;
        }
    }
    let policy = "    let mut policy = forge::security::RbacPolicy::new();\n";
    let statements = statements
        .join("\n")
        .replace(policy, &format!("{policy}{}\n", grant.join("\n")));

    let bootstrap = root.join("src/bootstrap/mod.rs");
    let mut source = fs::read_to_string(&bootstrap).unwrap_or_else(|error| panic!("{error}"));
    if !commands.is_empty() {
        let run = source
            .find("        .run()\n")
            .unwrap_or_else(|| panic!("run ends with .run():\n{source}"));
        source.insert_str(run, &format!("{}\n", commands.join("\n")));
    }
    let start = source
        .find(&format!("\nfn {function}("))
        .unwrap_or_else(|| panic!("no fn {function}:\n{source}"));
    let end = start
        + source[start..]
            .find("\n    Ok(())\n}\n")
            .unwrap_or_else(|| panic!("{function} ends with Ok(()):\n{source}"))
        + 1;
    let wired = format!("{}{statements}\n{}", &source[..end], &source[end..]);
    fs::write(&bootstrap, wired).unwrap_or_else(|error| panic!("{error}"));
}

/// Adds `declaration` (`pub mod name;`) to the sorted block of `file`.
fn declare(root: &Path, file: &str, declaration: String) {
    let path = root.join(file);
    let source = fs::read_to_string(&path).unwrap_or_else(|error| panic!("{file}: {error}"));
    let mut lines: Vec<String> = source.lines().map(str::to_owned).collect();
    let is_module = |line: &String| line.starts_with("pub mod ") && line.ends_with(';');
    match lines.iter().position(is_module) {
        Some(start) => {
            let end = start
                + lines[start..]
                    .iter()
                    .take_while(|line| is_module(line))
                    .count();
            let mut block: Vec<String> = lines[start..end].to_vec();
            block.push(declaration);
            block.sort();
            lines.splice(start..end, block);
        }
        None => {
            lines.push(String::new());
            lines.push(declaration);
        }
    }
    fs::write(&path, lines.join("\n") + "\n").unwrap_or_else(|error| panic!("{error}"));
}

/// Applies only the module declarations of printed wiring (a model's: its
/// use cases are built where the application needs them).
fn declare_modules(root: &Path, printed: &str) {
    let mut declared = 0;
    for line in printed.lines() {
        if let Some((file, module)) = line.trim().split_once(" pub mod ") {
            declare(root, file.trim(), format!("pub mod {module}"));
            declared += 1;
        }
    }
    assert!(declared > 0, "{printed}");
}

/// Generates `database`'s application with both scaffolds and runs its
/// gates; returns the temporary directory and the application root.
fn scaffolded(database: &str) -> (tempfile::TempDir, PathBuf) {
    let forge = env!("CARGO_BIN_EXE_forge");
    let parent = tempfile::tempdir().unwrap_or_else(|error| panic!("{error}"));
    let name = format!("scaffold-{database}");
    let forge_path = workspace().join("crates/forge");
    let forge_path = forge_path.to_string_lossy();
    run(
        parent.path(),
        forge,
        &[
            "new",
            &name,
            "-d",
            database,
            "--skip-docker",
            "--skip-ci",
            "--skip-lockfile",
            "--forge-path",
            &forge_path,
        ],
        &[],
    );
    let root = parent.path().join(&name);
    for scaffold in [POST, EVENT, NOTE] {
        let mut arguments = vec!["generate", "scaffold"];
        arguments.extend_from_slice(scaffold);
        let dry = {
            let mut dry = arguments.clone();
            dry.push("--dry-run");
            run(&root, forge, &dry, &[])
        };
        let output = run(&root, forge, &arguments, &[]);
        let printed = String::from_utf8_lossy(&output.stdout).into_owned();
        assert_eq!(
            String::from_utf8_lossy(&dry.stdout).replace("would create", "create      "),
            printed.replace(
                &version_of(&printed),
                &version_of(&String::from_utf8_lossy(&dry.stdout))
            ),
            "--dry-run lists exactly what is created"
        );
        apply_wiring(&root, &printed);
    }

    run(&root, "cargo", &["generate-lockfile", "--quiet"], &[]);
    run(&root, "cargo", &["fmt", "--all", "--", "--check"], &[]);
    run(
        &root,
        "cargo",
        &[
            "clippy",
            "--all-targets",
            "--locked",
            "--quiet",
            "--",
            "-D",
            "warnings",
        ],
        &[],
    );
    (parent, root)
}

/// The migration version in a scaffold's output.
fn version_of(printed: &str) -> String {
    printed
        .split("migrations/")
        .nth(1)
        .and_then(|rest| rest.split('_').next())
        .unwrap_or_default()
        .to_owned()
}

fn test_output(root: &Path, include_ignored: bool) -> String {
    let mut arguments = vec!["test", "--all-targets", "--locked"];
    if include_ignored {
        arguments.extend(["--", "--include-ignored"]);
    }
    let output = run(root, "cargo", &arguments, &[]);
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
#[ignore = "slow: builds generated applications; run with --ignored"]
fn sqlite_application_with_scaffolds_passes_its_gates() {
    let (_parent, root) = scaffolded("sqlite3");
    let tests = test_output(&root, false);
    for test in [
        "create_show_edit_update_and_delete ... ok",
        "stale_versions_are_conflicts ... ok",
        "the_generated_deny_all_default_refuses_every_page ... ok",
        "sqlite_round_trip_locking_and_pages ... ok",
        "blank_and_too_long_text_is_rejected_with_every_violation ... ok",
        "rings_respect_dependency_direction ... ok",
        "sub_second_timestamps_are_refused_and_edit_forms_show_whole_seconds ... ok",
        "use_cases_check_the_grant_of_the_tenant_context ... ok",
        "an_active_member_with_the_grant_is_authorized_for_its_tenant ... ok",
        "a_missing_or_unknown_session_is_unauthenticated ... ok",
        "unsafe_methods_need_the_session_csrf_token ... ok",
        "another_tenant_or_a_malformed_tenant_is_forbidden ... ok",
        "a_suspended_membership_is_forbidden ... ok",
        "a_role_without_the_grant_is_forbidden ... ok",
        "changes_are_broadcast_to_open_lists ... ok",
        "streams_refuse_forged_unknown_and_unauthorized_subscriptions ... ok",
        "broadcasts_stay_in_their_tenant ... ok",
        "production_refuses_to_start_without_a_signing_key ... ok",
    ] {
        assert!(tests.contains(test), "{test}\n{tests}");
    }
}

/// `forge new --api` has no views, but a model needs none: it is generated,
/// wired like the printed instructions say, and passes the application's own
/// gates and its repository tests (the `forge::db::testing` dev-dependency
/// is part of an `--api` application with a database).
#[test]
#[ignore = "slow: builds generated applications; run with --ignored"]
fn api_application_with_a_model_passes_its_gates() {
    let forge = env!("CARGO_BIN_EXE_forge");
    let parent = tempfile::tempdir().unwrap_or_else(|error| panic!("{error}"));
    let forge_path = workspace().join("crates/forge");
    let forge_path = forge_path.to_string_lossy();
    run(
        parent.path(),
        forge,
        &[
            "new",
            "api-model",
            "--api",
            "-d",
            "sqlite3",
            "--skip-docker",
            "--skip-ci",
            "--skip-lockfile",
            "--forge-path",
            &forge_path,
        ],
        &[],
    );
    let root = parent.path().join("api-model");
    let output = run(
        &root,
        forge,
        &["generate", "model", "post", "title:string", "--global"],
        &[],
    );
    let printed = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(printed.contains("tests/posts_repository.rs"), "{printed}");
    assert!(
        !root.join("templates").exists(),
        "an API application has no templates"
    );
    declare_modules(&root, &printed);
    // A scaffold writes pages, which an API application does not have.
    let refused = run_unchecked(
        &root,
        forge,
        &["generate", "scaffold", "note", "body:text", "--global"],
        &[],
        None,
    );
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("--api"));

    run(&root, "cargo", &["generate-lockfile", "--quiet"], &[]);
    run(&root, "cargo", &["fmt", "--all", "--", "--check"], &[]);
    run(
        &root,
        "cargo",
        &[
            "clippy",
            "--all-targets",
            "--locked",
            "--quiet",
            "--",
            "-D",
            "warnings",
        ],
        &[],
    );
    let tests = test_output(&root, false);
    assert!(
        tests.contains("sqlite_round_trip_locking_and_pages ... ok"),
        "{tests}"
    );
}

#[test]
#[ignore = "slow: builds generated applications; run with --ignored"]
fn mysql_application_with_scaffolds_passes_its_gates() {
    let (_parent, root) = scaffolded("mysql");
    let server = std::env::var_os("FORGE_TEST_MYSQL_URL").is_some();
    let tests = test_output(&root, server);
    assert!(
        tests.contains("create_show_edit_update_and_delete ... ok"),
        "{tests}"
    );
}

#[test]
#[ignore = "slow: builds generated applications; run with --ignored"]
fn postgresql_application_with_scaffolds_passes_its_gates_and_migrations_revert() {
    let (_parent, root) = scaffolded("postgresql");
    let server = std::env::var_os("FORGE_TEST_DATABASE_URL").is_some();
    let tests = test_output(&root, server);
    assert!(
        tests.contains("create_show_edit_update_and_delete ... ok"),
        "{tests}"
    );
    if !server {
        eprintln!(
            "FORGE_TEST_DATABASE_URL is not set: PostgreSQL repository tests and migrations skipped"
        );
        return;
    }
    for test in [
        "postgres_round_trip_locking_and_pages ... ok",
        "postgres_row_level_security_isolates_tenants ... ok",
        "every_wrong_field_gets_the_same_answer ... ok",
        "two_factor_sign_in_takes_a_code_once_and_a_backup_code_once ... ok",
        "a_challenge_cookie_carried_to_another_browser_is_refused ... ok",
        "changing_the_factor_needs_the_password_and_a_current_code ... ok",
        "without_the_key_an_enrolled_account_cannot_sign_in ... ok",
        "two_factor_codes_and_challenges_work_once_and_fail_closed ... ok",
        "sign_in_steps_lock_the_account_before_any_mfa_row ... ok",
        "step_up_backup_codes_are_used_up_only_by_a_change_that_commits ... ok",
        "reset_requests_issue_nothing_when_no_link_can_be_delivered ... ok",
        "a_factor_change_during_a_second_factor_sign_in_ends_its_session ... ok",
        "rotating_a_session_waits_for_a_factor_change_instead_of_deadlocking ... ok",
        "two_reset_links_of_one_account_redeemed_together_do_not_deadlock ... ok",
        "a_reset_link_works_once_changes_the_password_and_revokes_sessions ... ok",
        "setup_creates_one_owner_who_can_sign_in_and_refuses_afterwards ... ok",
        "concurrent_setups_create_exactly_one_tenant ... ok",
        "reset_tokens_work_once_expire_and_revoke_every_session ... ok",
    ] {
        assert!(tests.contains(test), "{test}\n{tests}");
    }

    run(&root, "cargo", &["build", "--locked", "--quiet"], &[]);
    let binary = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("scaffold-apps/debug/scaffold-postgresql")
        .to_string_lossy()
        .into_owned();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap_or_else(|error| panic!("{error}"));
    runtime.block_on(async {
        let database = TestDatabase::<sqlx::Postgres>::create_empty()
            .await
            .unwrap_or_else(|error| panic!("test database: {error}"));
        let url = database.url().expose().to_owned();
        // The job queue is granted to the runtime role (`forge.runtime_role`
        // names it for the migration), so the workers below run as that
        // least-privileged role, not as the superuser that migrates: the
        // grants on the job tables are exercised end to end. The role is
        // cluster-wide, hence unique, and dropped at the end.
        let runtime_role = format!("forge_rt_{}", uuid_simple());
        let runtime_password = uuid_simple();
        let pool = database.database().pool().clone();
        let name = sqlx::query_scalar::<_, String>("SELECT current_database()")
            .fetch_one(&pool)
            .await
            .unwrap_or_else(|error| panic!("{error}"));
        for statement in [
            format!(
                "CREATE ROLE \"{runtime_role}\" LOGIN NOSUPERUSER NOBYPASSRLS NOCREATEDB \
                 NOCREATEROLE PASSWORD '{runtime_password}'"
            ),
            format!("ALTER DATABASE \"{name}\" SET forge.runtime_role = '{runtime_role}'"),
        ] {
            // Audited: names and password are this test's own hex identifiers.
            sqlx::query(sqlx::AssertSqlSafe(statement))
                .execute(&pool)
                .await
                .unwrap_or_else(|error| panic!("{error}"));
        }
        let runtime_url = with_credentials(&url, &runtime_role, &runtime_password);
        let migrate = |command: &str| {
            let output = run(
                &root,
                &binary,
                &[command],
                &[("FORGE_MIGRATION_DATABASE_URL", &url)],
            );
            String::from_utf8_lossy(&output.stderr).into_owned()
                + &String::from_utf8_lossy(&output.stdout)
        };
        let table = |name: &'static str| {
            let pool = database.database().pool().clone();
            async move {
                sqlx::query_scalar::<_, Option<String>>("SELECT to_regclass($1)::text")
                    .bind(format!("public.{name}"))
                    .fetch_one(&pool)
                    .await
                    .unwrap_or_else(|error| panic!("{error}"))
            }
        };

        migrate("migrate");
        assert_eq!(table("posts").await.as_deref(), Some("posts"));
        assert_eq!(table("events").await.as_deref(), Some("events"));
        assert_eq!(table("notes").await.as_deref(), Some("notes"));
        assert!(migrate("rollback").contains("create notes"));
        assert_eq!(table("notes").await, None);
        assert!(migrate("rollback").contains("create events"));
        assert_eq!(table("events").await, None);
        assert!(migrate("rollback").contains("create posts"));
        assert_eq!(table("posts").await, None);
        assert!(table("forge_principals").await.is_some());
        migrate("migrate");
        assert_eq!(table("events").await.as_deref(), Some("events"));

        // Seeds are idempotent: running them twice succeeds.
        for _ in 0..2 {
            let seeded = migrate("seed");
            assert!(seeded.contains("Seed data loaded."), "{seeded}");
        }
        run_the_job_workers(&root, &binary, &runtime_url);
        sign_in_use_a_tenant_scaffold_and_sign_out(&root, &binary, &url);
        database
            .cleanup()
            .await
            .unwrap_or_else(|error| panic!("cleanup: {error}"));
        drop_role(&runtime_role).await;
    });
}

/// `url` with its user and password replaced: `postgres://user:password@host/db`.
fn with_credentials(url: &str, user: &str, password: &str) -> String {
    let (scheme, rest) = url
        .split_once("://")
        .unwrap_or_else(|| panic!("not a URL: {url}"));
    let host = rest.rsplit_once('@').map_or(rest, |(_, host)| host);
    format!("{scheme}://{user}:{password}@{host}")
}

/// Drops a cluster-wide role the test created, once its database is gone.
async fn drop_role(role: &str) {
    use forge_db::sqlx::Connection as _;

    let admin = std::env::var("FORGE_TEST_DATABASE_URL")
        .unwrap_or_else(|_| panic!("FORGE_TEST_DATABASE_URL is set when roles are created"));
    let mut connection = forge_db::sqlx::PgConnection::connect(&admin)
        .await
        .unwrap_or_else(|error| panic!("admin connection: {error}"));
    // Audited: the role name is this test's own hex identifier.
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "DROP ROLE IF EXISTS \"{role}\""
    )))
    .execute(&mut connection)
    .await
    .unwrap_or_else(|error| panic!("drop role {role}: {error}"));
}

const OWNER_PASSWORD: &str = "correct horse battery";

/// The job queue `forge new` wires: `migrate` applied its schema, `jobs
/// status` reads it with the runtime role's `url`, and `jobs work` runs the workers
/// until `SIGTERM`, then drains and exits cleanly (`serve` runs them too,
/// during the browser session that follows).
fn run_the_job_workers(root: &Path, binary: &str, url: &str) {
    let runtime = [("FORGE_DATABASE_URL", url)];
    let status = String::from_utf8_lossy(&run(root, binary, &["jobs", "status"], &runtime).stdout)
        .into_owned();
    assert_eq!(status, "no jobs\n");

    let logs = tempfile::tempdir().unwrap_or_else(|error| panic!("{error}"));
    let log = logs.path().join("work.log");
    let file = |path: &Path| fs::File::create(path).unwrap_or_else(|error| panic!("{error}"));
    let mut worker = command(root, binary, &["jobs", "work"], &runtime)
        .stdin(Stdio::null())
        .stdout(file(&log))
        .stderr(file(&logs.path().join("work.err")))
        .spawn()
        .unwrap_or_else(|error| panic!("jobs work: {error}"));
    let read = || {
        fs::read_to_string(&log).unwrap_or_default()
            + &fs::read_to_string(logs.path().join("work.err")).unwrap_or_default()
    };
    let mut started = false;
    for _ in 0..300 {
        if read().contains("job workers started") {
            started = true;
            break;
        }
        if let Ok(Some(exit)) = worker.try_wait() {
            panic!("jobs work exited with {exit}:\n{}", read());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    if !started {
        let _ = worker.kill();
        panic!("jobs work did not start:\n{}", read());
    }
    // Long enough for several claim polls: with the grants of the runtime
    // role missing, each one would log `claiming jobs failed`.
    std::thread::sleep(Duration::from_millis(2500));
    let signalled = Command::new("kill")
        .args(["-TERM", &worker.id().to_string()])
        .status()
        .unwrap_or_else(|error| panic!("kill: {error}"));
    assert!(signalled.success());
    let mut exit = None;
    for _ in 0..300 {
        exit = worker.try_wait().unwrap_or_else(|error| panic!("{error}"));
        if exit.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let Some(exit) = exit else {
        let _ = worker.kill();
        panic!("jobs work did not stop on SIGTERM:\n{}", read());
    };
    let output = read();
    assert!(exit.success(), "{exit}:\n{output}");
    assert!(output.contains("job workers stopped"), "{output}");
    assert!(
        !output.contains("claiming jobs failed"),
        "the runtime role cannot claim jobs:\n{output}"
    );
}

/// A free loopback port.
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr())
        .map(|address| address.port())
        .unwrap_or_else(|error| panic!("free port: {error}"))
}

/// The application server; killed when dropped.
struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// One HTTP/1.1 response.
struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(header, _)| header.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// A browser: sends the cookies responses set, asks for HTML.
#[derive(Default)]
struct Browser {
    cookies: Vec<(String, String)>,
}

impl Browser {
    fn request(&mut self, port: u16, method: &str, path: &str, form: Option<&str>) -> Reply {
        let mut stream = TcpStream::connect(("127.0.0.1", port))
            .unwrap_or_else(|error| panic!("connect: {error}"));
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap_or_else(|error| panic!("{error}"));
        let cookies: Vec<String> = self
            .cookies
            .iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect();
        let body = form.unwrap_or_default();
        let mut request = format!(
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\
             Accept: text/html,application/xhtml+xml\r\nContent-Length: {}\r\n",
            body.len()
        );
        if !cookies.is_empty() {
            request.push_str(&format!("Cookie: {}\r\n", cookies.join("; ")));
        }
        if form.is_some() {
            request.push_str("Content-Type: application/x-www-form-urlencoded\r\n");
        }
        request.push_str("\r\n");
        request.push_str(body);
        stream
            .write_all(request.as_bytes())
            .unwrap_or_else(|error| panic!("send: {error}"));
        let mut raw = Vec::new();
        stream
            .read_to_end(&mut raw)
            .unwrap_or_else(|error| panic!("receive: {error}"));
        let raw = String::from_utf8_lossy(&raw).into_owned();
        let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((raw.as_str(), ""));
        let mut lines = head.lines();
        let status = lines
            .next()
            .and_then(|line| line.split(' ').nth(1))
            .and_then(|code| code.parse().ok())
            .unwrap_or_else(|| panic!("status line in {head}"));
        let headers: Vec<(String, String)> = lines
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (name.trim().to_owned(), value.trim().to_owned()))
            .collect();
        for (name, value) in &headers {
            if !name.eq_ignore_ascii_case("set-cookie") {
                continue;
            }
            let pair = value.split(';').next().unwrap_or_default();
            let Some((cookie, content)) = pair.split_once('=') else {
                continue;
            };
            self.cookies.retain(|(existing, _)| existing != cookie);
            if !content.is_empty() && !value.contains("Max-Age=0") {
                self.cookies.push((cookie.to_owned(), content.to_owned()));
            }
        }
        Reply {
            status,
            headers,
            body: body.to_owned(),
        }
    }

    fn cookie(&self, name: &str) -> Option<String> {
        self.cookies
            .iter()
            .find(|(cookie, _)| cookie == name)
            .map(|(_, value)| value.clone())
    }

    fn csrf(&self) -> String {
        self.cookie("__Host-forge_csrf")
            .unwrap_or_else(|| panic!("no CSRF cookie in {:?}", self.cookies))
    }

    /// Opens an event stream like `EventSource` and returns the connection
    /// once its headers and first event arrived, with what was read.
    fn open_stream(&self, port: u16, path: &str) -> (TcpStream, String) {
        let mut stream = TcpStream::connect(("127.0.0.1", port))
            .unwrap_or_else(|error| panic!("connect: {error}"));
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap_or_else(|error| panic!("{error}"));
        let cookies: Vec<String> = self
            .cookies
            .iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect();
        let request = format!(
            "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAccept: text/event-stream\r\n\
             Cookie: {}\r\n\r\n",
            cookies.join("; ")
        );
        stream
            .write_all(request.as_bytes())
            .unwrap_or_else(|error| panic!("send: {error}"));
        let mut received = String::new();
        read_until(&mut stream, &mut received, "\r\n\r\n");
        if received.starts_with("HTTP/1.1 200 ") {
            read_until(&mut stream, &mut received, ": connected");
        }
        (stream, received)
    }
}

/// Reads from `stream` until `marker` arrived; panics at the end of the
/// stream.
fn read_until(stream: &mut TcpStream, received: &mut String, marker: &str) {
    let mut buffer = [0_u8; 4096];
    while !received.contains(marker) {
        let read = stream
            .read(&mut buffer)
            .unwrap_or_else(|error| panic!("receive {marker:?}: {error}\n{received}"));
        assert!(read > 0, "the stream ended before {marker:?}:\n{received}");
        received.push_str(&String::from_utf8_lossy(&buffer[..read]));
    }
}

fn form_encode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' => {
                char::from(byte).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

/// `setup` creates the owner; a browser is sent to sign in by a tenant page,
/// signs in, lists and creates events, signs out, and is sent to sign in
/// again with its old session cookie.
fn sign_in_use_a_tenant_scaffold_and_sign_out(root: &Path, binary: &str, url: &str) {
    let migration = [("FORGE_MIGRATION_DATABASE_URL", url)];
    let setup = [
        "setup",
        "--tenant",
        "acme",
        "--email",
        "owner@example.com",
        "--organization",
        "Acme Inc",
        "--password-stdin",
    ];
    let created = run_unchecked(
        root,
        binary,
        &setup,
        &migration,
        Some(&format!("{OWNER_PASSWORD}\n")),
    );
    let printed = String::from_utf8_lossy(&created.stdout).into_owned();
    assert!(
        created.status.success(),
        "{printed}{}",
        String::from_utf8_lossy(&created.stderr)
    );
    assert!(!printed.contains(OWNER_PASSWORD));
    let tenant = printed
        .split("Created tenant acme (")
        .nth(1)
        .and_then(|rest| rest.split(')').next())
        .unwrap_or_else(|| panic!("tenant id in {printed}"))
        .to_owned();
    let again = run_unchecked(
        root,
        binary,
        &setup,
        &migration,
        Some(&format!("{OWNER_PASSWORD}\n")),
    );
    assert_eq!(again.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&again.stderr).contains("already set up"));

    let port = free_port();
    let bind = format!("127.0.0.1:{port}");
    let mut serve = command(
        root,
        binary,
        &["serve"],
        &[
            ("FORGE_DATABASE_URL", url),
            ("FORGE_BIND", &bind),
            ("FORGE_AUTH_MFA_KEY", MFA_KEY),
        ],
    );
    let _server = Server(
        serve
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap_or_else(|error| panic!("serve: {error}")),
    );
    let mut ready = false;
    for _ in 0..200 {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            ready = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(ready, "the server did not start on {bind}");

    let mut browser = Browser::default();
    let events = format!("/tenants/{tenant}/events");
    let sent = browser.request(port, "GET", &events, None);
    assert_eq!(sent.status, 303, "{}", sent.body);
    let sign_in = sent.header("location").unwrap_or_default().to_owned();
    assert_eq!(
        sign_in,
        format!("/session/new?return_to=%2Ftenants%2F{tenant}%2Fevents")
    );
    let page = browser.request(port, "GET", &sign_in, None);
    assert_eq!(page.status, 200);
    assert!(
        page.body
            .contains(&format!("name=\"return_to\" value=\"{events}\""))
    );

    let anonymous = browser.csrf();
    let form = format!(
        "csrf_token={anonymous}&tenant=acme&email=owner%40example.com&password={}&return_to={}",
        form_encode(OWNER_PASSWORD),
        form_encode(&events)
    );
    let signed_in = browser.request(port, "POST", "/session", Some(&form));
    assert_eq!(signed_in.status, 303, "{}", signed_in.body);
    assert_eq!(signed_in.header("location"), Some(events.as_str()));
    let session = browser.cookie("__Host-forge_session").unwrap_or_default();
    assert_eq!(session.len(), 64);
    assert_ne!(browser.csrf(), anonymous);

    let index = browser.request(port, "GET", &events, None);
    assert_eq!(index.status, 200, "{}", index.body);
    // --broadcast: the index subscribes; the signed-in owner's stream is
    // open (PostgreSQL LISTEN/NOTIFY fanout) and a signed-out client's is not.
    let source = index
        .body
        .split("<turbo-stream-source src=\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .unwrap_or_else(|| panic!("the index subscribes: {}", index.body))
        .replace("&amp;", "&");
    assert!(
        source.starts_with(&format!("/tenants/{tenant}/streams?stream=")),
        "{source}"
    );
    let (_, refused) = Browser::default().open_stream(port, &source);
    assert!(refused.starts_with("HTTP/1.1 401 "), "{refused}");
    let (mut live, mut received) = browser.open_stream(port, &source);
    assert!(received.starts_with("HTTP/1.1 200 "), "{received}");
    assert!(
        received
            .to_ascii_lowercase()
            .contains("content-type: text/event-stream"),
        "{received}"
    );
    let create = format!(
        "csrf_token={}&name=Launch&starts_at=2026-01-02T03%3A04%3A05&capacity=42&public=true&\
         venue=67e55044-10b1-426f-9247-bb680e5fe0c8",
        browser.csrf()
    );
    let created = browser.request(port, "POST", &events, Some(&create));
    assert_eq!(created.status, 303, "{}", created.body);
    let shown = created.header("location").unwrap_or_default().to_owned();
    assert!(shown.starts_with(&format!("{events}/")), "{shown}");
    let show = browser.request(port, "GET", &shown, None);
    assert_eq!(show.status, 200);
    assert!(show.body.contains("Launch"));
    read_until(&mut live, &mut received, "target=\"events\"><template>");
    assert!(received.contains("Launch"), "{received}");
    drop(live);

    let csrf = browser.csrf();
    let signed_out = browser.request(
        port,
        "POST",
        "/session",
        Some(&format!("csrf_token={csrf}&_method=delete")),
    );
    assert_eq!(signed_out.status, 303, "{}", signed_out.body);
    assert_eq!(signed_out.header("location"), Some("/session/new"));
    assert!(browser.cookie("__Host-forge_session").is_none());

    // The revoked session no longer opens the page.
    let mut stale = Browser::default();
    stale
        .cookies
        .push(("__Host-forge_session".to_owned(), session));
    let refused = stale.request(port, "GET", &events, None);
    assert_eq!(refused.status, 303);

    two_factor_enrol_sign_in_and_recover(port);
}

/// `FORGE_AUTH_MFA_KEY` of the served application: 32 bytes of 0x07.
const MFA_KEY: &str = "BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc";

/// The current TOTP period.
fn totp_period() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_else(|error| panic!("{error}"))
        .as_secs()
        / forge_auth::TOTP_PERIOD_SECS
}

fn totp(key: &str, offset: u64) -> String {
    forge_auth::TotpSecret::from_base32(key)
        .unwrap_or_else(|error| panic!("key: {error}"))
        .code(totp_period() + offset)
}

/// Signs the owner in with the password; returns the answer.
fn password_sign_in(browser: &mut Browser, port: u16) -> Reply {
    browser.request(port, "GET", "/session/new", None);
    let form = format!(
        "csrf_token={}&tenant=acme&email=owner%40example.com&password={}",
        browser.csrf(),
        form_encode(OWNER_PASSWORD)
    );
    browser.request(port, "POST", "/session", Some(&form))
}

/// The owner turns two-factor on (password, then a code of the new key),
/// signs in with a TOTP code, is refused the same code again, signs in with
/// a backup code once and is refused it the second time.
fn two_factor_enrol_sign_in_and_recover(port: u16) {
    let mut browser = Browser::default();
    let signed_in = password_sign_in(&mut browser, port);
    assert_eq!(signed_in.status, 303, "{}", signed_in.body);
    let settings = browser.request(port, "GET", "/mfa", None);
    assert_eq!(settings.status, 200, "{}", settings.body);
    assert!(settings.body.contains("Two-factor authentication is off."));

    let form = format!(
        "csrf_token={}&password={}",
        browser.csrf(),
        form_encode(OWNER_PASSWORD)
    );
    let setup = browser.request(port, "POST", "/mfa", Some(&form));
    assert_eq!(setup.status, 200, "{}", setup.body);
    assert_eq!(setup.header("cache-control"), Some("no-store"));
    let key = setup
        .body
        .split("<code id=\"mfa_key\">")
        .nth(1)
        .and_then(|rest| rest.split("</code>").next())
        .unwrap_or_else(|| panic!("the key in {}", setup.body))
        .replace(' ', "");
    assert!(setup.body.contains("otpauth://totp/"));
    let confirm = format!(
        "csrf_token={}&password={}&code={}",
        browser.csrf(),
        form_encode(OWNER_PASSWORD),
        totp(&key, 0)
    );
    let confirmed = browser.request(port, "POST", "/mfa/confirm", Some(&confirm));
    assert_eq!(confirmed.status, 200, "{}", confirmed.body);
    let backup_codes: Vec<String> = confirmed
        .body
        .split("<li><code>")
        .skip(1)
        .filter_map(|rest| rest.split("</code>").next())
        .map(str::to_owned)
        .collect();
    assert_eq!(backup_codes.len(), 10, "{}", confirmed.body);
    assert!(
        browser.cookie("__Host-forge_session").is_none(),
        "enrolling signs out everywhere"
    );

    let code_step = |browser: &mut Browser, code: &str| {
        let challenged = password_sign_in(browser, port);
        assert_eq!(challenged.status, 303, "{}", challenged.body);
        assert_eq!(challenged.header("location"), Some("/session/mfa"));
        assert!(browser.cookie("__Host-forge_session").is_none());
        let page = browser.request(port, "GET", "/session/mfa", None);
        assert_eq!(page.status, 200, "{}", page.body);
        let form = format!("csrf_token={}&code={}", browser.csrf(), form_encode(code));
        browser.request(port, "POST", "/session/mfa", Some(&form))
    };
    let sign_out = |browser: &mut Browser| {
        let form = format!("csrf_token={}&_method=delete", browser.csrf());
        let out = browser.request(port, "POST", "/session", Some(&form));
        assert_eq!(out.status, 303, "{}", out.body);
    };

    let code = totp(&key, 1);
    let mut first = Browser::default();
    let done = code_step(&mut first, &code);
    assert_eq!(done.status, 303, "{}", done.body);
    assert_eq!(done.header("location"), Some("/"));
    assert_eq!(
        first
            .cookie("__Host-forge_session")
            .map(|session| session.len()),
        Some(64)
    );
    sign_out(&mut first);

    let mut replay = Browser::default();
    let refused = code_step(&mut replay, &code);
    assert_eq!(
        refused.status, 422,
        "a used code is refused: {}",
        refused.body
    );
    assert!(replay.cookie("__Host-forge_session").is_none());

    let mut recovery = Browser::default();
    let recovered = code_step(&mut recovery, &backup_codes[0]);
    assert_eq!(recovered.status, 303, "{}", recovered.body);
    assert!(recovery.cookie("__Host-forge_session").is_some());
    let left = recovery.request(port, "GET", "/mfa", None);
    assert!(left.body.contains("9 backup codes left."), "{}", left.body);
    sign_out(&mut recovery);

    let mut reuse = Browser::default();
    let reused = code_step(&mut reuse, &backup_codes[0]);
    assert_eq!(reused.status, 422, "a used backup code is refused");
}

#[test]
#[ignore = "slow: builds generated applications; run with --ignored"]
fn postgresql_application_regains_its_sign_in_pages_with_generate_authentication() {
    let forge = env!("CARGO_BIN_EXE_forge");
    let parent = tempfile::tempdir().unwrap_or_else(|error| panic!("{error}"));
    let forge_path = workspace().join("crates/forge");
    let forge_path = forge_path.to_string_lossy();
    run(
        parent.path(),
        forge,
        &[
            "new",
            "regained",
            "--skip-docker",
            "--skip-ci",
            "--skip-lockfile",
            "--forge-path",
            &forge_path,
        ],
        &[],
    );
    let root = parent.path().join("regained");

    // forge new already has the pages: the generator refuses, names a few of
    // the files and counts the rest.
    let refused = run_unchecked(&root, forge, &["generate", "authentication"], &[], None);
    assert!(!refused.status.success());
    let message = String::from_utf8_lossy(&refused.stderr).into_owned();
    assert!(message.contains("refusing to overwrite"), "{message}");
    assert!(message.contains("... and "), "{message}");
    assert!(message.lines().count() <= 12, "{message}");
    // Peel the pages off a few files at a time, until the refusal is gone.
    let mut files: Vec<String> = Vec::new();
    let mut originals: Vec<(String, String)> = Vec::new();
    for _ in 0..10 {
        let attempt = run_unchecked(
            &root,
            forge,
            &["generate", "authentication", "--dry-run"],
            &[],
            None,
        );
        if attempt.status.success() {
            break;
        }
        let message = String::from_utf8_lossy(&attempt.stderr).into_owned();
        let listed: Vec<String> = message
            .lines()
            .filter_map(|line| line.strip_prefix("  "))
            .filter(|line| !line.starts_with("..."))
            .map(str::to_owned)
            .collect();
        assert!(!listed.is_empty(), "{message}");
        for file in listed {
            let path = root.join(&file);
            let content =
                fs::read_to_string(&path).unwrap_or_else(|error| panic!("{file}: {error}"));
            fs::remove_file(&path).unwrap_or_else(|error| panic!("{file}: {error}"));
            files.push(file.clone());
            originals.push((file, content));
        }
    }
    assert!(files.len() >= 10, "{files:?}");

    // Make it an application created before the generator: no files, no wiring.
    for (file, line) in [
        ("src/infrastructure/mod.rs", "pub mod accounts;\n"),
        ("src/adapters/http/mod.rs", "pub mod authentication;\n"),
        (
            "src/adapters/http/views/mod.rs",
            "pub mod authentication;\n",
        ),
        ("src/bootstrap/mod.rs", "pub mod setup;\n"),
        (
            "src/bootstrap/mod.rs",
            "        .command(\"setup\", setup::SUMMARY, setup::run)\n",
        ),
    ] {
        let path = root.join(file);
        let source = fs::read_to_string(&path).unwrap_or_else(|error| panic!("{error}"));
        assert!(source.contains(line), "{file}: {line}");
        fs::write(&path, source.replacen(line, "", 1)).unwrap_or_else(|error| panic!("{error}"));
    }
    let bootstrap = root.join("src/bootstrap/mod.rs");
    let source = fs::read_to_string(&bootstrap).unwrap_or_else(|error| panic!("{error}"));
    let start = source
        .find("    // Sign-in, sign-out")
        .unwrap_or_else(|| panic!("{source}"));
    let call = "crate::adapters::http::authentication::routes(composition, &site, ports)?;\n";
    let end = source[start..]
        .find(call)
        .map(|offset| start + offset + call.len())
        .unwrap_or_else(|| panic!("{source}"));
    fs::write(
        &bootstrap,
        format!("{}{}", &source[..start], &source[end..]),
    )
    .unwrap_or_else(|error| panic!("{error}"));

    let dry = run(
        &root,
        forge,
        &["generate", "authentication", "--dry-run"],
        &[],
    );
    assert!(originals.iter().all(|(file, _)| !root.join(file).exists()));
    let output = run(&root, forge, &["generate", "authentication"], &[]);
    let printed = String::from_utf8_lossy(&output.stdout).into_owned();
    assert_eq!(
        String::from_utf8_lossy(&dry.stdout).replace("would create", "create      "),
        printed
    );
    for (file, content) in &originals {
        let written = fs::read_to_string(root.join(file)).unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(&written, content, "{file} is byte-identical to forge new's");
    }
    apply_wiring(&root, &printed);

    run(&root, "cargo", &["generate-lockfile", "--quiet"], &[]);
    run(&root, "cargo", &["fmt", "--all", "--", "--check"], &[]);
    run(
        &root,
        "cargo",
        &[
            "clippy",
            "--all-targets",
            "--locked",
            "--quiet",
            "--",
            "-D",
            "warnings",
        ],
        &[],
    );
    let tests = test_output(&root, false);
    for test in [
        "signing_in_issues_a_fresh_session_and_csrf_token_and_returns_to_the_page ... ok",
        "reset_requests_answer_the_same_whether_or_not_the_account_exists ... ok",
        "setup_refuses_password_arguments_and_short_passwords_before_touching_a_database ... ok",
    ] {
        assert!(tests.contains(test), "{test}\n{tests}");
    }
}

/// Removes the lifecycle test's database, also when an assertion panics.
struct DropDatabase {
    admin: String,
    name: String,
}

impl Drop for DropDatabase {
    fn drop(&mut self) {
        let admin = self.admin.clone();
        let statement = format!("DROP DATABASE IF EXISTS \"{}\" WITH (FORCE)", self.name);
        let outcome = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| error.to_string())?;
            runtime.block_on(async {
                use forge_db::sqlx::{Connection as _, Executor as _};

                let mut connection = forge_db::sqlx::PgConnection::connect(&admin)
                    .await
                    .map_err(|error| error.to_string())?;
                connection
                    .execute(forge_db::sqlx::AssertSqlSafe(statement))
                    .await
                    .map(|_| ())
                    .map_err(|error| error.to_string())
            })
        })
        .join();
        if !matches!(outcome, Ok(Ok(()))) {
            eprintln!("could not drop the lifecycle test database {}", self.name);
        }
    }
}

/// Whether `table` exists in the public schema of database `name`.
fn table_exists(admin: &str, name: &str, table: &str) -> bool {
    let url = format!(
        "{}/{name}",
        admin.rsplit_once('/').map_or(admin, |(base, _)| base)
    );
    let table = table.to_owned();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap_or_else(|error| panic!("{error}"));
        runtime.block_on(async {
            use forge_db::sqlx::Connection as _;

            let mut connection = forge_db::sqlx::PgConnection::connect(&url)
                .await
                .unwrap_or_else(|error| panic!("connection: {error}"));
            forge_db::sqlx::query_scalar::<_, Option<String>>("SELECT to_regclass($1)::text")
                .bind(format!("public.{table}"))
                .fetch_one(&mut connection)
                .await
                .unwrap_or_else(|error| panic!("{error}"))
                .is_some()
        })
    })
    .join()
    .unwrap_or_else(|_| panic!("table_exists thread panicked"))
}

/// Whether `url`'s database exists, asked of the server directly.
fn database_exists(admin: &str, name: &str) -> bool {
    let admin = admin.to_owned();
    let name = name.to_owned();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap_or_else(|error| panic!("{error}"));
        runtime.block_on(async {
            use forge_db::sqlx::Connection as _;

            let mut connection = forge_db::sqlx::PgConnection::connect(&admin)
                .await
                .unwrap_or_else(|error| panic!("admin connection: {error}"));
            forge_db::sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (SELECT 1 FROM pg_database WHERE datname = $1)",
            )
            .bind(name)
            .fetch_one(&mut connection)
            .await
            .unwrap_or_else(|error| panic!("{error}"))
        })
    })
    .join()
    .unwrap_or_else(|_| panic!("database_exists thread panicked"))
}

#[test]
#[ignore = "slow: builds generated applications; run with --ignored"]
fn postgresql_application_runs_its_lifecycle_through_the_forge_cli() {
    let Ok(admin) = std::env::var("FORGE_TEST_DATABASE_URL") else {
        eprintln!("FORGE_TEST_DATABASE_URL is not set: the forge CLI lifecycle test is skipped");
        return;
    };
    let forge = env!("CARGO_BIN_EXE_forge");
    let parent = tempfile::tempdir().unwrap_or_else(|error| panic!("{error}"));
    let forge_path = workspace().join("crates/forge");
    let forge_path = forge_path.to_string_lossy();
    run(
        parent.path(),
        forge,
        &[
            "new",
            "lifecycle",
            "--skip-docker",
            "--skip-ci",
            "--skip-lockfile",
            "--forge-path",
            &forge_path,
        ],
        &[],
    );
    let root = parent.path().join("lifecycle");
    run(&root, "cargo", &["generate-lockfile", "--quiet"], &[]);

    // A database that does not exist yet: the CLI creates it. The server and
    // credentials are the test server's; only the name differs.
    let name = format!("forge_lifecycle_{}", uuid_simple());
    let base = admin
        .rsplit_once('/')
        .map(|(base, _)| base.to_owned())
        .unwrap_or_else(|| panic!("FORGE_TEST_DATABASE_URL has a database name"));
    let url = format!("{base}/{name}");
    let _cleanup = DropDatabase {
        admin: admin.clone(),
        name: name.clone(),
    };
    // Less debug information keeps the build of the application small.
    let environment = [
        ("FORGE_DATABASE_URL", url.as_str()),
        ("FORGE_MIGRATION_DATABASE_URL", url.as_str()),
        ("CARGO_PROFILE_DEV_DEBUG", "0"),
    ];
    let output = |output: &Output| {
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    };

    // doctor sees the server, never prints the credentials.
    let doctor = run(&root, forge, &["doctor"], &environment);
    let text = output(&doctor);
    assert!(text.contains("configuration: ok"), "{text}");
    assert!(
        text.contains(&format!("database: postgres database \"{name}\" on ")),
        "{text}"
    );
    assert!(text.contains("the server accepts connections"), "{text}");
    assert!(
        !text.contains(&admin) && !text.contains("postgres:postgres"),
        "{text}"
    );

    // Nothing to inspect yet: the database is missing.
    let missing = run_unchecked(&root, forge, &["db", "status"], &environment, None);
    assert!(!missing.status.success(), "{}", output(&missing));

    // An empty database: status reads without creating the migrations table.
    run(&root, forge, &["db", "create"], &environment);
    let empty = run(&root, forge, &["db", "status"], &environment);
    let listing = String::from_utf8_lossy(&empty.stdout).into_owned();
    assert!(listing.contains("no migrations applied"), "{listing}");
    assert!(listing.contains("pending   1_forge_audit"), "{listing}");
    assert!(
        !table_exists(&admin, &name, "_sqlx_migrations"),
        "status must not create the bookkeeping table"
    );
    run(&root, forge, &["db", "drop"], &environment);

    // prepare = create + migrate + seed for a missing database.
    let prepared = run(&root, forge, &["db", "prepare"], &environment);
    let text = output(&prepared);
    assert!(text.contains("created postgres database"), "{text}");
    assert!(text.contains("Seed data loaded."), "{text}");
    assert!(database_exists(&admin, &name));
    let status = run(&root, forge, &["db", "status"], &environment);
    let listing = String::from_utf8_lossy(&status.stdout).into_owned();
    assert!(listing.contains("applied   1_forge_audit"), "{listing}");
    assert!(
        listing.contains("applied   3_forge_authentication"),
        "{listing}"
    );
    assert!(!listing.contains("pending"), "{listing}");

    // Prepared again, the database exists: it only migrates.
    let again = run(&root, forge, &["db", "prepare"], &environment);
    let text = output(&again);
    assert!(!text.contains("created postgres database"), "{text}");
    assert!(text.contains("no pending migrations"), "{text}");

    // A new migration shows as pending, applies, and rolls back.
    run(
        &root,
        forge,
        &["generate", "migration", "create customers"],
        &[],
    );
    let customers = std::fs::read_dir(root.join("migrations"))
        .unwrap_or_else(|error| panic!("{error}"))
        .map(|entry| entry.unwrap_or_else(|error| panic!("{error}")).path())
        .filter(|path| path.to_string_lossy().contains("_create_customers."))
        .collect::<Vec<_>>();
    for path in &customers {
        let sql = if path.to_string_lossy().ends_with(".up.sql") {
            "CREATE TABLE customers (id UUID PRIMARY KEY);\n"
        } else {
            "DROP TABLE customers;\n"
        };
        std::fs::write(path, sql).unwrap_or_else(|error| panic!("{error}"));
    }
    // The migrations are embedded at compile time: forge rebuilds the application.
    let pending = run(&root, forge, &["migrate", "status"], &environment);
    let listing = String::from_utf8_lossy(&pending.stdout).into_owned();
    assert!(
        listing.contains("pending   ") && listing.contains("_create_customers"),
        "{listing}"
    );
    let migrated = run(&root, forge, &["migrate"], &environment);
    assert!(
        output(&migrated).contains("applied migration"),
        "{}",
        output(&migrated)
    );
    let listing = run(&root, forge, &["db", "status"], &environment);
    assert!(!String::from_utf8_lossy(&listing.stdout).contains("pending"));
    let rolled = run(&root, forge, &["rollback"], &environment);
    assert!(
        output(&rolled).contains("reverted migration"),
        "{}",
        output(&rolled)
    );
    let listing = run(&root, forge, &["db", "status"], &environment);
    assert!(String::from_utf8_lossy(&listing.stdout).contains("pending"));
    run(&root, forge, &["migrate"], &environment);

    // setup creates the owner (the password only on standard input), once.
    let setup = [
        "setup",
        "--tenant",
        "acme",
        "--email",
        "owner@example.com",
        "--organization",
        "Acme Inc",
        "--password-stdin",
    ];
    let created = run_unchecked(
        &root,
        forge,
        &setup,
        &environment,
        Some(&format!("{OWNER_PASSWORD}\n")),
    );
    let text = output(&created);
    assert!(created.status.success(), "{text}");
    assert!(text.contains("Created tenant acme"), "{text}");
    assert!(!text.contains(OWNER_PASSWORD));
    let twice = run_unchecked(
        &root,
        forge,
        &setup,
        &environment,
        Some(&format!("{OWNER_PASSWORD}\n")),
    );
    assert!(!twice.status.success(), "{}", output(&twice));

    // Production refuses before touching anything.
    let mut production = environment.to_vec();
    production.push(("FORGE_ENV", "production"));
    for arguments in [&["db", "drop"][..], &["db", "reset"], &["db", "rollback"]] {
        let refused = run_unchecked(&root, forge, arguments, &production, None);
        assert_eq!(refused.status.code(), Some(1), "{arguments:?}");
        let text = output(&refused);
        assert!(
            text.contains("refusing to ") && text.contains("FORGE_ENV is production"),
            "{arguments:?}: {text}"
        );
    }
    assert!(
        database_exists(&admin, &name),
        "the database is still there"
    );
    run(&root, forge, &["db", "status"], &environment);

    // reset drops and rebuilds: the owner is gone, so setup works again.
    let reset = run(&root, forge, &["db", "reset"], &environment);
    let text = output(&reset);
    assert!(text.contains("dropped postgres database"), "{text}");
    assert!(text.contains("created postgres database"), "{text}");
    let recreated = run_unchecked(
        &root,
        forge,
        &setup,
        &environment,
        Some(&format!("{OWNER_PASSWORD}\n")),
    );
    assert!(recreated.status.success(), "{}", output(&recreated));

    // drop removes the database; dropping again says there is none.
    let dropped = run(&root, forge, &["db", "drop"], &environment);
    assert!(output(&dropped).contains("dropped postgres database"));
    assert!(!database_exists(&admin, &name));
    let nothing = run(&root, forge, &["db", "drop"], &environment);
    assert!(
        output(&nothing).contains("does not exist"),
        "{}",
        output(&nothing)
    );
}

/// A UUIDv7 without hyphens, for unique database names.
fn uuid_simple() -> String {
    let id = run(Path::new("."), env!("CARGO_BIN_EXE_forge"), &["id"], &[]);
    String::from_utf8_lossy(&id.stdout).trim().replace('-', "")
}

/// Every file below `root` except build output and the lockfile, relative
/// path to content.
fn source_tree(root: &Path) -> std::collections::BTreeMap<String, Vec<u8>> {
    fn visit(
        root: &Path,
        directory: &Path,
        files: &mut std::collections::BTreeMap<String, Vec<u8>>,
    ) {
        let entries = fs::read_dir(directory).unwrap_or_else(|error| panic!("{error}"));
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            let relative = path
                .strip_prefix(root)
                .unwrap_or_else(|error| panic!("{error}"))
                .to_string_lossy()
                .replace('\\', "/");
            if relative == "target" || relative == "Cargo.lock" || relative == ".git" {
                continue;
            }
            if path.is_dir() {
                visit(root, &path, files);
            } else {
                let content = fs::read(&path).unwrap_or_else(|error| panic!("{error}"));
                files.insert(relative, content);
            }
        }
    }
    let mut files = std::collections::BTreeMap::new();
    visit(root, root, &mut files);
    files
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// `forge routes` end to end: a PostgreSQL application with a public HTML
/// controller, a JSON controller and a scaffold lists every route (HTML,
/// `/v1` JSON and Forge's own), sorted like Forge-Go's table, each naming
/// the source line that registered it. The application is composed, not
/// served, so no database has to run.
#[test]
#[ignore = "slow: builds generated applications; run with --ignored"]
fn postgresql_application_lists_every_route_with_forge_routes() {
    let forge = env!("CARGO_BIN_EXE_forge");
    let parent = tempfile::tempdir().unwrap_or_else(|error| panic!("{error}"));
    let forge_path = workspace().join("crates/forge");
    let forge_path = forge_path.to_string_lossy();
    run(
        parent.path(),
        forge,
        &[
            "new",
            "routes-postgresql",
            "--skip-docker",
            "--skip-ci",
            "--skip-lockfile",
            "--forge-path",
            &forge_path,
        ],
        &[],
    );
    let root = parent.path().join("routes-postgresql");
    for (generator, arguments) in [
        (
            "controller",
            &["pages", "index", "about_us", "--public"][..],
        ),
        ("controller", &["reports", "monthly", "--api"]),
        ("scaffold", &["article", "title:string", "--tenant-scoped"]),
    ] {
        let mut command = vec!["generate", generator];
        command.extend_from_slice(arguments);
        let printed = stdout(&run(&root, forge, &command, &[]));
        apply_wiring(&root, &printed);
    }
    run(&root, "cargo", &["generate-lockfile", "--quiet"], &[]);

    // The application composes with the configuration `serve` needs.
    let refused = run_unchecked(&root, forge, &["routes"], &[], None);
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("FORGE_DATABASE_URL"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
    let environment = [(
        "FORGE_DATABASE_URL",
        "postgres://app:secret@127.0.0.1:1/routes",
    )];
    let output = run(&root, forge, &["routes"], &environment);
    let table = stdout(&output);
    assert!(!table.contains("secret"), "{table}");

    let mut lines = table.lines();
    let header: Vec<&str> = lines
        .next()
        .unwrap_or_default()
        .split_whitespace()
        .collect();
    assert_eq!(header, ["METHOD", "PATH", "ACTION", "SOURCE"], "{table}");
    let rows: Vec<Vec<&str>> = lines
        .by_ref()
        .take_while(|line| !line.is_empty())
        .map(|line| line.split_whitespace().collect())
        .collect();
    assert!(
        rows.iter().all(|row| row.len() >= 4 && row[2] == "-"),
        "{table}"
    );
    let listed =
        |method: &str, path: &str| rows.iter().any(|row| row[0] == method && row[1] == path);
    for (method, path) in [
        // Forge's own routes.
        ("GET", "/health"),
        ("GET", "/health/live"),
        ("GET", "/health/ready"),
        ("GET", "/openapi.json"),
        // The welcome page and the sign-in pages.
        ("GET", "/"),
        ("GET", "/session/new"),
        ("POST", "/session"),
        // The public HTML controller.
        ("GET", "/pages"),
        ("GET", "/pages/about_us"),
        // The JSON routes, which Forge-Go's source scan does not see.
        ("GET", "/v1/status"),
        ("GET", "/v1/reports/monthly"),
    ] {
        assert!(
            listed(method, path),
            "{method} {path} is not listed:\n{table}"
        );
    }
    assert!(
        listed("GET", "/tenants/{tenant_id}/articles"),
        "the scaffold's pages are listed:\n{table}"
    );
    // Sorted by path, then method.
    let order: Vec<(&str, &str)> = rows.iter().map(|row| (row[1], row[0])).collect();
    let mut sorted = order.clone();
    sorted.sort_unstable();
    assert_eq!(order, sorted, "{table}");
    // Forge's routes say so; the application's name the line that registered them.
    for row in &rows {
        let source = row[3..].join(" ");
        if source.starts_with("forge (built in") {
            continue;
        }
        let (file, line) = source
            .rsplit_once(':')
            .unwrap_or_else(|| panic!("{source} is not file:line"));
        let text =
            fs::read_to_string(root.join(file)).unwrap_or_else(|error| panic!("{file}: {error}"));
        let line: usize = line
            .parse()
            .unwrap_or_else(|error| panic!("{source}: {error}"));
        assert!(
            text.lines()
                .nth(line - 1)
                .is_some_and(|code| code.contains("route(")),
            "{source} does not register a route:\n{table}"
        );
    }
    let footer: String = lines.collect::<Vec<_>>().join("\n");
    assert!(footer.starts_with("Scope: every route"), "{table}");

    // Arguments are a usage error, like the other lifecycle commands.
    let extra = run_unchecked(&root, forge, &["routes", "--json"], &environment, None);
    assert_eq!(extra.status.code(), Some(2));
}

/// `forge generate model|controller|mailer|job` and `forge destroy` end to
/// end (ADR 0038): a PostgreSQL application gets a model, a tenant-scoped
/// scaffold, a controller that reuses the model's permission, a public and
/// a JSON controller, a mailer and a queue job; the printed wiring is
/// applied and the application passes `cargo fmt --check`, Clippy with
/// warnings denied and `cargo test` (with FORGE_TEST_DATABASE_URL, the
/// repository tests too). Then everything is destroyed, the wiring taken
/// out, and the application is the one `forge new` wrote again (plus the
/// shared access and jobs modules, which destroy never removes) and still
/// builds.
#[test]
#[ignore = "slow: builds generated applications; run with --ignored"]
fn postgresql_application_generates_and_destroys_every_generator() {
    let forge = env!("CARGO_BIN_EXE_forge");
    let parent = tempfile::tempdir().unwrap_or_else(|error| panic!("{error}"));
    let forge_path = workspace().join("crates/forge");
    let forge_path = forge_path.to_string_lossy();
    run(
        parent.path(),
        forge,
        &[
            "new",
            "generators-postgresql",
            "--skip-docker",
            "--skip-ci",
            "--skip-lockfile",
            "--forge-path",
            &forge_path,
        ],
        &[],
    );
    let root = parent.path().join("generators-postgresql");
    let original = source_tree(&root);

    const MODEL: &[&str] = &["book", "title:string", "pages:integer?", "--global"];
    const SCAFFOLD: &[&str] = &["article", "title:string", "--tenant-scoped"];
    const BOOKS: &[&str] = &["books", "shelf"];
    const PAGES: &[&str] = &["pages", "index", "about_us", "--public"];
    const REPORTS: &[&str] = &["reports", "monthly", "--api"];
    const MAILER: &[&str] = &["user", "welcome:name,url", "password_reset"];
    const JOB: &[&str] = &["send_welcome", "user_id:uuid", "note:string?", "--queue"];
    let generations: [(&str, &[&str]); 7] = [
        ("model", MODEL),
        ("scaffold", SCAFFOLD),
        ("controller", BOOKS),
        ("controller", PAGES),
        ("controller", REPORTS),
        ("mailer", MAILER),
        ("job", JOB),
    ];
    for (generator, arguments) in generations {
        let mut command = vec!["generate", generator];
        command.extend_from_slice(arguments);
        let mut dry = command.clone();
        dry.push("--dry-run");
        let before = source_tree(&root);
        let planned = stdout(&run(&root, forge, &dry, &[]));
        assert_eq!(before, source_tree(&root), "--dry-run writes nothing");
        let printed = stdout(&run(&root, forge, &command, &[]));
        assert_eq!(
            planned.replace("would create", "create      "),
            printed.replace(&version_of(&printed), &version_of(&planned)),
            "--dry-run lists exactly what {generator} creates"
        );
        // Generating again refuses and writes nothing.
        let again = run_unchecked(&root, forge, &command, &[], None);
        assert!(!again.status.success(), "{generator} {arguments:?} twice");
        if generator == "model" {
            declare_modules(&root, &printed);
        } else {
            apply_wiring(&root, &printed);
        }
        if generator == "controller" && arguments == BOOKS {
            assert!(printed.contains("already declares"), "{printed}");
        }
    }
    // `forge new` enabled the job feature on both `forge` dependencies.
    assert_eq!(
        fs::read_to_string(root.join("Cargo.toml"))
            .unwrap_or_default()
            .matches("\"jobs\"")
            .count(),
        2
    );

    // A model whose permission a controller declares is refused (one owner).
    let refused = run_unchecked(
        &root,
        forge,
        &["generate", "model", "report", "title:string", "--global"],
        &[],
        None,
    );
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("reports:read"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );

    run(&root, "cargo", &["generate-lockfile", "--quiet"], &[]);
    run(&root, "cargo", &["fmt", "--all", "--", "--check"], &[]);
    let clippy = [
        "clippy",
        "--all-targets",
        "--locked",
        "--quiet",
        "--",
        "-D",
        "warnings",
    ];
    run(&root, "cargo", &clippy, &[]);
    let server = std::env::var_os("FORGE_TEST_DATABASE_URL").is_some();
    let tests = test_output(&root, server);
    for test in [
        "every_action_answers ... ok",
        "only_get_is_routed ... ok",
        "every_route_asks_for_the_read_permission_once ... ok",
        "the_generated_deny_all_default_refuses_every_action ... ok",
        "welcome_is_delivered_with_its_subject_and_both_parts ... ok",
        "welcome_escapes_its_data_in_the_html_part ... ok",
        "password_reset_is_delivered_with_its_subject_and_both_parts ... ok",
        "previews_render_and_exist_only_in_development ... ok",
        "a_failing_sender_records_nothing_and_an_unconfigured_mailer_fails ... ok",
        "the_handler_registers_once ... ok",
        "the_placeholder_fails_permanently ... ok",
        "create_show_edit_update_and_delete ... ok",
    ] {
        assert!(tests.contains(test), "{test}\n{tests}");
    }
    if server {
        assert!(
            tests.contains("postgres_round_trip_locking_and_pages ... ok"),
            "{tests}"
        );
    }

    // The model's files belong to its permission's controller until it goes.
    let model = |extra: &[&'static str]| {
        let mut command = vec!["destroy", "model"];
        command.extend_from_slice(MODEL);
        command.extend_from_slice(extra);
        command
    };
    let blocked = run_unchecked(&root, forge, &model(&[]), &[], None);
    assert!(!blocked.status.success());
    assert!(String::from_utf8_lossy(&blocked.stderr).contains("books_controller.rs"));

    // An edited file is never removed without --force.
    let page = root.join("templates/pages/about_us.html");
    let edited = fs::read_to_string(&page).unwrap_or_default();
    fs::write(&page, format!("{edited}<p>edited</p>\n")).unwrap_or_else(|error| panic!("{error}"));
    let mut pages = vec!["destroy", "controller"];
    pages.extend_from_slice(PAGES);
    let refused = run_unchecked(&root, forge, &pages, &[], None);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("templates/pages/about_us.html"));
    assert!(root.join("src/adapters/http/pages_controller.rs").exists());
    fs::write(&page, edited).unwrap_or_else(|error| panic!("{error}"));

    let destroy = |generator: &'static str, arguments: &[&'static str], extra: &[&'static str]| {
        let mut command = vec!["destroy", generator];
        command.extend_from_slice(arguments);
        command.extend_from_slice(extra);
        stdout(&run(&root, forge, &command, &[]))
    };
    let dry = destroy("controller", BOOKS, &["--dry-run"]);
    assert!(
        dry.contains("would remove src/adapters/http/books_controller.rs"),
        "{dry}"
    );
    assert!(root.join("src/adapters/http/books_controller.rs").exists());
    destroy("controller", BOOKS, &[]);
    let kept = stdout(&run(&root, forge, &model(&[]), &[]));
    assert!(kept.contains("keep         migrations/"), "{kept}");
    let leftover = run_unchecked(&root, forge, &model(&[]), &[], None);
    assert!(String::from_utf8_lossy(&leftover.stderr).contains("--migrations"));
    run(&root, forge, &model(&["--migrations"]), &[]);
    destroy("scaffold", SCAFFOLD, &["--migrations"]);
    destroy("controller", PAGES, &[]);
    destroy("controller", REPORTS, &[]);
    destroy("mailer", MAILER, &[]);
    destroy("job", JOB, &[]);

    // Take the wiring out again: the files Forge never edits.
    for (path, content) in &original {
        let current = fs::read(root.join(path)).unwrap_or_default();
        if &current != content {
            fs::write(root.join(path), content).unwrap_or_else(|error| panic!("{error}"));
        }
    }
    let mut restored = source_tree(&root);
    for shared in ["src/adapters/http/access.rs", "src/adapters/jobs/mod.rs"] {
        assert!(
            restored.remove(shared).is_some(),
            "{shared} is shared and kept"
        );
    }
    let drift: Vec<&String> = original
        .keys()
        .chain(restored.keys())
        .filter(|path| original.get(*path) != restored.get(*path))
        .collect();
    assert!(drift.is_empty(), "destroy left or removed {drift:?}");
    assert!(!root.join("templates/mailers").exists());
    assert!(
        !root
            .join("src/adapters/http/views/pages_controller.rs")
            .exists()
    );

    run(&root, "cargo", &clippy, &[]);
    run(&root, "cargo", &["build", "--locked", "--quiet"], &[]);
}
