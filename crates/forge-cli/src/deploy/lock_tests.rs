//! The deploy lock scripts run for real under `sh`, one directory per host
//! standing in for the SSH user's home (Forge-Go's `lock_test.go`): own and
//! foreign release, stale takeover, the deterministic takeover race (a shim
//! `find` on `PATH` runs a competitor between the staleness check and the
//! claim), noclobber on the temporary record, and a takeover whose stderr
//! rejects writes.

use std::collections::BTreeMap;
use std::fs::{File, FileTimes};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

use super::commands::{RemoteCommand, STALE_LOCK_NOTICE};
use super::deployer::{Deployer, Parts};
use super::errors::{EXIT_LOCK_HELD, LOCK_HELD_EXIT_STATUS, RemoteError};
use super::runner::{Cancel, LocalRunner, Output, RunError, Runner, Sink};
use super::secrets::Secrets;
use super::tests::{Buffer, PLAN_SECRETS, VALID_CONFIG, parse, plan_epoch};

/// Runs each host's commands with `sh` in its own directory, the way SSH
/// starts them in the home directory, so the lock scripts run for real
/// without sshd.
#[derive(Clone, Default)]
struct ShellRunner {
    dirs: BTreeMap<String, PathBuf>,
    /// Added to the environment of every command.
    env: Vec<(String, String)>,
}

impl Runner for ShellRunner {
    fn run(
        &self,
        _cancel: &Cancel,
        host: &str,
        command: &RemoteCommand,
    ) -> Result<Output, RunError> {
        let dir = self
            .dirs
            .get(host)
            .ok_or_else(|| RunError::Other(format!("unknown host {host}")))?;
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(&command.script)
            .current_dir(dir)
            .envs(self.env.iter().map(|(key, value)| (key, value)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| RunError::Other(error.to_string()))?;
        if let Some(mut stdin) = child.stdin.take() {
            // A script that does not read stdin may exit first.
            let _ = stdin.write_all(command.stdin.as_deref().unwrap_or_default());
        }
        let output = child
            .wait_with_output()
            .map_err(|error| RunError::Other(error.to_string()))?;
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        if output.status.success() {
            return Ok(Output { stdout, stderr });
        }
        Err(RunError::Remote(RemoteError {
            host: host.to_owned(),
            exit_code: output.status.code().unwrap_or(-1),
            stderr,
            stdout,
        }))
    }

    fn stream(
        &self,
        _cancel: &Cancel,
        _host: &str,
        _command: &RemoteCommand,
        _out: &dyn Sink,
    ) -> Result<(), RunError> {
        Ok(())
    }

    fn interactive(&self, _cancel: &Cancel, _host: &str, _script: &str) -> Result<(), RunError> {
        Ok(())
    }

    fn close(&self) {}
}

struct NoLocal;

impl LocalRunner for NoLocal {
    fn output(&self, _cancel: &Cancel, _dir: &Path, argv: &[String]) -> Result<String, String> {
        Err(format!("unexpected local command {argv:?}"))
    }

