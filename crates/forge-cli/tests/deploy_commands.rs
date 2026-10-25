//! `forge deploy`, `forge logs` and `forge app` through the real `forge`
//! executable, with a stand-in `ssh` on `PATH`: argument handling, exit
//! codes and printed output. The deployer itself is tested without a process
//! in `src/deploy/` and against a real `sshd` in `src/deploy/ssh_tests.rs`.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const FORGE: &str = env!("CARGO_BIN_EXE_forge");

/// One proxied role on one host, no registry, no secrets, no migration.
const CONFIG: &str = "\
service: shop
image: registry.example.com/acme/shop
servers:
  web:
    hosts: [192.0.2.10]
proxy:
  image: registry.example.com/forge-proxy@sha256:1111111111111111111111111111111111111111111111111111111111111111
  hosts: [shop.example.com]
env:
  clear:
    FORGE_ENV: production
migrate:
  enabled: false
";

/// Records its arguments, answers `docker version`, and reports a held lock
/// when `LOCK_HELD` is set.
const FAKE_SSH: &str = r#"#!/bin/sh
printf '%s\n' "$*" >> "$RECORD"
for last; do :; done
case "$last" in
  *"docker version"*) echo "27.0.1";;
  *"mkdir .forge/shop/lock"*)
    if [ -n "$LOCK_HELD" ]; then
      echo '{"holder":"bob@ci","message":"window","acquired_at":"2026-10-19T11:00:00Z"}'
      exit 75
    fi;;
  *"if [ -d .forge/shop/lock ]"*)
    if [ -n "$LOCK_HELD" ]; then
      echo '{"holder":"bob@ci","message":"window","acquired_at":"2026-10-19T11:00:00Z"}'
    fi;;
esac
exit 0
"#;

struct Project {
    directory: tempfile::TempDir,
}

