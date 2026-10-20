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
/// end of `fn compose`.
fn apply_wiring(root: &Path, printed: &str) {
    let mut declarations = Vec::new();
    let mut statements = Vec::new();
    let mut commands = Vec::new();
    let mut section = 0;
    for line in printed.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("1. Declare") {
            section = 1;
        } else if trimmed.starts_with("2. In src/bootstrap/mod.rs") {
            section = 2;
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
        let path = root.join(&file);
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
    let end = source
        .rfind("    Ok(())\n}\n")
        .unwrap_or_else(|| panic!("compose ends with Ok(()):\n{source}"));
    let wired = format!("{}{statements}\n{}", &source[..end], &source[end..]);
    fs::write(&bootstrap, wired).unwrap_or_else(|error| panic!("{error}"));
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
        sign_in_use_a_tenant_scaffold_and_sign_out(&root, &binary, &url);
        database
            .cleanup()
            .await
            .unwrap_or_else(|error| panic!("cleanup: {error}"));
    });
}

const OWNER_PASSWORD: &str = "correct horse battery";

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
        &[("FORGE_DATABASE_URL", url), ("FORGE_BIND", &bind)],
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

    // forge new already has the pages: the generator refuses and lists them.
    let refused = run_unchecked(&root, forge, &["generate", "authentication"], &[], None);
    assert!(!refused.status.success());
    let message = String::from_utf8_lossy(&refused.stderr).into_owned();
    assert!(message.contains("refusing to overwrite"), "{message}");
    let files: Vec<String> = message
        .lines()
        .filter_map(|line| line.strip_prefix("  "))
        .map(str::to_owned)
        .collect();
    assert!(files.len() >= 10, "{message}");

    // Make it an application created before the generator: no files, no wiring.
    let originals: Vec<(String, String)> = files
        .iter()
        .map(|file| {
            let path = root.join(file);
            let content =
                fs::read_to_string(&path).unwrap_or_else(|error| panic!("{file}: {error}"));
            fs::remove_file(&path).unwrap_or_else(|error| panic!("{file}: {error}"));
            (file.clone(), content)
        })
        .collect();
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
