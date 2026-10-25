//! `forge deploy` operations and commands against the recording fakes: the
//! failure paths of every step (warnings that must not fail a deploy, errors
//! that must), locks, hooks, local builds, details, logs and `forge app`.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::cli::{
    AppAction, Common, DeployAction, Factory, Io, LockAction, run_app, run_deploy, run_logs,
};
use super::commands::RemoteCommand;
use super::config::{Config, DEFAULT_CONFIG_PATH};
use super::deployer::{parse_inventory, parse_lock_info, performer, resolve_version};
use super::errors::{
    EXIT_HOOK_FAILED, EXIT_INCONSISTENT, EXIT_LOCK_HELD, EXIT_UNHEALTHY, LOCK_GONE_EXIT_STATUS,
    LOCK_HELD_EXIT_STATUS, LOCK_NOT_OURS_EXIT_STATUS,
};
use super::operations::Outcome;
use super::remote::{ExecOptions, LogsOptions, PrefixWriter};
use super::runner::{Cancel, ExecLocal, LocalRunner, Output, RunError, Runner, Sink};
use super::secrets::Secrets;
use super::tests::{
    Buffer, FakeRunner, PLAN_SECRETS, VALID_CONFIG, common, image_json, io, plan_fixture, remote,
};

const HOSTS: [&str; 3] = ["192.0.2.10", "192.0.2.11", "192.0.2.20"];

fn held(host: &str) -> RunError {
    RunError::Remote(super::errors::RemoteError {
        host: host.to_owned(),
        exit_code: LOCK_HELD_EXIT_STATUS,
        stderr: String::new(),
        stdout: "{\"holder\":\"bob@ci\",\"message\":\"window\",\"acquired_at\":\"2026-10-19T11:00:00Z\"}\n".to_owned(),
    })
}

fn is_lock_acquire(command: &RemoteCommand) -> bool {
    command.script.contains("mkdir .forge/shop/lock")
}

fn is_lock_release(command: &RemoteCommand) -> bool {
    command.script.starts_with("record=$(cat)")
}

fn write_hook(root: &Path, name: &str, body: &str) {
    let hooks = root.join(".forge/hooks");
    std::fs::create_dir_all(&hooks).expect("hooks directory");
    let path = hooks.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("hook");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).expect("chmod");
}

/// Runs `deploy` with every remote command for which `fail` answers `Some`
/// failing, and returns the outcome with the progress log.
fn deploy_failing(
    fail: impl Fn(&str, &RemoteCommand) -> Option<RunError> + Send + Sync + 'static,
) -> (Outcome, String) {
    let fixture = plan_fixture(false);
    fixture.runner.fail_with(fail);
    let outcome = fixture.deployer.deploy(&Cancel::never(), "v3", true);
    (outcome, fixture.log.text())
}

fn warnings_of(outcome: &Outcome) -> Vec<String> {
    outcome
        .0
        .as_ref()
        .map(|report| report.warnings.clone())
        .unwrap_or_default()
}

// --- secrets and preconditions ------------------------------------------------

#[test]
fn every_operation_refuses_to_start_without_its_secrets() {
    let mut fixture = plan_fixture(false);
    fixture.deployer.secrets = Secrets::default();
    let deployer = &fixture.deployer;
    let cancel = Cancel::never();
    let out = Buffer::default();

    let refused = |outcome: Outcome| {
        let (report, result) = outcome;
        assert!(
            report.is_none(),
            "no report for an operation that never started"
        );
        let error = result.expect_err("missing secrets");
        assert!(
            error
                .message
                .starts_with("secrets not defined in .forge/secrets: "),
            "{error}"
        );
    };
    refused(deployer.setup(&cancel));
    refused(deployer.deploy(&cancel, "v3", true));
    refused(deployer.rollback(&cancel, "v1"));
    refused(deployer.prune(&cancel));
    let (hosts, result) = deployer.details(&cancel);
    assert!(hosts.is_empty());
    assert!(result.is_err());
    assert!(deployer.maintenance(&cancel, true).is_err());
    assert!(deployer.lock_acquire(&cancel, "window").is_err());
    assert!(deployer.lock_status(&cancel).is_err());
    assert!(deployer.lock_release(&cancel, true).is_err());
    assert!(
        deployer
            .logs(&cancel, out.writer(), LogsOptions::default())
            .is_err()
    );
    let exec = ExecOptions {
        argv: vec!["migrate".to_owned()],
        ..ExecOptions::default()
    };
    assert!(deployer.exec(&cancel, out.writer(), &exec).is_err());
    assert!(deployer.shell(&cancel, "", "", "").is_err());
    assert!(fixture.runner.calls().is_empty(), "nothing reached a host");
}

#[test]
fn invalid_versions_are_refused_before_the_lock_is_taken() {
    let fixture = plan_fixture(false);
    let (report, result) = fixture
        .deployer
        .deploy(&Cancel::never(), "bad version!", true);
    assert!(report.is_none());
    assert!(result.expect_err("invalid").message.contains("must match"));
    let (report, result) = fixture.deployer.rollback(&Cancel::never(), "-bad");
    assert!(report.is_none());
    assert!(result.expect_err("invalid").message.contains("must match"));
    assert!(fixture.runner.calls().is_empty());
}

#[test]
fn the_performer_is_user_at_host() {
    let performer = performer();
    let (user, host) = performer.split_once('@').expect("user@host");
    assert!(!user.is_empty() && !host.is_empty(), "{performer}");
}

// --- setup --------------------------------------------------------------------

#[test]
fn setup_warns_about_a_proxy_image_that_is_not_pinned_by_digest() {
    let mut fixture = plan_fixture(false);
    fixture.deployer.config.proxy.image = "registry.example.com/forge-proxy:1".to_owned();
    let (report, result) = fixture.deployer.setup(&Cancel::never());
    result.expect("setup");
    let warnings = report.expect("report").warnings;
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(
        warnings[0]
            .starts_with("proxy.image registry.example.com/forge-proxy:1 is not pinned by digest"),
        "{warnings:?}"
    );
}