impl Project {
    fn new() -> Self {
        let directory = tempfile::tempdir().expect("temporary directory");
        let root = directory.path();
        fs::create_dir_all(root.join("config")).expect("config directory");
        fs::write(root.join("config/deploy.yml"), CONFIG).expect("config");
        fs::create_dir(root.join("bin")).expect("bin");
        let ssh = root.join("bin/ssh");
        fs::write(&ssh, FAKE_SSH).expect("script");
        fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).expect("chmod");
        Self { directory }
    }

    fn root(&self) -> &Path {
        self.directory.path()
    }

    fn record(&self) -> PathBuf {
        self.root().join("record")
    }

    fn command(&self, arguments: &[&str]) -> Command {
        let mut command = Command::new(FORGE);
        for (name, _) in std::env::vars_os() {
            if name.to_string_lossy().starts_with("FORGE_TEST_") {
                command.env_remove(name);
            }
        }
        let path = format!(
            "{}:{}",
            self.root().join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        command
            .args(arguments)
            .current_dir(self.root())
            .env("PATH", path)
            .env("RECORD", self.record())
            .env_remove("LOCK_HELD");
        command
    }

    fn run(&self, arguments: &[&str]) -> Output {
        self.command(arguments).output().expect("forge runs")
    }

    fn ssh_calls(&self) -> Vec<String> {
        fs::read_to_string(self.record())
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn init_writes_the_configuration_once() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let run = |arguments: &[&str]| {
        Command::new(FORGE)
            .args(arguments)
            .current_dir(directory.path())
            .output()
            .expect("forge runs")
    };
    let output = run(&["deploy", "init", "--service", "storefront"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("wrote config/deploy.yml\n"), "{text}");
    assert!(
        text.contains("next: edit config/deploy.yml and .forge/secrets"),
        "{text}"
    );
    let config = fs::read_to_string(directory.path().join("config/deploy.yml")).expect("config");
    assert!(config.contains("service: storefront"), "{config}");
    let again = run(&["deploy", "init", "--service", "storefront"]);
    assert_eq!(again.status.code(), Some(1));
    assert!(
        stderr(&again).contains("already exists"),
        "{}",
        stderr(&again)
    );
}

#[test]
fn a_missing_configuration_is_reported_with_the_path_as_given() {
    let directory = tempfile::tempdir().expect("temporary directory");
    for arguments in [
        &["deploy", "setup"][..],
        &["logs"],
        &["app", "exec", "migrate"],
        &["app", "shell"],
    ] {
        let output = Command::new(FORGE)
            .args(arguments)
            .current_dir(directory.path())
            .output()
            .expect("forge runs");
        assert_eq!(output.status.code(), Some(1), "{arguments:?}");
        assert!(
            stderr(&output).contains("read deploy config: open config/deploy.yml: no such file"),
            "{arguments:?}: {}",
            stderr(&output)
        );
    }
}

#[test]
fn the_version_and_skip_build_flags_need_a_plain_deploy() {
    let project = Project::new();
    let output = project.run(&["deploy", "--version", "v1", "details"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output)
            .contains("--version and --skip-build apply to `forge deploy` without a subcommand"),
        "{}",
        stderr(&output)
    );
    assert!(project.ssh_calls().is_empty());
}

#[test]
fn dry_runs_print_the_plan_and_change_nothing() {
    let project = Project::new();
    let deploy = project.run(&["deploy", "--dry-run", "--version", "v1", "--skip-build"]);
    assert!(deploy.status.success(), "{}", stderr(&deploy));
    let text = stdout(&deploy);
    assert!(text.contains("# lock acquired (deploy v1)"), "{text}");
    assert!(
        text.contains("$ docker pull registry.example.com/acme/shop:v1"),
        "{text}"
    );
    assert!(text.contains("dry run: nothing was changed ("), "{text}");

    let setup = project.run(&["deploy", "setup", "--dry-run"]);
    assert!(setup.status.success(), "{}", stderr(&setup));
    assert!(
        stdout(&setup).contains("# docker 27.0.1"),
        "{}",
        stdout(&setup)
    );

    for (arguments, want) in [
        (
            &["deploy", "prune", "--dry-run"][..],
            "# lock acquired (prune)",
        ),
        (
            &["deploy", "maintenance", "on", "--dry-run"],
            "maintenance true",
        ),
        (
            &["deploy", "maintenance", "off", "--dry-run"],
            "maintenance false",
        ),
    ] {
        let output = project.run(arguments);
        assert!(
            output.status.success(),
            "{arguments:?}: {}",
            stderr(&output)
        );
        assert!(
            stdout(&output).contains(want),
            "{arguments:?}: {}",
            stdout(&output)
        );
    }

    // Rolling back to a version no host retains is refused before any change.
    let rollback = project.run(&["deploy", "rollback", "v0", "--dry-run"]);
    assert_eq!(rollback.status.code(), Some(1));
    assert!(
        stderr(&rollback).contains("version v0 is not retained"),
        "{}",
        stderr(&rollback)
    );

    // The only commands that reached the host are read-only queries (and
    // the control master's exit).
    for call in project.ssh_calls() {
        assert!(
            call.contains("docker version")
                || call.contains("docker ps --all")
                || call.contains("docker image ls")
                || call.contains("if [ -d .forge/shop/lock ]")
                || call.contains("-O exit"),
            "a dry run ran: {call}"
        );
    }
}

#[test]
fn json_reports_stay_on_stdout_and_progress_on_stderr() {
    let project = Project::new();
    let output = project.run(&["deploy", "prune", "--dry-run", "--json"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let report: serde_json::Value =
        serde_json::from_str(stdout(&output).trim()).expect("one JSON line");
    assert_eq!(report["ok"], true);
    assert_eq!(report["action"], "prune");
    assert!(
        stderr(&output).contains("# lock acquired (prune)"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn lock_commands_report_and_enforce_the_lock() {
    let project = Project::new();
    let status = project.run(&["deploy", "lock", "status"]);
    assert!(status.status.success(), "{}", stderr(&status));
    assert_eq!(stdout(&status), "not locked\n");

    let held = project
        .command(&["deploy", "lock", "status"])
        .env("LOCK_HELD", "1")
        .output()
        .expect("forge runs");
    assert!(held.status.success(), "{}", stderr(&held));
    assert!(
        stdout(&held).starts_with("192.0.2.10: held by bob@ci since 2026-10-19T11:00:00Z (window)"),
        "{}",
        stdout(&held)
    );

    let free = project.run(&["deploy", "lock", "acquire", "--message", "maintenance"]);
    assert!(free.status.success(), "{}", stderr(&free));
    assert_eq!(stdout(&free), "lock acquired\n");

    // A held lock is exit status 3.
    let refused = project
        .command(&["deploy", "lock", "acquire"])
        .env("LOCK_HELD", "1")
        .output()
        .expect("forge runs");
    assert_eq!(refused.status.code(), Some(3), "{}", stderr(&refused));
    assert!(
        stderr(&refused).contains("deploy lock is held"),
        "{}",
        stderr(&refused)
    );

    let release = project.run(&["deploy", "lock", "release", "--force"]);
    assert!(release.status.success(), "{}", stderr(&release));
    assert_eq!(stdout(&release), "lock released\n");
}

#[test]
fn details_print_the_host() {
    let project = Project::new();
    let output = project.run(&["deploy", "details"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).starts_with("192.0.2.10 (web)\n"),
        "{}",
        stdout(&output)
    );
    let output = project.run(&["deploy", "details", "--json"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let report: serde_json::Value = serde_json::from_str(stdout(&output).trim()).expect("JSON");
    assert_eq!(report["service"], "shop");
}

#[test]
fn logs_and_app_commands_print_the_remote_command_in_a_dry_run() {
    let project = Project::new();
    for (arguments, want) in [
        (
            &[
                "logs",
                "--dry-run",
                "--lines",
                "5",
                "--since",
                "1h",
                "--grep",
                "error",
            ][..],
            "docker logs --timestamps --since 1h --tail 5",
        ),
        (
            &[
                "logs",
                "--dry-run",
                "-f",
                "--role",
                "web",
                "--host",
                "192.0.2.10",
            ],
            "--follow",
        ),
        (
            &["app", "exec", "--dry-run", "migrate", "status"],
            "migrate status",
        ),
        (
            &[
                "app",
                "exec",
                "--dry-run",
                "-i",
                "--reuse",
                "--host",
                "192.0.2.10",
                "sh",
            ],
            "docker exec --interactive --tty",
        ),
        (&["app", "shell", "--dry-run"], "\"$c\" sh"),
        (
            &["app", "shell", "--dry-run", "--image", "busybox:1.37"],
            "busybox:1.37",
        ),
    ] {
        let output = project.run(arguments);
        assert!(
            output.status.success(),
            "{arguments:?}: {}",
            stderr(&output)
        );
        assert!(
            stdout(&output).contains(want),
            "{arguments:?}: {}",
            stdout(&output)
        );
    }
    let invalid = project.run(&["logs", "--dry-run", "--since", "yesterday!"]);
    assert_eq!(invalid.status.code(), Some(1));
    assert!(
        stderr(&invalid).contains("--since must be a duration"),
        "{}",
        stderr(&invalid)
    );
}