    fn run(
        &self,
        _cancel: &Cancel,
        _dir: &Path,
        argv: &[String],
        _stdin: Option<&[u8]>,
        _env: &[(String, String)],
    ) -> Result<(), String> {
        Err(format!("unexpected local command {argv:?}"))
    }
}

struct LockFixture {
    deployer: Deployer,
    runner: ShellRunner,
    log: Buffer,
    /// The home of the first host.
    home: PathBuf,
    hosts: Vec<String>,
    _dirs: Vec<tempfile::TempDir>,
    _root: tempfile::TempDir,
}

/// Forge-Go's `lockFixture`: the plan configuration (hosts .10, .11 and
/// .20), each host in its own temporary home.
fn lock_fixture(performer: &str) -> LockFixture {
    let config = parse(VALID_CONFIG);
    let hosts = config.all_hosts();
    let mut dirs = Vec::new();
    let mut runner = ShellRunner::default();
    for host in &hosts {
        let dir = tempfile::tempdir().expect("tempdir");
        runner.dirs.insert(host.clone(), dir.path().to_owned());
        dirs.push(dir);
    }
    let home = runner.dirs[&hosts[0]].clone();
    let log = Buffer::default();
    let root = tempfile::tempdir().expect("tempdir");
    let deployer = Deployer::new(Parts {
        config,
        secrets: Secrets::from_values(PLAN_SECRETS),
        runner: Box::new(runner.clone()),
        local: Box::new(NoLocal),
        root: root.path().to_owned(),
        log: log.writer(),
        performer: Some(performer.to_owned()),
        now: Some(Box::new(plan_epoch)),
        suffix: None,
        dry_run: false,
    });
    deployer.prepare().expect("prepare");
    LockFixture {
        deployer,
        runner,
        log,
        home,
        hosts,
        _dirs: dirs,
        _root: root,
    }
}

fn state_dir(home: &Path) -> PathBuf {
    home.join(".forge").join("shop")
}

fn lock_dir(home: &Path) -> PathBuf {
    state_dir(home).join("lock")
}

fn lock_exists(home: &Path) -> bool {
    lock_dir(home).exists()
}

/// Sets the modification time of `path` five minutes back, past the
/// one-minute grace period of a lock without a record.
fn age(path: &Path) {
    let old = SystemTime::now() - Duration::from_secs(300);
    File::open(path)
        .expect("open")
        .set_times(FileTimes::new().set_accessed(old).set_modified(old))
        .expect("set times");
}

fn held_exit(result: &Result<Output, RunError>) -> bool {
    matches!(result, Err(RunError::Remote(remote)) if remote.exit_code == LOCK_HELD_EXIT_STATUS)
}

/// `lock/info` holds `want` and no temporary record is left in the state
/// directory.
fn assert_lock_record(home: &Path, want: &str) {
    let info = std::fs::read_to_string(lock_dir(home).join("info"));
    assert_eq!(
        info.as_deref().ok(),
        Some(want),
        "lock/info = {info:?}; want the winner's record"
    );
    assert_no_temporary_record(home);
}

fn assert_no_temporary_record(home: &Path) {
    let leftovers: Vec<_> = std::fs::read_dir(state_dir(home))
        .expect("state dir")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("lock.tmp."))
        .collect();
    assert!(
        leftovers.is_empty(),
        "temporary record left behind: {leftovers:?}"
    );
}

#[test]
fn release_removes_only_its_own_lock() {
    let fixture = lock_fixture("alice@laptop");
    let (deployer, cancel) = (&fixture.deployer, Cancel::never());
    let guard = deployer
        .acquire(&cancel, &fixture.hosts, "deploy")
        .expect("acquire");
    assert!(lock_exists(&fixture.home));
    // Someone force-releases and takes the lock while the deploy runs.
    deployer.lock_release(&cancel, true).expect("force release");
    let bob = "{\"holder\":\"bob@ci\",\"message\":\"maintenance\",\"acquired_at\":\"2026-10-20T10:00:00Z\"}\n";
    fixture
        .runner
        .run(
            &cancel,
            &fixture.hosts[0],
            &deployer.cmds.acquire_lock(bob.as_bytes().to_vec()),
        )
        .expect("bob acquires");
    drop(guard);
    assert!(
        lock_exists(&fixture.home),
        "the deferred release deleted a lock somebody else took"
    );
    let log = fixture.log.text();
    assert!(
        log.contains(
            "192.0.2.10: the deploy lock is now held by someone else and was left in place"
        ),
        "no warning about the foreign lock:\n{log}"
    );
    assert!(
        log.contains("192.0.2.11: the deploy lock was already released (by --force?)"),
        "no note about the released lock:\n{log}"
    );
    let locks = deployer.lock_status(&cancel).expect("status");
    assert_eq!(locks[&fixture.hosts[0]].holder, "bob@ci", "{locks:?}");
    let error = deployer
        .lock_release(&cancel, false)
        .expect_err("released bob's lock without --force");
    assert_eq!(error.exit_code(), EXIT_LOCK_HELD, "{error}");
    assert_eq!(
        error.message,
        "192.0.2.10: lock is held by bob@ci (maintenance); pass --force to release it anyway"
    );
    assert!(lock_exists(&fixture.home), "bob's lock is gone");
}