#[test]
fn setup_names_the_step_that_failed() {
    for (marker, want) in [
        ("docker version", "docker is not available"),
        ("accessory-db.env", "192.0.2.20"),
        ("docker login", "registry login"),
    ] {
        let fixture = plan_fixture(false);
        fixture.runner.fail_with(move |host, command| {
            command
                .script
                .contains(marker)
                .then(|| remote(host, 1, "refused"))
        });
        let (report, result) = fixture.deployer.setup(&Cancel::never());
        let error = result.expect_err(marker);
        assert!(error.message.contains(want), "{marker}: {error}");
        assert!(report.is_some(), "the report is kept for --json");
    }
}

#[test]
fn setup_reports_the_hosts_that_hold_the_lock() {
    let fixture = plan_fixture(false);
    fixture.runner.fail_with(|host, command| {
        (host == "192.0.2.10" && is_lock_acquire(command)).then(|| held(host))
    });
    for outcome in [
        fixture.deployer.setup(&Cancel::never()),
        fixture.deployer.rollback(&Cancel::never(), "v1"),
        fixture.deployer.prune(&Cancel::never()),
    ] {
        let (report, result) = outcome;
        assert!(report.is_some());
        assert_eq!(result.expect_err("held").exit_code(), EXIT_LOCK_HELD);
    }
}

// --- deploy -------------------------------------------------------------------

#[test]
fn deploy_stops_when_a_step_before_the_switch_fails() {
    for (marker, want) in [
        ("jobs.env", "192.0.2.11"),
        ("docker pull", "pull registry.example.com/acme/shop:v3"),
        ("docker ps --all", "remote command exited"),
    ] {
        let (outcome, _) = deploy_failing(move |host, command| {
            (host == "192.0.2.11" && command.script.contains(marker))
                .then(|| remote(host, 1, "refused"))
        });
        let error = outcome.1.expect_err(marker);
        assert!(error.message.contains(want), "{marker}: {error}");
        assert!(
            outcome.0.expect("report").steps.is_empty(),
            "{marker}: nothing was switched"
        );
    }
}

#[test]
fn a_failing_post_deploy_hook_fails_the_deploy_after_the_switch() {
    let fixture = plan_fixture(false);
    let mut deployer = fixture.deployer;
    deployer.local = Box::new(ExecLocal);
    write_hook(
        &deployer.root,
        "post-deploy",
        "echo hook says no >&2\nexit 1",
    );
    let (report, result) = deployer.deploy(&Cancel::never(), "v3", true);
    let error = result.expect_err("hook fails");
    assert_eq!(error.exit_code(), EXIT_HOOK_FAILED, "{error}");
    assert!(error.message.contains("hook post-deploy failed"), "{error}");
    // The containers were switched before the hook ran.
    assert_eq!(report.expect("report").steps.len(), 3);
    assert!(
        fixture
            .runner
            .calls()
            .iter()
            .any(|call| call.contains("forge-proxy deploy"))
    );
}

#[test]
fn a_hook_that_cannot_be_inspected_fails_with_the_hook_exit_code() {
    let fixture = plan_fixture(false);
    let deployer = fixture.deployer;
    // `.forge/hooks` is a file, so `.forge/hooks/pre-deploy` is neither
    // missing nor a file.
    std::fs::create_dir_all(deployer.root.join(".forge")).expect("directory");
    std::fs::write(deployer.root.join(".forge/hooks"), "not a directory").expect("file");
    let (_, result) = deployer.deploy(&Cancel::never(), "v3", true);
    let error = result.expect_err("hook cannot be read");
    assert_eq!(error.exit_code(), EXIT_HOOK_FAILED);
    assert!(error.message.starts_with("hook pre-deploy: "), "{error}");
}

/// A local runner that answers `git` queries and fails the command whose
/// words contain `fail_on`.
struct ScriptedLocal {
    head: Result<String, String>,
    status: Result<String, String>,
    fail_on: &'static str,
    calls: Arc<Mutex<Vec<String>>>,
}

impl ScriptedLocal {
    fn new(fail_on: &'static str) -> Self {
        Self {
            head: Ok("0123456789ab\n".to_owned()),
            status: Ok(String::new()),
            fail_on,
            calls: Arc::default(),
        }
    }
}

impl LocalRunner for ScriptedLocal {
    fn output(&self, _: &Cancel, _: &Path, argv: &[String]) -> Result<String, String> {
        match argv.get(1).map(String::as_str) {
            Some("rev-parse") => self.head.clone(),
            Some("status") => self.status.clone(),
            _ => Err(format!("unexpected query {argv:?}")),
        }
    }

    fn run(
        &self,
        _: &Cancel,
        _: &Path,
        argv: &[String],
        _: Option<&[u8]>,
        _: &[(String, String)],
    ) -> Result<(), String> {
        let line = argv.join(" ");
        self.calls.lock().expect("lock").push(line.clone());
        if !self.fail_on.is_empty() && line.contains(self.fail_on) {
            return Err(format!("{} exited 1 with hunter-two-secret", argv[0]));
        }
        Ok(())
    }
}

