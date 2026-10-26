//! The system runners against stand-in programs: an `ssh` shell script that
//! answers by the command it is asked to run, and real local commands.
//! Nothing here needs a network, an SSH server or Docker.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::commands::RemoteCommand;
use super::config::Ssh;
use super::errors::RemoteError;
use super::runner::{
    Cancel, DryLocal, DryRunner, ExecLocal, LocalRunner, RunError, Runner, Sink, SshRunner,
};
use super::secrets::Secrets;
use super::tests::{Buffer, write_executable};

/// A fake `ssh`: records its arguments to `log`, then behaves by the last
/// argument, which is the quoted remote script (or the host for `-O exit`).
fn fake_ssh(directory: &Path) -> (PathBuf, PathBuf) {
    let log = directory.join("ssh.log");
    let script = directory.join("ssh");
    let body = format!(
        r#"#!/bin/sh
printf '%s\n' "$*" >> {log}
for last; do :; done
case "$last" in
  *say-hello*) echo "hello"; echo "careful" >&2; exit 0;;
  *fail-seven*) echo "partial output"; echo "boom" >&2; exit 7;;
  *refuse-connection*) echo "ssh: connect to host refused" >&2; exit 255;;
  *die-by-signal*) kill -9 $$;;
  *copy-stdin*) cat; exit 0;;
  *wait-for-stdin*) exec cat > /dev/null;;
  *sleep-forever*) exec sleep 30;;
  *stream-text*) echo "line one"; echo "line two" >&2; exit 0;;
  *stream-fail*) echo "out"; echo "stream went wrong" >&2; exit 3;;
esac
exit 0
"#,
        log = log.display()
    );
    write_executable(&script, &body);
    (script, log)
}

fn runner_with(binary: &Path) -> SshRunner {
    let mut runner = SshRunner::new(&Ssh {
        user: "deploy".to_owned(),
        port: 2222,
        proxy_jump: "ops@bastion.example.com".to_owned(),
    });
    runner.binary = binary.to_path_buf();
    runner
}

fn script(text: &str) -> RemoteCommand {
    RemoteCommand {
        script: text.to_owned(),
        ..RemoteCommand::default()
    }
}

#[derive(Default)]
struct Collect(Mutex<Vec<u8>>);

impl Sink for Collect {
    fn write(&self, data: &[u8]) {
        self.0.lock().expect("lock").extend_from_slice(data);
    }
}

impl Collect {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().expect("lock")).into_owned()
    }
}

fn remote_of(error: RunError) -> RemoteError {
    match error {
        RunError::Remote(remote) => remote,
        RunError::Other(message) => panic!("expected a remote error, got {message}"),
    }
}

#[test]
fn run_error_displays_the_remote_error_or_the_message() {
    let remote = RunError::Remote(RemoteError {
        host: "h".to_owned(),
        exit_code: 2,
        stderr: "bad".to_owned(),
        stdout: String::new(),
    });
    assert_eq!(
        remote.to_string(),
        "h: remote command exited with status 2: bad"
    );
    assert_eq!(
        RunError::Other("ssh missing".to_owned()).to_string(),
        "ssh missing"
    );
}

#[test]
fn run_captures_output_and_classifies_failures() {
    let directory = tempfile::tempdir().expect("tempdir");
    let (binary, log) = fake_ssh(directory.path());
    let runner = runner_with(&binary);
    let cancel = Cancel::never();

    let output = runner
        .run(&cancel, "192.0.2.10", &script("say-hello"))
        .expect("success");
    assert_eq!(output.stdout, "hello\n");
    assert_eq!(output.stderr, "careful\n");

    // A non-zero exit keeps the stdout it printed (a held lock answers with
    // its record there) and the stderr.
    let remote = remote_of(
        runner
            .run(&cancel, "192.0.2.10", &script("fail-seven"))
            .expect_err("exit 7"),
    );
    assert_eq!(remote.exit_code, 7);
    assert_eq!(remote.stdout, "partial output\n");
    assert_eq!(remote.stderr, "boom\n");

    // ssh's own failure (255) is not a remote exit status.
    let error = runner
        .run(&cancel, "192.0.2.10", &script("refuse-connection"))
        .expect_err("ssh failed");
    assert_eq!(
        error,
        RunError::Other("192.0.2.10: ssh failed: ssh: connect to host refused".to_owned())
    );

    // A process killed by a signal has no exit code.
    let remote = remote_of(
        runner
            .run(&cancel, "192.0.2.10", &script("die-by-signal"))
            .expect_err("killed"),
    );
    assert_eq!(remote.exit_code, -1);

    // Secrets travel on stdin.
    let command = RemoteCommand {
        script: "copy-stdin".to_owned(),
        stdin: Some(b"registry-secret\n".to_vec()),
        ..RemoteCommand::default()
    };
    let output = runner.run(&cancel, "192.0.2.10", &command).expect("stdin");
    assert_eq!(output.stdout, "registry-secret\n");

    // The arguments are Forge-Go's, and the script is quoted as one word.
    let calls = std::fs::read_to_string(&log).expect("log");
    let first = calls.lines().next().expect("a call");
    for want in [
        "-o ControlMaster=auto",
        "-o BatchMode=yes -T",
        "-l deploy",
        "-p 2222",
        "-J ops@bastion.example.com",
        "-- 192.0.2.10 sh -c say-hello",
    ] {
        assert!(first.contains(want), "{want:?} missing from {first}");
    }
    runner.close();
}