#[test]
fn release_removes_own_lock() {
    let fixture = lock_fixture("alice@laptop");
    let (deployer, cancel) = (&fixture.deployer, Cancel::never());
    let guard = deployer
        .acquire(&cancel, &fixture.hosts, "deploy")
        .expect("acquire");
    drop(guard);
    assert!(!lock_exists(&fixture.home), "own lock not released");
    deployer
        .lock_acquire(&cancel, "window")
        .expect("lock acquire");
    assert!(lock_exists(&fixture.home));
    deployer
        .lock_release(&cancel, false)
        .expect("release of own lock");
    assert!(!lock_exists(&fixture.home), "own lock not released");
    assert!(deployer.lock_status(&cancel).expect("status").is_empty());
}

#[test]
fn lock_without_record_is_recoverable() {
    let fixture = lock_fixture("alice@laptop");
    let (deployer, cancel) = (&fixture.deployer, Cancel::never());
    let lock = lock_dir(&fixture.home);
    std::fs::create_dir_all(&lock).expect("mkdir lock");

    // Fresh: an acquire in progress may still write its record.
    let error = deployer
        .acquire(&cancel, &fixture.hosts, "deploy")
        .err()
        .expect("a fresh record-less lock is held");
    assert_eq!(error.exit_code(), EXIT_LOCK_HELD, "{error}");
    assert!(error.message.contains("without a record"), "{error}");
    assert!(
        lock_exists(&fixture.home),
        "a fresh record-less lock was removed"
    );
    let locks = deployer.lock_status(&cancel).expect("status");
    assert_eq!(locks.len(), 1, "{locks:?}");
    let info = &locks[&fixture.hosts[0]];
    assert_eq!(info.holder, "unknown");
    assert!(info.message.contains("without a record"), "{info:?}");
    let error = deployer
        .lock_release(&cancel, false)
        .expect_err("release without --force of a record-less lock");
    assert_eq!(error.exit_code(), EXIT_LOCK_HELD, "{error}");

    // Older than the grace period: recovered without --force, and reported.
    age(&lock);
    let guard = deployer
        .acquire(&cancel, &fixture.hosts, "deploy")
        .expect("stale record-less lock taken over");
    let log = fixture.log.text();
    assert!(
        log.contains(&format!("192.0.2.10: {STALE_LOCK_NOTICE}")),
        "recovery not reported:\n{log}"
    );
    let locks = deployer.lock_status(&cancel).expect("status");
    assert_eq!(locks[&fixture.hosts[0]].holder, "alice@laptop");
    drop(guard);
    assert!(!lock_exists(&fixture.home), "lock not released");
}

/// A lock with a record, or with an empty info file, is never taken over,
/// however old. The script runs on one host directly, so the other hosts'
/// locks cannot stand in for the answer.
#[test]
fn stale_recovery_leaves_a_lock_with_an_info_file_alone() {
    for (name, content) in [
        (
            "record",
            "{\"holder\":\"bob@ci\",\"message\":\"window\",\"acquired_at\":\"2026-10-20T10:00:00Z\"}\n",
        ),
        ("empty info", ""),
    ] {
        let fixture = lock_fixture("alice@laptop");
        let lock = lock_dir(&fixture.home);
        let info = lock.join("info");
        std::fs::create_dir_all(&lock).expect("mkdir lock");
        std::fs::write(&info, content).expect("write info");
        age(&info);
        age(&lock);
        let result = fixture.runner.run(
            &Cancel::never(),
            &fixture.hosts[0],
            &fixture
                .deployer
                .cmds
                .acquire_lock(b"{\"holder\":\"alice@laptop\"}\n".to_vec()),
        );
        assert!(
            held_exit(&result),
            "{name}: acquire over an old lock with an info file = {result:?}, want exit {LOCK_HELD_EXIT_STATUS}"
        );
        assert_eq!(
            std::fs::read_to_string(&info).expect("info").as_str(),
            content,
            "{name}: info changed"
        );
        assert_no_temporary_record(&fixture.home);
    }
}