#[test]
fn local_build_failures_name_the_step_and_redact_secrets() {
    for (fail_on, want) in [
        ("login", "registry login: docker exited 1 with [REDACTED]"),
        ("build", "build: docker exited 1 with [REDACTED]"),
        ("push", "push: docker exited 1 with [REDACTED]"),
    ] {
        let mut fixture = plan_fixture(false);
        fixture.deployer.local = Box::new(ScriptedLocal::new(fail_on));
        fixture.deployer.secrets = Secrets::from_values(PLAN_SECRETS);
        fixture.deployer.redactor =
            Secrets::from_values([("TOKEN", "hunter-two-secret")]).redactor();
        let (report, result) = fixture.deployer.deploy(&Cancel::never(), "v3", false);
        let error = result.expect_err(fail_on);
        assert_eq!(error.message, want);
        assert!(report.expect("report").steps.is_empty());
        assert!(
            fixture
                .runner
                .calls()
                .iter()
                .all(|call| !call.contains("docker pull")),
            "{fail_on}: nothing reached the hosts after the failed build"
        );
    }
}

#[test]
fn the_version_defaults_to_the_commit_of_a_clean_tree() {
    let cancel = Cancel::never();
    let root = Path::new(".");
    let clean = ScriptedLocal::new("");
    assert_eq!(
        resolve_version(&cancel, &clean, root, "").expect("clean"),
        "0123456789ab"
    );
    assert_eq!(
        resolve_version(&cancel, &clean, root, "v7").expect("explicit"),
        "v7"
    );
    let error = resolve_version(&cancel, &clean, root, "not a version").expect_err("invalid");
    assert!(
        error
            .message
            .starts_with("version \"not a version\" must match "),
        "{error}"
    );

    let mut no_git = ScriptedLocal::new("");
    no_git.head = Err("git: not found".to_owned());
    assert_eq!(
        resolve_version(&cancel, &no_git, root, "")
            .expect_err("no git")
            .message,
        "determine version from git (or pass --version): git: not found"
    );
    let mut broken = ScriptedLocal::new("");
    broken.status = Err("git: index locked".to_owned());
    assert_eq!(
        resolve_version(&cancel, &broken, root, "")
            .expect_err("status fails")
            .message,
        "check working tree: git: index locked"
    );
    let mut dirty = ScriptedLocal::new("");
    dirty.status = Ok(" M src/main.rs\n".to_owned());
    assert_eq!(
        resolve_version(&cancel, &dirty, root, "")
            .expect_err("dirty")
            .message,
        "the working tree has uncommitted changes; commit them or pass --version"
    );
}

#[test]
fn a_failing_wait_for_a_role_without_proxy_reverts_every_host() {
    let (outcome, log) = deploy_failing(|host, command| {
        (host == "192.0.2.11" && command.script.contains("sleep 1; done"))
            .then(|| remote(host, 1, "container is exited"))
    });
    let error = outcome.1.expect_err("not ready");
    assert_eq!(error.exit_code(), EXIT_UNHEALTHY, "{error}");
    assert!(error.message.contains("did not become ready"), "{error}");
    assert!(
        error
            .message
            .ends_with("every host serves its previous version"),
        "{error}"
    );
    assert!(
        log.contains("traffic switched back to shop-web-v2-bbbbbb"),
        "{log}"
    );
}

#[test]
fn a_revert_that_cannot_stop_or_remove_its_containers_is_inconsistent() {
    for marker in ["docker stop", "docker rm "] {
        let (outcome, _) = deploy_failing(move |host, command| {
            if host == "192.0.2.11" && command.script.contains("sleep 1; done") {
                return Some(remote(host, 1, "container is exited"));
            }
            command
                .script
                .contains(marker)
                .then(|| remote(host, 1, "docker is gone"))
        });
        let error = outcome.1.expect_err(marker);
        assert_eq!(error.exit_code(), EXIT_INCONSISTENT, "{marker}: {error}");
        assert!(
            error.message.contains("reverting also failed"),
            "{marker}: {error}"
        );
    }
}

#[test]
fn problems_after_the_switch_are_warnings_not_failures() {
    for (marker, want) in [
        (
            "docker stop",
            "192.0.2.10: stopping shop-web-v2-bbbbbb failed",
        ),
        ("docker rm ", "pruning containers failed"),
        ("docker image ls", "listing images failed"),
        (
            "docker image rm",
            "removing image registry.example.com/acme/shop:v0 failed",
        ),
        ("deploys.log", "recording the deploy failed"),
    ] {
        let (outcome, _) = deploy_failing(move |host, command| {
            command
                .script
                .contains(marker)
                .then(|| remote(host, 1, "refused"))
        });
        let warnings = warnings_of(&outcome);
        outcome.1.as_ref().expect("the new version serves");
        assert!(
            warnings.iter().any(|warning| warning.contains(want)),
            "{marker}: {warnings:?}"
        );
    }
}

#[test]
fn image_listings_with_other_lines_are_skipped_not_removed() {
    let fixture = plan_fixture(false);
    fixture.runner.images.lock().expect("lock").insert(
        "192.0.2.10".to_owned(),
        format!(
            "not json at all\n{}{}{}",
            image_json("v0"),
            "{\"Repository\":\"other/image\",\"Tag\":\"old\"}\n",
            image_json("v1")
        ),
    );
    let (report, result) = fixture.deployer.deploy(&Cancel::never(), "v3", true);
    result.expect("deploy");
    let pruned = report.expect("report").pruned;
    assert!(
        pruned.iter().any(|name| name.ends_with("shop:v0")),
        "{pruned:?}"
    );
    assert!(
        !pruned.iter().any(|name| name.contains("other/image")),
        "{pruned:?}"
    );
}

#[test]
fn a_lock_that_cannot_be_released_after_the_deploy_is_a_warning() {
    let fixture = plan_fixture(false);
    fixture
        .runner
        .fail_with(|host, command| is_lock_release(command).then(|| remote(host, 1, "disk full")));
    fixture
        .deployer
        .deploy(&Cancel::never(), "v3", true)
        .1
        .expect("the deploy itself succeeded");
    let log = fixture.log.text();
    assert!(log.contains("releasing the deploy lock failed"), "{log}");
}