#[test]
fn run_reports_a_program_that_cannot_start_and_stops_a_cancelled_command() {
    let directory = tempfile::tempdir().expect("tempdir");
    let mut missing = runner_with(&directory.path().join("no-such-ssh"));
    missing.config_file = Some(directory.path().join("ssh_config"));
    let error = missing
        .run(&Cancel::never(), "192.0.2.10", &script("say-hello"))
        .expect_err("no program");
    let RunError::Other(message) = error else {
        panic!("expected a start failure");
    };
    assert!(message.contains("no-such-ssh"), "{message}");
    for result in [
        missing
            .stream(
                &Cancel::never(),
                "192.0.2.10",
                &script("stream-text"),
                &Collect::default(),
            )
            .expect_err("no program"),
        missing
            .interactive(&Cancel::never(), "192.0.2.10", "say-hello")
            .expect_err("no program"),
    ] {
        assert!(matches!(result, RunError::Other(_)), "{result:?}");
    }
    missing.close();

    let (binary, _) = fake_ssh(directory.path());
    let runner = runner_with(&binary);
    // A deadline kills the sleeping command: it ends without a status.
    let cancel = Cancel::detached(Duration::from_millis(200));
    let remote = remote_of(
        runner
            .run(&cancel, "192.0.2.10", &script("sleep-forever"))
            .expect_err("cancelled"),
    );
    assert_eq!(remote.exit_code, -1);
    runner.close();
}

#[test]
fn stream_copies_both_outputs_and_reports_the_failure_with_its_stderr() {
    let directory = tempfile::tempdir().expect("tempdir");
    let (binary, _) = fake_ssh(directory.path());
    let runner = runner_with(&binary);

    let sink = Collect::default();
    runner
        .stream(
            &Cancel::never(),
            "192.0.2.10",
            &script("stream-text"),
            &sink,
        )
        .expect("stream");
    let text = sink.text();
    assert!(
        text.contains("line one\n") && text.contains("line two\n"),
        "{text}"
    );

    let sink = Collect::default();
    let remote = remote_of(
        runner
            .stream(
                &Cancel::never(),
                "192.0.2.10",
                &script("stream-fail"),
                &sink,
            )
            .expect_err("exit 3"),
    );
    assert_eq!(remote.exit_code, 3);
    assert_eq!(remote.stderr, "stream went wrong\n");
    assert!(sink.text().contains("stream went wrong"), "{}", sink.text());

    // A cancelled stream closes the command's stdin, which ends a watchdog
    // that reads it, and then succeeds.
    let sink = Collect::default();
    let cancel = Cancel::detached(Duration::from_millis(200));
    runner
        .stream(&cancel, "192.0.2.10", &script("wait-for-stdin"), &sink)
        .expect("a cancelled stream is not an error");
    runner.close();
}

#[test]
fn interactive_runs_attached_and_reports_the_exit_status() {
    let directory = tempfile::tempdir().expect("tempdir");
    let (binary, log) = fake_ssh(directory.path());
    let runner = runner_with(&binary);
    runner
        .interactive(&Cancel::never(), "192.0.2.10", "say-hello")
        .expect("success");
    let remote = remote_of(
        runner
            .interactive(&Cancel::never(), "192.0.2.10", "fail-seven")
            .expect_err("exit 7"),
    );
    assert_eq!((remote.exit_code, remote.stderr.as_str()), (7, ""));
    let calls = std::fs::read_to_string(log).expect("log");
    assert!(
        calls
            .lines()
            .next()
            .is_some_and(|line| line.contains(" -t ") && !line.contains("BatchMode")),
        "an interactive session allocates a terminal and may prompt: {calls}"
    );
    let cancel = Cancel::detached(Duration::from_millis(200));
    let remote = remote_of(
        runner
            .interactive(&cancel, "192.0.2.10", "sleep-forever")
            .expect_err("cancelled"),
    );
    assert_eq!(remote.exit_code, -1);
    runner.close();
}

