//! The generators, `forge destroy`, the cargo wrappers and `forge new`'s
//! lockfile step through the real `forge` executable. Nothing here builds a
//! generated application: `tests/scaffold.rs` does.
#![cfg(unix)]

use std::fs;
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
fn every_generator_lists_its_files_and_registers_its_wiring() {
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
    assert!(
        scaffold.contains("  wire         src/bootstrap/routes_gen.rs  (scaffold posts)\n"),
        "{scaffold}"
    );
    assert!(
        scaffold.contains("  declare      src/domain/entities/mod.rs  (pub mod post;)\n"),
        "{scaffold}"
    );
    assert!(!scaffold.contains("by hand"), "{scaffold}");
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
        controller.contains("  wire         src/bootstrap/routes_gen.rs  (controller pages)\n"),
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

/// Forge-Go re-runs `go mod vendor` after `forge generate`, because what it
/// generates can import framework packages the vendor directory lacks.
/// Forge-Rust has no such step (forge-rust#77): `forge new` sets up no
/// vendoring, and a generator never edits `Cargo.toml`, `Cargo.lock` or
/// `.cargo/config.toml` and writes only code for crates the manifest already
/// declares, so an application vendored with `cargo vendor` has nothing to
/// re-vendor. This keeps that true for every generator.
#[test]
fn generators_leave_the_manifest_lockfile_and_vendored_dependencies_alone() {
    let (_parent, root) = application();
    let sentinels = [
        ("Cargo.lock", "# a lockfile\n"),
        (
            ".cargo/config.toml",
            "[source.crates-io]\nreplace-with = \"vendored-sources\"\n",
        ),
        ("vendor/example/src/lib.rs", "// vendored\n"),
    ];
    for (path, content) in sentinels {
        let path = root.join(path);
        fs::create_dir_all(path.parent().expect("a parent")).expect("directory");
        fs::write(path, content).expect("sentinel");
    }
    let guarded = [
        "Cargo.toml",
        "Cargo.lock",
        ".cargo/config.toml",
        "vendor/example/src/lib.rs",
    ];
    let snapshot = || -> Vec<String> {
        guarded
            .iter()
            .map(|path| fs::read_to_string(root.join(path)).expect("guarded file"))
            .collect()
    };
    let before = snapshot();

    for arguments in [
        &[
            "generate",
            "scaffold",
            "post",
            "title:string",
            "--global",
            "--broadcast",
        ][..],
        &[
            "generate",
            "scaffold",
            "note",
            "body:text",
            "--tenant-scoped",
        ],
        &[
            "generate",
            "model",
            "comment",
            "body:text",
            "--tenant-scoped",
        ],
        &["generate", "controller", "pages", "index"],
        &["generate", "mailer", "user", "welcome:name"],
        &["generate", "job", "send_welcome", "user_id:uuid", "--queue"],
        &["generate", "migration", "add_things"],
    ] {
        let output = forge(&root, arguments);
        assert!(
            output.status.success(),
            "{arguments:?}: {}",
            stderr(&output)
        );
        assert_eq!(snapshot(), before, "{arguments:?} changed a guarded file");
        assert!(!stdout(&output).contains("vendor"), "{arguments:?}");
    }
    // Nothing else appeared under the guarded directories either.
    let vendored: Vec<_> = fs::read_dir(root.join("vendor"))
        .expect("vendor directory")
        .collect();
    assert_eq!(vendored.len(), 1);
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
        write_executable(&cargo, FAKE_CARGO);
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
        &["format", "--check"],
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
            "cargo fmt --all -- --check --files-with-diff",
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

#[test]
fn dry_runs_of_models_and_public_controllers() {
    let (_parent, root) = application();
    let model = succeeds(
        &root,
        &[
            "generate",
            "model",
            "comment",
            "body:text",
            "--global",
            "--dry-run",
        ],
    );
    assert!(
        model.contains("  would create src/domain/entities/comment.rs\n"),
        "{model}"
    );
    assert!(!root.join("src/domain/entities/comment.rs").exists());

    // Public pages need no access policy: nothing is reused or created.
    let controller = succeeds(
        &root,
        &["generate", "controller", "pages", "index", "--public"],
    );
    assert!(
        controller.contains("  create       templates/pages/index.html\n"),
        "{controller}"
    );
    assert!(!controller.contains("reuse"), "{controller}");
    assert!(!root.join("src/adapters/http/access.rs").exists());
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

/// `forge g` and `forge d` are `generate` and `destroy` (D25); `generate
/// migration` infers ALTER TABLE from add_X_to_TABLE (D26), warns about a
/// blank pair and a column drop, and reports files as Forge-Go does.
#[test]
fn g_and_d_aliases_and_inferred_migrations() {
    let (_parent, root) = application();

    let created = succeeds(
        &root,
        &["g", "scaffold", "order", "total:int", "--tenant-scoped"],
    );
    assert!(
        created.contains("  wire         src/bootstrap/routes_gen.rs  (scaffold orders)\n"),
        "{created}"
    );
    let again = succeeds(
        &root,
        &["g", "scaffold", "order", "total:integer", "--tenant-scoped"],
    );
    assert!(
        again.contains("  identical    src/adapters/http/orders.rs\n"),
        "{again}"
    );
    assert!(!again.contains("  create"), "{again}");

    let added = succeeds(
        &root,
        &["g", "migration", "add_sku_to_orders", "sku:string"],
    );
    let up = added
        .lines()
        .find_map(|line| line.strip_prefix("  create  "))
        .expect("the up file is listed");
    assert!(
        up.starts_with("migrations/") && up.ends_with("_add_sku_to_orders.up.sql"),
        "{added}"
    );
    let sql = std::fs::read_to_string(root.join(up)).expect("up migration");
    assert!(
        sql.contains("ALTER TABLE \"orders\" ADD COLUMN \"sku\" text NOT NULL DEFAULT ''"),
        "{sql}"
    );
    assert!(!added.contains("WARNING"), "{added}");

    let removed = succeeds(
        &root,
        &["g", "migration", "remove_sku_from_orders", "sku:string"],
    );
    assert!(
        removed.contains("WARNING: forge migrate drops these columns"),
        "{removed}"
    );

    let blank = succeeds(&root, &["g", "migration", "backfill_orders"]);
    assert!(
        blank.contains("WARNING: fill in migrations/")
            && blank.contains("an empty migration is refused"),
        "{blank}"
    );
    let refused = fails(&root, &["g", "migration", "create_widgets", "name:string"]);
    assert!(refused.contains("forge generate model"), "{refused}");
    let refused = fails(
        &root,
        &["g", "migration", "add_at_to_orders", "at:timestamp"],
    );
    assert!(refused.contains("at:timestamp?"), "{refused}");

    let destroyed = succeeds(
        &root,
        &["d", "scaffold", "order", "total:int", "--tenant-scoped"],
    );
    assert!(
        destroyed.contains("  unwire       src/bootstrap/routes_gen.rs  (scaffold orders)\n"),
        "{destroyed}"
    );
    assert!(!root.join("src/adapters/http/orders.rs").exists());
}

/// `forge generate job` needs no --queue (every Forge-Rust job is a queue
/// job), and accepts it, as Forge-Go spells it.
#[test]
fn a_job_needs_no_queue_flag() {
    let (_parent, root) = application();
    let job = succeeds(&root, &["generate", "job", "cleanup"]);
    assert!(job.contains("src/adapters/jobs/cleanup.rs"), "{job}");
    let queued = succeeds(&root, &["generate", "job", "send_welcome", "--queue"]);
    assert!(
        queued.contains("src/adapters/jobs/send_welcome.rs"),
        "{queued}"
    );
}