// --- redaction of runner errors -------------------------------------------------

#[test]
fn errors_that_are_not_remote_exits_are_redacted_too() {
    let mut fixture = plan_fixture(false);
    fixture.deployer.secrets = Secrets::from_values(PLAN_SECRETS);
    fixture.deployer.redactor = Secrets::from_values(PLAN_SECRETS).redactor();
    fixture.runner.fail_with(|_, command| {
        command
            .script
            .starts_with("docker version")
            .then(|| RunError::Other("ssh: could not resolve pg-secret-3".to_owned()))
    });
    let (_, result) = fixture.deployer.setup(&Cancel::never());
    let error = result.expect_err("ssh failed");
    assert!(error.message.contains("[REDACTED]"), "{error}");
    assert!(!error.message.contains("pg-secret-3"), "{error}");
}

// --- rollback -----------------------------------------------------------------

#[test]
fn rollback_to_the_running_version_changes_nothing() {
    let fixture = plan_fixture(false);
    let (report, result) = fixture.deployer.rollback(&Cancel::never(), "v2");
    result.expect("rollback");
    assert!(report.expect("report").steps.is_empty());
    let log = fixture.log.text();
    assert!(log.contains("192.0.2.10      web already runs v2"), "{log}");
    assert!(log.contains("jobs already runs v2"), "{log}");
    assert!(
        fixture
            .runner
            .calls()
            .iter()
            .all(|call| !call.contains("docker start")),
        "nothing was restarted"
    );
}

#[test]
fn rollback_lists_the_versions_a_host_does_have() {
    let fixture = plan_fixture(false);
    fixture
        .runner
        .inventory
        .lock()
        .expect("lock")
        .remove("192.0.2.10");
    let error = fixture
        .deployer
        .rollback(&Cancel::never(), "v1")
        .1
        .expect_err("not retained");
    assert!(
        error.message.contains("192.0.2.10/web (available: none)"),
        "{error}"
    );
}

#[test]
fn rollback_hooks_can_stop_it_before_and_after_the_switch() {
    for (name, steps) in [("pre-deploy", 0), ("post-deploy", 3)] {
        let fixture = plan_fixture(false);
        let mut deployer = fixture.deployer;
        deployer.local = Box::new(ExecLocal);
        write_hook(&deployer.root, name, "exit 1");
        let (report, result) = deployer.rollback(&Cancel::never(), "v1");
        let error = result.expect_err(name);
        assert_eq!(error.exit_code(), EXIT_HOOK_FAILED, "{name}: {error}");
        assert_eq!(report.expect("report").steps.len(), steps, "{name}");
    }
}

// --- prune, maintenance and details -------------------------------------------

#[test]
fn prune_fails_when_a_host_cannot_be_inventoried() {
    let fixture = plan_fixture(false);
    fixture.runner.fail_with(|host, command| {
        (host == "192.0.2.11" && command.script.starts_with("docker ps --all"))
            .then(|| remote(host, 1, "docker is down"))
    });
    let (report, result) = fixture.deployer.prune(&Cancel::never());
    assert!(
        result
            .expect_err("inventory")
            .message
            .contains("docker is down")
    );
    assert!(report.is_some());
}

#[test]
fn maintenance_switches_every_web_host_and_needs_a_proxied_role() {
    let mut fixture = plan_fixture(false);
    fixture
        .deployer
        .maintenance(&Cancel::never(), true)
        .expect("on");
    fixture
        .deployer
        .maintenance(&Cancel::never(), false)
        .expect("off");
    let calls = fixture.runner.calls();
    for host in &HOSTS[..2] {
        for state in ["on", "off"] {
            assert!(
                calls.iter().any(|call| call
                    .starts_with(&format!("{host}: docker exec forge-proxy"))
                    && call.ends_with(&format!("maintenance -service shop {state}"))),
                "{host} {state}: {calls:?}"
            );
        }
    }
    let log = fixture.log.text();
    assert!(
        log.contains("maintenance true") && log.contains("maintenance false"),
        "{log}"
    );

    fixture
        .deployer
        .config
        .servers
        .get_mut("web")
        .expect("web")
        .proxy = Some(false);
    let error = fixture
        .deployer
        .maintenance(&Cancel::never(), true)
        .expect_err("no proxy");
    assert_eq!(error.message, "no role uses the proxy");
}

const PROXY_STATUS: &str = "{\"services\":[{\"service\":\"shop\",\"target\":\"shop-web-v2-bbbbbb:8080\",\"hosts\":[\"shop.example.com\"],\"tls\":true,\"maintenance\":false}]}";
const LOCK_RECORD: &str =
    "{\"holder\":\"bob@ci\",\"message\":\"window\",\"acquired_at\":\"2026-10-19T11:00:00Z\"}\n";

#[test]
fn details_collect_containers_proxy_lock_and_history() {
    let fixture = plan_fixture(false);
    {
        let mut answers = fixture.runner.answers.lock().expect("lock");
        answers.push((
            "docker exec forge-proxy".to_owned(),
            PROXY_STATUS.to_owned(),
        ));
        answers.push((
            "if [ -d .forge/shop/lock ]".to_owned(),
            LOCK_RECORD.to_owned(),
        ));
        answers.push((
            "tail -n 5".to_owned(),
            "{\"action\":\"deploy\",\"version\":\"v2\"}\nnot json\n".to_owned(),
        ));
    }
    let (hosts, result) = fixture.deployer.details(&Cancel::never());
    result.expect("details");
    assert_eq!(hosts.len(), 2);
    let first = &hosts[0];
    assert_eq!(first.host, "192.0.2.10");
    assert_eq!(first.containers.len(), 3);
    assert_eq!(first.proxy.as_ref().map(Vec::len), Some(1));
    assert_eq!(
        first.lock.as_ref().map(|lock| lock.holder.as_str()),
        Some("bob@ci")
    );
    assert_eq!(
        first.recent_deploys.len(),
        1,
        "unparsable history lines are skipped"
    );
    assert_eq!(first.error, "");
}