#[test]
fn close_asks_ssh_to_end_each_control_master_and_removes_the_directory() {
    let directory = tempfile::tempdir().expect("tempdir");
    let (binary, log) = fake_ssh(directory.path());
    let mut runner = runner_with(&binary);
    let config = directory.path().join("ssh_config");
    runner.config_file = Some(config.clone());
    runner
        .run(&Cancel::never(), "192.0.2.10", &script("say-hello"))
        .expect("first host");
    runner
        .run(&Cancel::never(), "192.0.2.11", &script("say-hello"))
        .expect("second host");
    runner
        .run(&Cancel::never(), "192.0.2.11", &script("say-hello"))
        .expect("second host again");
    runner.close();
    let calls = std::fs::read_to_string(&log).expect("log");
    let exits: Vec<&str> = calls
        .lines()
        .filter(|line| line.contains(" -O exit "))
        .collect();
    assert_eq!(exits.len(), 2, "one control master per host: {calls}");
    for line in &exits {
        assert!(line.contains(&format!("-F {}", config.display())), "{line}");
        assert!(
            line.contains("-l deploy") && line.contains("-p 2222"),
            "{line}"
        );
    }
    assert!(exits[0].ends_with("-- 192.0.2.10") && exits[1].ends_with("-- 192.0.2.11"));
    // Closing again, or a runner that never connected, does nothing.
    runner.close();
    let idle = runner_with(&binary);
    idle.close();
    assert_eq!(
        std::fs::read_to_string(&log).expect("log").lines().count(),
        calls.lines().count()
    );
}

// --- local commands ----------------------------------------------------------

fn argv(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}

#[test]
fn local_output_returns_stdout_and_describes_failures() {
    let directory = tempfile::tempdir().expect("tempdir");
    let local = ExecLocal;
    let cancel = Cancel::never();
    let output = local
        .output(
            &cancel,
            directory.path(),
            &argv(&["sh", "-c", "echo $0-ok", "name"]),
        )
        .expect("success");
    assert_eq!(output, "name-ok\n");

    let error = local
        .output(
            &cancel,
            directory.path(),
            &argv(&["sh", "-c", "echo 'it broke' >&2; exit 4"]),
        )
        .expect_err("exit 4");
    assert_eq!(error, "sh: exit status 4: it broke");

    assert_eq!(
        local
            .output(&cancel, directory.path(), &[])
            .expect_err("no command"),
        "no command"
    );
    let error = local
        .output(&cancel, directory.path(), &argv(&["/no/such/program"]))
        .expect_err("cannot start");
    assert!(error.starts_with("/no/such/program: "), "{error}");

    // The working directory is the one given.
    let output = local
        .output(&cancel, directory.path(), &argv(&["pwd"]))
        .expect("pwd");
    assert_eq!(
        Path::new(output.trim()).canonicalize().expect("canonical"),
        directory.path().canonicalize().expect("canonical")
    );

    // A deadline kills a command that does not end.
    let error = local
        .output(
            &Cancel::detached(Duration::from_millis(200)),
            directory.path(),
            &argv(&["sleep", "30"]),
        )
        .expect_err("killed");
    assert_eq!(error, "sleep: signal: killed: ");
}

#[test]
fn local_run_passes_stdin_and_environment_and_reports_the_status() {
    let directory = tempfile::tempdir().expect("tempdir");
    let local = ExecLocal;
    let cancel = Cancel::never();
    let copy = directory.path().join("copy");
    local
        .run(
            &cancel,
            directory.path(),
            &argv(&[
                "sh",
                "-c",
                &format!(
                    "cat > {}; echo \"$FORGE_VERSION\" >> {}",
                    copy.display(),
                    copy.display()
                ),
            ]),
            Some(b"password\n"),
            &[("FORGE_VERSION".to_owned(), "v3".to_owned())],
        )
        .expect("success");
    assert_eq!(
        std::fs::read_to_string(copy).expect("copy"),
        "password\nv3\n"
    );

    assert_eq!(
        local
            .run(
                &cancel,
                directory.path(),
                &argv(&["sh", "-c", "exit 9"]),
                None,
                &[]
            )
            .expect_err("exit 9"),
        "sh: exit status 9"
    );
    assert_eq!(
        local
            .run(&cancel, directory.path(), &[], None, &[])
            .expect_err("no command"),
        "no command"
    );
    let error = local
        .run(
            &cancel,
            directory.path(),
            &argv(&["/no/such/program"]),
            None,
            &[],
        )
        .expect_err("cannot start");
    assert!(error.starts_with("/no/such/program: "), "{error}");
    let error = local
        .run(
            &Cancel::detached(Duration::from_millis(200)),
            directory.path(),
            &argv(&["sleep", "30"]),
            None,
            &[],
        )
        .expect_err("killed");
    assert_eq!(error, "sleep: signal: killed");
}

