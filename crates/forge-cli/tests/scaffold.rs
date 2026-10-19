//! `forge generate scaffold` end to end: generate a PostgreSQL, a SQLite and
//! a MySQL application, scaffold a global and a tenant-scoped resource in
//! each, apply the printed wiring exactly as printed, and run the
//! application's own gates: `cargo fmt --check`, Clippy with warnings denied
//! and `cargo test`.
//!
//! With `FORGE_TEST_DATABASE_URL` the PostgreSQL application also runs its
//! ignored repository and row-level-security tests and applies and reverts
//! the scaffold migrations with its own `migrate`/`rollback` commands (with
//! `FORGE_TEST_MYSQL_URL` the MySQL repository tests run too).
//!
//! These tests build whole applications and resolve dependencies with
//! `cargo generate-lockfile`, so they are ignored by default:
//!
//! ```sh
//! cargo test -p forge-cli --test scaffold -- --ignored
//! ```

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

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
];

fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn run(
    directory: &Path,
    program: &str,
    arguments: &[&str],
    environment: &[(&str, &str)],
) -> Output {
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
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("{program} {arguments:?} did not start: {error}"));
    assert!(
        output.status.success(),
        "{program} {arguments:?} failed in {}:\n{}\n{}",
        directory.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

/// Applies the printed wiring like a careful developer: each `pub mod`
/// line goes into the sorted block of its file, the statements go at the
/// end of `fn compose`.
fn apply_wiring(root: &Path, printed: &str) {
    let mut declarations = Vec::new();
    let mut statements = Vec::new();
    let mut section = 0;
    for line in printed.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("1. Declare") {
            section = 1;
        } else if trimmed.starts_with("2. In src/bootstrap/mod.rs") {
            section = 2;
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

    // The grant example printed for PostgreSQL tenant resources, applied
    // too, so it is proven to compile.
    let mut grant = Vec::new();
    let mut in_example = false;
    for line in printed.lines() {
        if line.contains("make `policy` mutable and add, for example:") {
            in_example = true;
        } else if in_example && line.starts_with("    ") {
            grant.push(line.to_owned());
        } else if in_example && !grant.is_empty() {
            break;
        }
    }
    let policy = "    let policy = forge::security::RbacPolicy::new();\n";
    let statements = statements.join("\n").replace(
        policy,
        &format!(
            "    let mut policy = forge::security::RbacPolicy::new();\n{}\n",
            grant.join("\n")
        ),
    );

    let bootstrap = root.join("src/bootstrap/mod.rs");
    let source = fs::read_to_string(&bootstrap).unwrap_or_else(|error| panic!("{error}"));
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
    for scaffold in [POST, EVENT] {
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
        assert!(migrate("rollback").contains("create events"));
        assert_eq!(table("events").await, None);
        assert!(migrate("rollback").contains("create posts"));
        assert_eq!(table("posts").await, None);
        assert!(table("forge_principals").await.is_some());
        migrate("migrate");
        assert_eq!(table("events").await.as_deref(), Some("events"));
        database
            .cleanup()
            .await
            .unwrap_or_else(|error| panic!("cleanup: {error}"));
    });
}