#[test]
fn details_keep_going_when_one_part_of_a_host_fails() {
    let fixture = plan_fixture(false);
    fixture.runner.fail_with(|host, command| {
        if command.script.contains("forge-proxy status") {
            return Some(remote(host, 1, "proxy down"));
        }
        (host == "192.0.2.11" && command.script.starts_with("docker ps --all"))
            .then(|| remote(host, 1, "docker down"))
    });
    let (hosts, result) = fixture.deployer.details(&Cancel::never());
    assert!(
        result
            .expect_err("one host failed")
            .message
            .contains("docker down")
    );
    assert!(
        hosts[0].error.starts_with("forge-proxy: "),
        "{}",
        hosts[0].error
    );
    assert!(hosts[0].containers.len() == 3 && hosts[0].proxy.is_none());
    assert!(hosts[1].error.contains("docker down"), "{}", hosts[1].error);
    assert!(hosts[1].containers.is_empty());
}

#[test]
fn inventories_refuse_output_that_is_not_docker_ps_json() {
    assert!(parse_inventory("\n  \n").expect("blank lines").is_empty());
    let error = parse_inventory("[1]\n").expect_err("array");
    assert_eq!(error.message, "unexpected docker ps output: not an object");
    let error = parse_inventory("{\n").expect_err("broken");
    assert!(
        error.message.starts_with("unexpected docker ps output: "),
        "{error}"
    );
    let info = parse_lock_info("this is not json");
    assert_eq!(
        (info.holder.as_str(), info.message.as_str()),
        ("unknown", "unreadable lock record")
    );
}

// --- locks: release without force ----------------------------------------------

#[test]
fn release_tells_a_vanished_lock_from_one_that_changed_and_from_an_error() {
    for (code, want_ok, want) in [
        (LOCK_GONE_EXIT_STATUS, true, "lock was already released"),
        (
            LOCK_NOT_OURS_EXIT_STATUS,
            false,
            "the lock changed while releasing it",
        ),
        (1, false, "disk full"),
    ] {
        let fixture = plan_fixture(false);
        {
            let record = "{\"holder\":\"alice@laptop\",\"message\":\"deploy\",\"acquired_at\":\"2026-10-19T12:00:00Z\"}\n";
            fixture
                .runner
                .answers
                .lock()
                .expect("lock")
                .push(("if [ -d .forge/shop/lock ]".to_owned(), record.to_owned()));
        }
        fixture.runner.fail_with(move |host, command| {
            is_lock_release(command).then(|| {
                RunError::Remote(super::errors::RemoteError {
                    host: host.to_owned(),
                    exit_code: code,
                    stderr: "disk full".to_owned(),
                    stdout: String::new(),
                })
            })
        });
        let result = fixture.deployer.lock_release(&Cancel::never(), false);
        if want_ok {
            result.expect("an already released lock is fine");
            assert!(fixture.log.text().contains(want), "{}", fixture.log.text());
        } else {
            let error = result.expect_err("release fails");
            assert!(error.message.contains(want), "{error}");
            if code == LOCK_NOT_OURS_EXIT_STATUS {
                assert_eq!(error.exit_code(), EXIT_LOCK_HELD);
            }
        }
    }
}

// --- logs, exec and shell -------------------------------------------------------

#[test]
fn log_selection_follows_the_role_host_and_option_rules() {
    let fixture = plan_fixture(false);
    let out = Buffer::default();
    let cancel = Cancel::never();
    let logs = |options: LogsOptions| fixture.deployer.logs(&cancel, out.writer(), options);

    let error = logs(LogsOptions {
        role: "cron".to_owned(),
        ..LogsOptions::default()
    })
    .expect_err("unknown role");
    assert_eq!(error.message, "unknown role \"cron\"");
    let error = logs(LogsOptions {
        host: "192.0.2.20".to_owned(),
        ..LogsOptions::default()
    })
    .expect_err("not a web host");
    assert_eq!(error.message, "host \"192.0.2.20\" does not run role web");
    assert!(
        logs(LogsOptions {
            since: "yesterday!".to_owned(),
            ..LogsOptions::default()
        })
        .expect_err("since")
        .message
        .starts_with("--since must be")
    );
    assert!(
        logs(LogsOptions {
            lines: 100_001,
            ..LogsOptions::default()
        })
        .expect_err("lines")
        .message
        .starts_with("--lines must be")
    );
    assert!(
        logs(LogsOptions {
            grep: "(".to_owned(),
            ..LogsOptions::default()
        })
        .expect_err("grep")
        .message
        .starts_with("--grep: ")
    );
    assert!(
        fixture.runner.calls().is_empty(),
        "invalid options never reach a host"
    );

    // One named host runs alone; the default line count is 100, or 10 when
    // following.
    logs(LogsOptions {
        host: "192.0.2.11".to_owned(),
        ..LogsOptions::default()
    })
    .expect("one host");
    logs(LogsOptions {
        host: "192.0.2.11".to_owned(),
        follow: true,
        ..LogsOptions::default()
    })
    .expect("following");
    let calls = fixture.runner.calls();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert!(
        calls[0].starts_with("192.0.2.11: ") && calls[0].contains("--tail 100"),
        "{}",
        calls[0]
    );
    assert!(
        calls[1].contains("--follow") && calls[1].contains("--tail 10 "),
        "{}",
        calls[1]
    );
}

/// A runner whose streams panic: a followed log of a crashed stream must
/// not take the command down with it.
struct PanickingStream;