#[test]
fn release_of_a_force_released_lock_is_reported_as_gone() {
    let fixture = lock_fixture("alice@laptop");
    let (deployer, cancel) = (&fixture.deployer, Cancel::never());
    let guard = deployer
        .acquire(&cancel, &fixture.hosts, "deploy")
        .expect("acquire");
    deployer.lock_release(&cancel, true).expect("force release");
    drop(guard);
    assert!(!lock_exists(&fixture.home), "lock reappeared");
    let log = fixture.log.text();
    assert!(
        log.contains("already released") && !log.contains("someone else"),
        "log should say the lock was already released:\n{log}"
    );
    // A non-force release of a lock that vanished between the status read
    // and the release is not an error either.
    let result = fixture.runner.run(
        &cancel,
        &fixture.hosts[0],
        &deployer.cmds.release_lock(b"{}\n".to_vec()),
    );
    assert!(
        matches!(&result, Err(RunError::Remote(remote)) if remote.exit_code == super::errors::LOCK_GONE_EXIT_STATUS),
        "{result:?}"
    );
}

/// Two acquirers race for an aged lock without an info file: exactly one
/// may win, its record must be the one in `lock/info`, and no temporary
/// file may stay behind. Every round starts from the same aged, empty
/// directory.
#[test]
fn concurrent_stale_recovery_has_one_winner() {
    let fixture = lock_fixture("alice@laptop");
    let host = &fixture.hosts[0];
    let lock = lock_dir(&fixture.home);
    let records = ["{\"holder\":\"p0\"}\n", "{\"holder\":\"p1\"}\n"];
    let commands = records.map(|record| {
        fixture
            .deployer
            .cmds
            .acquire_lock(record.as_bytes().to_vec())
    });
    for round in 0..200 {
        if lock.exists() {
            std::fs::remove_dir_all(&lock).expect("remove lock");
        }
        std::fs::create_dir_all(&lock).expect("mkdir lock");
        age(&lock);
        let wins: Vec<bool> = std::thread::scope(|scope| {
            let handles: Vec<_> = commands
                .iter()
                .map(|command| {
                    let runner = &fixture.runner;
                    scope.spawn(move || runner.run(&Cancel::never(), host, command).is_ok())
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().expect("acquirer"))
                .collect()
        });
        let winners: Vec<usize> = (0..2).filter(|&index| wins[index]).collect();
        assert_eq!(
            winners.len(),
            1,
            "round {round}: winners {winners:?}, want exactly one"
        );
        assert_lock_record(&fixture.home, records[winners[0]]);
    }
}

/// The interleaving is forced, not hoped for: acquirer A sees an aged lock
/// without info and asks `find` whether it is stale. A shim `find` first
/// runs a competing acquirer B to completion on the same directory and then
/// answers "stale" to A (the competitor's own `find` is the real one, and
/// the real one would now see a fresh directory, so the shim answers for
/// it). B wins; A's claim must lose. A script that overwrites `info`
/// instead of linking it (`cp`) lets A win as well, and this test fails.
#[test]
fn stale_recovery_interleaved_race_has_one_winner() {
    let fixture = lock_fixture("alice@laptop");
    let host = &fixture.hosts[0];
    let lock = lock_dir(&fixture.home);
    std::fs::create_dir_all(&lock).expect("mkdir lock");
    age(&lock);

    let record_a = "{\"holder\":\"a@laptop\"}\n";
    let record_b = "{\"holder\":\"b@ci\"}\n";
    let shims = tempfile::tempdir().expect("tempdir");
    let write = |name: &str, content: &str| {
        let path = shims.path().join(name);
        std::fs::write(&path, content).expect("write shim file");
        path
    };
    let competitor = write(
        "competitor.sh",
        &fixture
            .deployer
            .cmds
            .acquire_lock(record_b.as_bytes().to_vec())
            .script,
    );
    let record_file = write("record", record_b);
    let status = shims.path().join("competitor.status");
    let path = std::env::var_os("PATH").unwrap_or_default();
    let Some(real_find) = std::env::split_paths(&path)
        .map(|dir| dir.join("find"))
        .find(|candidate| candidate.is_file())
    else {
        eprintln!("skipped: no find on PATH");
        return;
    };
    let shim = write(
        "find",
        "#!/bin/sh\n\
         if [ -n \"$FORGE_SHIM_ACTIVE\" ]; then\n\
         \texec \"$FORGE_REAL_FIND\" \"$@\"\n\
         fi\n\
         FORGE_SHIM_ACTIVE=1 sh \"$FORGE_SHIM_SCRIPT\" < \"$FORGE_SHIM_RECORD\" >/dev/null 2>&1\n\
         echo $? > \"$FORGE_SHIM_STATUS\"\n\
         echo \"$1\"\n",
    );
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o700))
            .expect("chmod shim");
    }
    let mut search = vec![shims.path().to_owned()];
    search.extend(std::env::split_paths(&path));
    let runner = ShellRunner {
        dirs: fixture.runner.dirs.clone(),
        env: [
            (
                "PATH",
                std::env::join_paths(search)
                    .expect("PATH")
                    .to_string_lossy()
                    .into_owned(),
            ),
            ("FORGE_SHIM_SCRIPT", competitor.display().to_string()),
            ("FORGE_SHIM_RECORD", record_file.display().to_string()),
            ("FORGE_SHIM_STATUS", status.display().to_string()),
            ("FORGE_REAL_FIND", real_find.display().to_string()),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect(),
    };

    let result = runner.run(
        &Cancel::never(),
        host,
        &fixture
            .deployer
            .cmds
            .acquire_lock(record_a.as_bytes().to_vec()),
    );
    let competitor_status = std::fs::read_to_string(&status);
    assert_eq!(
        competitor_status.as_deref().map(str::trim).ok(),
        Some("0"),
        "the competing acquire did not win ({competitor_status:?}); the shim did not run or the competitor lost"
    );
    assert!(
        held_exit(&result),
        "acquire that lost the race = {result:?}, want exit {LOCK_HELD_EXIT_STATUS}"
    );
    if let Err(RunError::Remote(remote)) = &result {
        assert_eq!(
            remote.stdout, record_b,
            "the loser reported the wrong record"
        );
    }
    assert_lock_record(&fixture.home, record_b);
}