// --- dry runs ---------------------------------------------------------------

fn dry(inner: Option<Box<dyn Runner>>) -> (DryRunner, Buffer) {
    let log = Buffer::default();
    let secrets = Secrets::from_values([("TOKEN", "hunter-two-secret")]);
    (
        DryRunner {
            inner,
            out: log.writer(),
            redactor: secrets.redactor(),
        },
        log,
    )
}

#[test]
fn a_dry_runner_prints_every_kind_of_command_and_runs_none_of_the_mutating_ones() {
    let (runner, log) = dry(None);
    let cancel = Cancel::never();
    // Without an inner runner a read-only query answers with nothing.
    let query = RemoteCommand {
        script: "docker ps".to_owned(),
        read_only: true,
        ..RemoteCommand::default()
    };
    assert_eq!(
        runner.run(&cancel, "web1", &query).expect("query").stdout,
        ""
    );
    let change = RemoteCommand {
        script: "docker login --password hunter-two-secret".to_owned(),
        stdin: Some(b"x".to_vec()),
        stdin_note: "registry password (TOKEN)".to_owned(),
        read_only: false,
    };
    runner.run(&cancel, "web1", &change).expect("planned");
    runner
        .stream(&cancel, "web1", &script("docker logs"), &Collect::default())
        .expect("planned stream");
    runner
        .interactive(&cancel, "web1", "docker exec -it app sh")
        .expect("planned shell");
    runner.close();
    assert_eq!(
        log.text(),
        "web1            ? docker ps\n\
         web1            $ docker login --password [REDACTED]\n\
         web1            < stdin: registry password (TOKEN)\n\
         web1            $ docker logs\n\
         web1            $ docker exec -it app sh\n"
    );

    // With an inner runner the query runs there, and close reaches it.
    struct Inner(Arc<Mutex<Vec<String>>>);
    impl Runner for Inner {
        fn run(
            &self,
            _: &Cancel,
            host: &str,
            command: &RemoteCommand,
        ) -> Result<super::runner::Output, RunError> {
            self.0
                .lock()
                .expect("lock")
                .push(format!("run {host} {}", command.script));
            Ok(super::runner::Output {
                stdout: "real".to_owned(),
                stderr: String::new(),
            })
        }
        fn stream(
            &self,
            _: &Cancel,
            _: &str,
            _: &RemoteCommand,
            _: &dyn Sink,
        ) -> Result<(), RunError> {
            Ok(())
        }
        fn interactive(&self, _: &Cancel, _: &str, _: &str) -> Result<(), RunError> {
            Ok(())
        }
        fn close(&self) {
            self.0.lock().expect("lock").push("close".to_owned());
        }
    }
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (runner, _) = dry(Some(Box::new(Inner(Arc::clone(&seen)))));
    assert_eq!(
        runner.run(&cancel, "web1", &query).expect("query").stdout,
        "real"
    );
    runner.close();
    assert_eq!(*seen.lock().expect("lock"), ["run web1 docker ps", "close"]);
}

#[test]
fn a_dry_local_runner_prints_commands_and_hides_values() {
    struct Real;
    impl LocalRunner for Real {
        fn output(&self, _: &Cancel, _: &Path, argv: &[String]) -> Result<String, String> {
            Ok(format!("ran {}", argv.join(" ")))
        }
        fn run(
            &self,
            _: &Cancel,
            _: &Path,
            _: &[String],
            _: Option<&[u8]>,
            _: &[(String, String)],
        ) -> Result<(), String> {
            Err("a dry run must not run this".to_owned())
        }
    }
    let log = Buffer::default();
    let local = DryLocal {
        inner: Box::new(Real),
        out: log.writer(),
        redactor: Secrets::default().redactor(),
    };
    let cancel = Cancel::never();
    let dir = Path::new(".");
    // Queries still run: the plan names real commits.
    assert_eq!(
        local
            .output(&cancel, dir, &argv(&["git", "rev-parse"]))
            .expect("query"),
        "ran git rev-parse"
    );
    local
        .run(
            &cancel,
            dir,
            &argv(&["docker", "login", "--password-stdin"]),
            Some(b"secret"),
            &[
                ("FORGE_COMMAND".to_owned(), "deploy".to_owned()),
                ("FORGE_VERSION".to_owned(), "v3".to_owned()),
            ],
        )
        .expect("planned");
    assert_eq!(
        log.text(),
        "local           $ docker login --password-stdin\n\
         local           < stdin: value not shown\n\
         local             with 2 FORGE_* variables\n"
    );
}