impl Runner for PanickingStream {
    fn run(&self, _: &Cancel, _: &str, _: &RemoteCommand) -> Result<Output, RunError> {
        Ok(Output::default())
    }
    fn stream(&self, _: &Cancel, _: &str, _: &RemoteCommand, _: &dyn Sink) -> Result<(), RunError> {
        panic!("stream crashed (expected by the test)");
    }
    fn interactive(&self, _: &Cancel, _: &str, _: &str) -> Result<(), RunError> {
        Ok(())
    }
    fn close(&self) {}
}

#[test]
fn a_followed_log_stream_that_panics_is_reported() {
    let mut fixture = plan_fixture(false);
    fixture.deployer.runner = Box::new(PanickingStream);
    let out = Buffer::default();
    let error = fixture
        .deployer
        .logs(
            &Cancel::never(),
            out.writer(),
            LogsOptions {
                follow: true,
                host: "192.0.2.10".to_owned(),
                ..LogsOptions::default()
            },
        )
        .expect_err("a stream panicked");
    assert_eq!(error.message, "log stream failed");
}

#[test]
fn a_line_without_a_newline_is_flushed_once_it_is_too_long() {
    let out = Buffer::default();
    let writer = PrefixWriter::new(
        "web|".to_owned(),
        out.writer(),
        None,
        Secrets::default().redactor(),
    );
    writer.write(&vec![b'x'; 70 << 10]);
    let text = out.text();
    assert!(
        text.starts_with("web|xxxx") && text.ends_with('\n'),
        "{} bytes",
        text.len()
    );
    assert_eq!(text.matches("web|").count(), 1);
    writer.write(b"tail");
    writer.flush();
    assert!(out.text().ends_with("web|tail\n"));
}

#[test]
fn exec_validates_its_command_and_attaches_a_terminal_when_asked() {
    let fixture = plan_fixture(false);
    let out = Buffer::default();
    let cancel = Cancel::never();
    let exec = |argv: &[&str], interactive: bool| {
        fixture.deployer.exec(
            &cancel,
            out.writer(),
            &ExecOptions {
                argv: argv.iter().map(|word| (*word).to_owned()).collect(),
                interactive,
                ..ExecOptions::default()
            },
        )
    };
    assert_eq!(
        exec(&[], false).expect_err("empty").message,
        "a command is required"
    );
    assert_eq!(
        exec(&["echo", "a\u{7}b"], false)
            .expect_err("control character")
            .message,
        "command arguments must not contain control characters"
    );
    assert!(fixture.runner.calls().is_empty());
    exec(&["migrate", "status"], true).expect("interactive");
    let calls = fixture.runner.calls();
    assert_eq!(calls.len(), 1);
    assert!(
        calls[0].contains("--interactive --tty") && calls[0].ends_with("migrate status"),
        "{calls:?}"
    );
}

#[test]
fn shell_opens_the_container_or_a_debug_sidecar() {
    let fixture = plan_fixture(false);
    let cancel = Cancel::never();
    fixture
        .deployer
        .shell(&cancel, "", "", "")
        .expect("shell in the container");
    fixture
        .deployer
        .shell(&cancel, "web", "192.0.2.11", "busybox:1.37")
        .expect("debug shell");
    let error = fixture
        .deployer
        .shell(&cancel, "web", "", "Not An Image!")
        .expect_err("image");
    assert_eq!(
        error.message,
        "--image \"Not An Image!\" is not an image reference"
    );
    let calls = fixture.runner.calls();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert!(
        calls[0].starts_with("192.0.2.10: ") && calls[0].ends_with("\"$c\" sh"),
        "{}",
        calls[0]
    );
    assert!(
        calls[1].starts_with("192.0.2.11: ") && calls[1].contains("busybox:1.37"),
        "{}",
        calls[1]
    );
}

// --- the commands, with a configuration on disk -----------------------------------

struct Project {
    root: tempfile::TempDir,
    runner: FakeRunner,
    local: ProjectLocal,
}

/// A local runner for `forge deploy` with the version resolved from git.
#[derive(Clone)]
struct ProjectLocal(Arc<Mutex<Vec<String>>>);

impl LocalRunner for ProjectLocal {
    fn output(&self, _: &Cancel, _: &Path, argv: &[String]) -> Result<String, String> {
        Ok(if argv.get(1).map(String::as_str) == Some("rev-parse") {
            "abc123def456\n".to_owned()
        } else {
            String::new()
        })
    }
    fn run(
        &self,
        _: &Cancel,
        _: &Path,
        argv: &[String],
        _: Option<&[u8]>,
        _: &[(String, String)],
    ) -> Result<(), String> {
        self.0.lock().expect("lock").push(argv.join(" "));
        Ok(())
    }
}

fn project() -> Project {
    // The fixture's runner knows the hosts' containers and images.
    let fixture = plan_fixture(false);
    let root = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(root.path().join("config")).expect("config directory");
    std::fs::write(root.path().join(DEFAULT_CONFIG_PATH), VALID_CONFIG).expect("config");
    std::fs::create_dir_all(root.path().join(".forge")).expect("state directory");
    let secrets: String = PLAN_SECRETS
        .iter()
        .map(|(name, value)| format!("{name}={value}\n"))
        .collect();
    let path = root.path().join(".forge/secrets");
    std::fs::write(&path, secrets).expect("secrets");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    Project {
        root,
        runner: fixture.runner.clone(),
        local: ProjectLocal(Arc::default()),
    }
}

impl Project {
    fn factory(&self) -> Factory {
        let runner = self.runner.clone();
        let local = self.local.clone();
        Factory {
            runner: Box::new(move |_: &Config| Box::new(runner.clone()) as Box<dyn Runner>),
            local: Box::new(move || Box::new(local.clone()) as Box<dyn LocalRunner>),
        }
    }

