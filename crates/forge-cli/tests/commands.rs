//! The generators, `forge destroy`, the cargo wrappers and `forge new`'s
//! lockfile step through the real `forge` executable. Nothing here builds a
//! generated application: `tests/scaffold.rs` does.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const FORGE: &str = env!("CARGO_BIN_EXE_forge");

/// A `forge` command without the harness's `FORGE_TEST_*` variables, which
/// the commands that read the application's configuration refuse.
fn forge_command(current_dir: &Path) -> Command {
    let mut command = Command::new(FORGE);
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("FORGE_TEST_") {
            command.env_remove(name);
        }
    }
    command.current_dir(current_dir);
    command
}

fn forge(current_dir: &Path, arguments: &[&str]) -> Output {
    forge_command(current_dir)
        .args(arguments)
        .output()
        .expect("forge executable should start")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// A freshly generated PostgreSQL application with views, named `shop`.
fn application() -> (tempfile::TempDir, PathBuf) {
    let parent = tempfile::tempdir().expect("temporary directory");
    let output = forge(
        parent.path(),
        &["new", "shop", "--skip-lockfile", "--forge-path", "../forge"],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let root = parent.path().join("shop");
    (parent, root)
}

fn succeeds(root: &Path, arguments: &[&str]) -> String {
    let output = forge(root, arguments);
    assert!(
        output.status.success(),
        "{arguments:?}: {}",
        stderr(&output)
    );
    stdout(&output)
}

fn fails(root: &Path, arguments: &[&str]) -> String {
    let output = forge(root, arguments);
    assert_eq!(output.status.code(), Some(1), "{arguments:?}: {output:?}");
    stderr(&output)
}

#[test]
fn every_generator_lists_its_files_and_prints_its_wiring() {
    let (_parent, root) = application();

    let scaffold = succeeds(
        &root,
        &[
            "generate",
            "scaffold",
            "post",
            "title:string",
            "body:text",
            "--global",
        ],
    );
    assert!(
        scaffold.contains("  create       src/adapters/http/posts.rs\n"),
        "{scaffold}"
    );
    assert!(scaffold.contains("Forge did not edit any existing file. Wire the resource by hand"));
    assert!(root.join("templates/posts/index.html").is_file());
    assert!(root.join("src/adapters/http/access.rs").is_file());

    // The access policy is shared: the next resource reuses it, and a
    // broadcasting one reuses it and writes the streams module.
    let second = succeeds(
        &root,
        &[
            "generate",
            "scaffold",
            "note",
            "body:text",
            "--tenant-scoped",
            "--broadcast",
        ],
    );
    assert!(
        second.contains("  reuse        src/adapters/http/access.rs\n"),
        "{second}"
    );
    assert!(root.join("src/adapters/http/streams.rs").is_file());
    let third = succeeds(
        &root,
        &[
            "generate",
            "scaffold",
            "tag",
            "label:string",
            "--global",
            "--broadcast",
        ],
    );
    assert!(
        third.contains("  reuse        src/adapters/http/streams.rs\n"),
        "{third}"
    );

    let model = succeeds(
        &root,
        &[
            "generate",
            "model",
            "comment",
            "body:text",
            "--tenant-scoped",
        ],
    );
    assert!(
        model.contains("  create       src/domain/entities/comment.rs\n"),
        "{model}"
    );
    assert!(!model.contains("reuse"), "{model}");

    let controller = succeeds(
        &root,
        &["generate", "controller", "pages", "index", "about"],
    );
    assert!(
        controller.contains("  create       templates/pages/about.html\n"),
        "{controller}"
    );
    assert!(
        controller.contains("  reuse        src/adapters/http/access.rs\n"),
        "{controller}"
    );
    assert!(
        controller.contains("Wire the controller by hand"),
        "{controller}"
    );

    let mailer = succeeds(&root, &["generate", "mailer", "user", "welcome:name"]);
    assert!(
        mailer.contains("  create       src/adapters/email/user_mailer.rs\n"),
        "{mailer}"
    );

    let job = succeeds(
        &root,
        &["generate", "job", "send_welcome", "user_id:uuid", "--queue"],
    );
    assert!(
        job.contains("  create       src/adapters/jobs/send_welcome.rs\n"),
        "{job}"
    );

    // A dry run lists what it would create and writes nothing.
    let dry = succeeds(
        &root,
        &["generate", "mailer", "billing", "invoice", "--dry-run"],
    );
    assert!(
        dry.contains("  would create src/adapters/email/billing_mailer.rs\n"),
        "{dry}"
    );
    assert!(!root.join("src/adapters/email/billing_mailer.rs").exists());

    // The sign-in pages are part of `forge new`.
    let refusal = fails(&root, &["generate", "authentication", "--dry-run"]);
    assert!(
        refusal.contains("refusing to overwrite existing files"),
        "{refusal}"
    );
}

#[test]
fn destroy_takes_back_what_each_generator_wrote() {
    let (_parent, root) = application();
    succeeds(
        &root,
        &["generate", "scaffold", "post", "title:string", "--global"],
    );
    succeeds(
        &root,
        &[
            "generate",
            "model",
            "comment",
            "body:text",
            "--tenant-scoped",
        ],
    );
    succeeds(&root, &["generate", "controller", "pages", "index"]);
    succeeds(&root, &["generate", "mailer", "user", "welcome"]);
    succeeds(&root, &["generate", "job", "send_welcome", "--queue"]);

    let job = succeeds(&root, &["destroy", "job", "send_welcome", "--queue"]);
    assert!(
        job.contains("  remove       src/adapters/jobs/send_welcome.rs\n"),
        "{job}"
    );
    assert!(!root.join("src/adapters/jobs/send_welcome.rs").exists());
    assert!(
        job.contains("Take out the wiring you added by hand"),
        "{job}"
    );

    let mailer = succeeds(&root, &["destroy", "mailer", "user", "welcome"]);
    assert!(
        mailer.contains("  remove       src/adapters/email/user_mailer.rs\n"),
        "{mailer}"
    );

    let controller = succeeds(&root, &["destroy", "controller", "pages", "index"]);
    assert!(
        controller.contains("  remove       src/adapters/http/pages_controller.rs\n"),
        "{controller}"
    );

    // A model's migrations stay unless asked for, and a dry run removes
    // nothing.
    let model = succeeds(
        &root,
        &[
            "destroy",
            "model",
            "comment",
            "body:text",
            "--tenant-scoped",
            "--dry-run",
        ],
    );
    assert!(model.contains("  keep         migrations/"), "{model}");
    assert!(
        model.contains("  would remove src/domain/entities/comment.rs\n"),
        "{model}"
    );
    assert!(root.join("src/domain/entities/comment.rs").is_file());
    let model = succeeds(
        &root,
        &[
            "destroy",
            "model",
            "comment",
            "body:text",
            "--tenant-scoped",
        ],
    );
    assert!(
        model.contains("  remove       src/domain/entities/comment.rs\n"),
        "{model}"
    );
    assert!(!root.join("src/domain/entities/comment.rs").exists());

    let scaffold = succeeds(
        &root,
        &[
            "destroy",
            "scaffold",
            "post",
            "title:string",
            "--global",
            "--migrations",
        ],
    );
    assert!(
        scaffold.contains("  remove       migrations/"),
        "{scaffold}"
    );
    assert!(!root.join("templates/posts").exists());

    // Nothing is left to destroy, and migrations are never destroyed.
    let nothing = fails(&root, &["destroy", "job", "send_welcome", "--queue"]);
    assert!(nothing.contains("nothing to destroy"), "{nothing}");
    let migration = fails(&root, &["destroy", "migration", "create_posts"]);
    assert!(
        migration.contains("forge destroy does not remove migrations"),
        "{migration}"
    );
}

// --- cargo wrappers ---------------------------------------------------------------

const FAKE_CARGO: &str = r#"#!/bin/sh
echo "cargo $*" >> "$RECORD"
exit "${CARGO_EXIT:-0}"
"#;

struct Bin {
    directory: tempfile::TempDir,
}

impl Bin {
    fn new() -> Self {
        let directory = tempfile::tempdir().expect("temporary directory");
        fs::create_dir(directory.path().join("bin")).expect("bin");
        let cargo = directory.path().join("bin/cargo");
        fs::write(&cargo, FAKE_CARGO).expect("script");
        fs::set_permissions(&cargo, fs::Permissions::from_mode(0o755)).expect("chmod");
        Self { directory }
    }

    fn record(&self) -> PathBuf {
        self.directory.path().join("record")
    }

    fn command(&self, arguments: &[&str]) -> Command {
        let mut command = forge_command(self.directory.path());
        command
            .args(arguments)
            .env("PATH", self.directory.path().join("bin"))
            .env("RECORD", self.record());
        command
    }

    fn calls(&self) -> Vec<String> {
        fs::read_to_string(self.record())
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

#[test]
fn the_cargo_wrappers_run_cargo_with_the_documented_arguments() {
    let bin = Bin::new();
    for arguments in [
        &["check"][..],
        &["test"],
        &["test", "--lib", "orders", "--", "--nocapture"],
        &["format"],
        &["lint"],
        &["build"],
        &["build", "shop"],
    ] {
        let output = bin.command(arguments).output().expect("forge runs");
        assert!(
            output.status.success(),
            "{arguments:?}: {}",
            stderr(&output)
        );
    }
    assert_eq!(
        bin.calls(),
        [
            "cargo check --all-targets --all-features",
            "cargo test --all-targets --all-features",
            "cargo test --lib orders -- --nocapture",
            "cargo fmt --all",
            "cargo clippy --all-targets --all-features -- -D warnings",
            "cargo build --release --locked",
            "cargo build --release --locked --bin shop",
        ]
    );
}

#[test]
fn a_failing_cargo_fails_the_command_with_its_exit_code_in_the_message() {
    let bin = Bin::new();
    let output = bin
        .command(&["check"])
        .env("CARGO_EXIT", "3")
        .output()
        .expect("forge runs");
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("`cargo check` failed with exit code 3"),
        "{}",
        stderr(&output)
    );
    // No cargo at all.
    let output = forge_command(bin.directory.path())
        .arg("lint")
        .env("PATH", bin.directory.path().join("empty"))
        .output()
        .expect("forge runs");
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("could not run `cargo clippy`"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn new_generates_the_lockfile_with_cargo_and_warns_when_that_fails() {
    let bin = Bin::new();
    let output = bin
        .command(&["new", "shop", "--forge-path", "../forge"])
        .output()
        .expect("forge runs");
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(bin.calls(), ["cargo generate-lockfile"]);
    assert!(!stdout(&output).contains("warning:"), "{}", stdout(&output));

    let failing = Bin::new();
    let output = failing
        .command(&["new", "shop", "--forge-path", "../forge"])
        .env("CARGO_EXIT", "101")
        .output()
        .expect("forge runs");
    assert!(output.status.success(), "the application was still created");
    let text = stdout(&output);
    assert!(
        text.contains("warning: `cargo generate-lockfile` failed with exit code 101"),
        "{text}"
    );
    assert!(text.contains("run `cargo generate-lockfile`"), "{text}");
    assert!(failing.directory.path().join("shop/Cargo.toml").is_file());
}

#[test]
fn version_prints_the_package_version() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let text = succeeds(directory.path(), &["version"]);
    assert_eq!(text, format!("forge {}\n", env!("CARGO_PKG_VERSION")));
}