/// The temporary file is opened under noclobber: a name that survived an
/// earlier process with the same PID, still linked to `lock/info`, must not
/// be truncated. The script's `$$` is replaced by a fixed number to stage
/// that.
#[test]
fn acquire_never_truncates_a_linked_temporary_file() {
    let fixture = lock_fixture("alice@laptop");
    let lock = lock_dir(&fixture.home);
    std::fs::create_dir_all(&lock).expect("mkdir lock");
    let bob = "{\"holder\":\"bob@ci\",\"message\":\"window\"}\n";
    let info = lock.join("info");
    std::fs::write(&info, bob).expect("write info");
    if let Err(error) = std::fs::hard_link(&info, state_dir(&fixture.home).join("lock.tmp.4242")) {
        eprintln!("skipped: no hard links here: {error}");
        return;
    }
    let mut command = fixture
        .deployer
        .cmds
        .acquire_lock(b"{\"holder\":\"alice@laptop\"}\n".to_vec());
    assert!(command.script.contains("set -C; "), "{}", command.script);
    command.script = command.script.replace("$$", "4242");
    let result = fixture
        .runner
        .run(&Cancel::never(), &fixture.hosts[0], &command);
    assert!(
        result.is_err(),
        "acquire succeeded over a lock that has a record"
    );
    assert_eq!(
        std::fs::read_to_string(&info).expect("info"),
        bob,
        "info was truncated or replaced"
    );
}

/// A stderr that rejects writes must not turn a won takeover into a held
/// lock: the notice is printed after the win is decided.
#[test]
fn stale_takeover_survives_a_failing_stderr() {
    let full = match std::fs::OpenOptions::new().write(true).open("/dev/full") {
        Ok(full) => full,
        Err(error) => {
            eprintln!("skipped: no /dev/full: {error}");
            return;
        }
    };
    let fixture = lock_fixture("alice@laptop");
    let lock = lock_dir(&fixture.home);
    std::fs::create_dir_all(&lock).expect("mkdir lock");
    age(&lock);
    let record = "{\"holder\":\"alice@laptop\"}\n";
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(
            &fixture
                .deployer
                .cmds
                .acquire_lock(record.as_bytes().to_vec())
                .script,
        )
        .current_dir(&fixture.home)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::from(full))
        .spawn()
        .expect("sh");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(record.as_bytes())
        .expect("write record");
    let status = child.wait().expect("wait");
    assert!(status.success(), "takeover with a failing stderr: {status}");
    assert_lock_record(&fixture.home, record);
}