    fn deploy(
        &self,
        action: DeployAction,
        common: &Common,
    ) -> (Result<(), super::DeployError>, String, String) {
        let (io, out, err) = io();
        let result = run_deploy(
            action,
            common,
            self.root.path(),
            &io,
            &self.factory(),
            &Cancel::never(),
        );
        (result, out.text(), err.text())
    }
}

fn flags(dry_run: bool, json: bool) -> Common {
    Common {
        dry_run,
        json,
        ..common(Path::new("."))
    }
}

#[test]
fn deploy_prints_a_summary_and_resolves_the_version_from_git() {
    let project = project();
    let (result, out, _) = project.deploy(
        DeployAction::Deploy {
            version: None,
            skip_build: false,
        },
        &flags(false, false),
    );
    result.expect("deploy");
    assert!(out.starts_with("deployed abc123def456 ("), "{out}");
    let builds = project.local.0.lock().expect("lock").clone();
    assert!(
        builds
            .iter()
            .any(|line| line.contains("--tag registry.example.com/acme/shop:abc123def456")),
        "{builds:?}"
    );

    let (result, out, _) = project.deploy(
        DeployAction::Deploy {
            version: Some("v9".to_owned()),
            skip_build: true,
        },
        &flags(false, true),
    );
    result.expect("json deploy");
    let report: serde_json::Value = serde_json::from_str(out.trim()).expect("one JSON line");
    assert_eq!(
        (report["ok"].as_bool(), report["version"].as_str()),
        (Some(true), Some("v9"))
    );
}

#[test]
fn a_dry_run_prints_the_plan_and_says_nothing_was_changed() {
    let project = project();
    let (result, out, err) = project.deploy(DeployAction::Prune, &flags(true, false));
    result.expect("dry run");
    assert!(out.contains("# lock acquired (prune)"), "{out}");
    assert!(
        out.trim_end().ends_with(')') && out.contains("dry run: nothing was changed ("),
        "{out}"
    );
    assert_eq!(err, "", "the plan is the command's output");
    // With --json the plan moves to stderr and stdout carries the report.
    let (result, out, err) = project.deploy(DeployAction::Prune, &flags(true, true));
    result.expect("json dry run");
    assert!(
        serde_json::from_str::<serde_json::Value>(out.trim()).is_ok(),
        "{out}"
    );
    assert!(err.contains("# lock acquired (prune)"), "{err}");
}

#[test]
fn setup_rollback_and_prune_print_their_own_summaries() {
    let project = project();
    let plain = flags(false, false);
    let (result, out, _) = project.deploy(DeployAction::Setup, &plain);
    result.expect("setup");
    assert!(out.starts_with("setup complete ("), "{out}");
    let (result, out, _) = project.deploy(DeployAction::Prune, &plain);
    result.expect("prune");
    assert!(out.starts_with("prune complete ("), "{out}");
    let (result, out, _) = project.deploy(
        DeployAction::Rollback {
            version: "v2".to_owned(),
        },
        &plain,
    );
    result.expect("rollback");
    assert!(out.starts_with("rolled back to v2 ("), "{out}");
    let (result, _, _) = project.deploy(
        DeployAction::Rollback {
            version: "v7".to_owned(),
        },
        &plain,
    );
    assert!(
        result
            .expect_err("not retained")
            .message
            .contains("is not retained")
    );
}

#[test]
fn warnings_are_printed_before_the_summary() {
    let project = project();
    project.runner.fail_with(|host, command| {
        command
            .script
            .starts_with("docker image ls")
            .then(|| remote(host, 1, "refused"))
    });
    let (result, out, _) = project.deploy(DeployAction::Prune, &flags(false, false));
    result.expect("prune");
    assert!(
        out.starts_with("warning: 192.0.2.10: listing images failed"),
        "{out}"
    );
    assert!(out.contains("prune complete ("), "{out}");
}

#[test]
fn init_and_details_refuse_flags_that_do_not_apply() {
    let project = project();
    let (result, _, _) = project.deploy(DeployAction::Init { service: None }, &flags(true, false));
    assert_eq!(
        result.expect_err("dry run").message,
        "usage: forge deploy init [--service NAME]"
    );
    let (result, _, _) = project.deploy(DeployAction::Init { service: None }, &flags(false, true));
    assert!(result.is_err());
    let (result, _, _) = project.deploy(DeployAction::Details, &flags(true, false));
    assert_eq!(
        result.expect_err("dry run").message,
        "usage: forge deploy details [--json]"
    );
    let (result, _, _) =
        project.deploy(DeployAction::Maintenance { on: true }, &flags(false, true));
    assert!(
        result
            .expect_err("json")
            .message
            .starts_with("usage: forge deploy maintenance")
    );
}

#[test]
fn a_secrets_file_that_others_can_read_is_named_as_given() {
    let project = project();
    let path = project.root.path().join(".forge/secrets");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod");
    let (result, _, _) = project.deploy(DeployAction::Setup, &flags(false, false));
    let message = result.expect_err("mode").message;
    assert!(
        message.starts_with("secrets file .forge/secrets is accessible by other users"),
        "{message}"
    );
    assert!(
        !message.contains(project.root.path().to_str().expect("utf-8 path")),
        "paths are shown as given: {message}"
    );
}

#[test]
fn lock_commands_report_status_acquire_and_release() {
    let project = project();
    let lock = |action: LockAction, force: bool, json: bool| {
        project.deploy(
            DeployAction::Lock {
                action,
                message: "maintenance window".to_owned(),
                force,
            },
            &flags(false, json),
        )
    };
    let (result, out, _) = lock(LockAction::Status, false, false);
    result.expect("status");
    assert_eq!(out, "not locked\n");
    let (result, out, _) = lock(LockAction::Status, false, true);
    result.expect("json status");
    let report: serde_json::Value = serde_json::from_str(out.trim()).expect("JSON");
    assert_eq!(report["locked"], false);

    project.runner.answers.lock().expect("lock").push((
        "if [ -d .forge/shop/lock ]".to_owned(),
        LOCK_RECORD.to_owned(),
    ));
    let (result, out, _) = lock(LockAction::Status, false, false);
    result.expect("status");
    assert_eq!(
        out.lines().next(),
        Some("192.0.2.10: held by bob@ci since 2026-10-19T11:00:00Z (window)")
    );
    let (result, out, _) = lock(LockAction::Status, false, true);
    result.expect("json status");
    let report: serde_json::Value = serde_json::from_str(out.trim()).expect("JSON");
    assert_eq!(report["locked"], true);
    assert_eq!(report["locks"]["192.0.2.10"]["holder"], "bob@ci");

    let (result, out, _) = lock(LockAction::Acquire, false, false);
    result.expect("acquire");
    assert_eq!(out, "lock acquired\n");
    let (result, out, _) = lock(LockAction::Release, true, false);
    result.expect("release");
    assert_eq!(out, "lock released\n");
    let (result, _, _) = lock(LockAction::Release, false, false);
    assert_eq!(result.expect_err("held by bob").exit_code(), EXIT_LOCK_HELD);
}

#[test]
fn details_print_hosts_containers_proxy_lock_and_history() {
    let project = project();
    {
        let mut answers = project.runner.answers.lock().expect("lock");
        answers.push((
            "docker exec forge-proxy".to_owned(),
            PROXY_STATUS.to_owned(),
        ));
        answers.push((
            "if [ -d .forge/shop/lock ]".to_owned(),
            LOCK_RECORD.to_owned(),
        ));
        answers.push((
            "tail -n 5".to_owned(),
            "{\"action\":\"deploy\"}\n".to_owned(),
        ));
    }
    let (result, out, _) = project.deploy(DeployAction::Details, &flags(false, false));
    result.expect("details");
    for want in [
        "192.0.2.10 (web)\n",
        "192.0.2.11 (web, jobs)\n",
        "  web      running    v2                   shop-web-v2-bbbbbb\n",
        "  proxy    shop -> shop-web-v2-bbbbbb:8080 hosts=shop.example.com tls=true maintenance=false\n",
        "  lock     held by bob@ci since 2026-10-19T11:00:00Z (window)\n",
        "  history  {\"action\":\"deploy\"}\n",
    ] {
        assert!(out.contains(want), "{want:?} missing from\n{out}");
    }

    // A failing host is printed with its error, and the command fails.
    project.runner.fail_with(|host, command| {
        (host == "192.0.2.11" && command.script.starts_with("docker ps --all"))
            .then(|| remote(host, 1, "docker down"))
    });
    let (result, out, _) = project.deploy(DeployAction::Details, &flags(false, false));
    assert!(result.is_err());
    assert!(out.contains("192.0.2.11 (web, jobs)\n  error: "), "{out}");
    let (result, out, _) = project.deploy(DeployAction::Details, &flags(false, true));
    assert!(result.is_err());
    let report: serde_json::Value = serde_json::from_str(out.trim()).expect("JSON");
    assert_eq!(report["service"], "shop");
    assert_eq!(report["hosts"].as_array().map(Vec::len), Some(2));
}

#[test]
fn maintenance_runs_through_the_command() {
    let project = project();
    let (result, out, err) =
        project.deploy(DeployAction::Maintenance { on: true }, &flags(false, false));
    result.expect("maintenance");
    assert_eq!(out, "");
    assert!(err.contains("maintenance true"), "{err}");
}

#[test]
fn logs_and_app_commands_run_on_the_configured_hosts() {
    let project = project();
    let (io, out, _) = io();
    let factory = project.factory();
    let cancel = Cancel::never();
    let root = project.root.path();
    let remote_flags = flags(false, false);

    run_logs(
        LogsOptions::default(),
        &remote_flags,
        root,
        &io,
        &factory,
        &cancel,
    )
    .expect("logs");
    let calls = project.runner.calls();
    assert_eq!(calls.len(), 2, "both web hosts: {calls:?}");

    let exec = AppAction::Exec(ExecOptions {
        role: "web".to_owned(),
        host: "192.0.2.10".to_owned(),
        argv: vec!["migrate".to_owned(), "status".to_owned()],
        ..ExecOptions::default()
    });
    run_app(exec, &remote_flags, root, &io, &factory, &cancel).expect("exec");
    let shell = AppAction::Shell {
        role: String::new(),
        host: String::new(),
        image: String::new(),
    };
    run_app(shell, &remote_flags, root, &io, &factory, &cancel).expect("shell");
    let calls = project.runner.calls();
    assert_eq!(calls.len(), 4, "{calls:?}");
    assert_eq!(out.text(), "", "the fake streams print nothing");

    // A missing configuration is reported as given.
    let empty = tempfile::tempdir().expect("tempdir");
    let error = run_logs(
        LogsOptions::default(),
        &remote_flags,
        empty.path(),
        &io,
        &factory,
        &cancel,
    )
    .expect_err("no configuration");
    assert!(
        error
            .message
            .starts_with("read deploy config: open config/deploy.yml"),
        "{error}"
    );
}

#[test]
fn the_system_factory_builds_the_ssh_and_local_runners() {
    let factory = Factory::system();
    let config = Config::parse(VALID_CONFIG.as_bytes()).expect("config");
    let runner = (factory.runner)(&config);
    // A runner that never connected has nothing to close.
    runner.close();
    let local = (factory.local)();
    let output = local
        .output(&Cancel::never(), &PathBuf::from("."), &["true".to_owned()])
        .expect("true runs");
    assert_eq!(output, "");
    // The process streams are the process's own.
    let _ = Io::process();
}
