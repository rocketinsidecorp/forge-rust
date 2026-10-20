//! How commands reach hosts and the local machine.
//!
//! [`SshRunner`] drives the system `ssh` client with an argument vector
//! (never a local shell), so `~/.ssh/config`, the agent, `known_hosts` and
//! `ProxyJump` apply. Host keys are always verified (nothing relaxes
//! `StrictHostKeyChecking`), `BatchMode=yes` prevents password prompts,
//! and one multiplexed master connection per host is shared by every
//! command of an invocation and closed at the end. The remote side runs the
//! quoted script with `sh -c`, whatever the login shell. Secret material
//! is written to the child's stdin, never to its arguments.
//!
//! Every child is owned: waited for, or killed when the invocation is
//! cancelled; reader and writer threads are scoped to the call.

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::commands::{RemoteCommand, join, quote};
use super::config::valid_ssh_host;
use super::errors::RemoteError;
use super::secrets::Redactor;
use crate::process::Shutdown;

/// How often a waiting thread checks its child and the cancellation.
const POLL: Duration = Duration::from_millis(20);
/// How long a cancelled stream may take to end after its stdin closed.
const WAIT_DELAY: Duration = Duration::from_secs(5);

/// When work must stop: a signal, a deadline, or never.
#[derive(Clone, Debug, Default)]
pub(crate) struct Cancel {
    shutdown: Option<Shutdown>,
    deadline: Option<Instant>,
}

impl Cancel {
    /// Never cancelled.
    pub(crate) fn never() -> Self {
        Self::default()
    }

    /// Cancelled by SIGINT/SIGTERM.
    pub(crate) fn signal(shutdown: Shutdown) -> Self {
        Self {
            shutdown: Some(shutdown),
            deadline: None,
        }
    }

    /// Whether work must stop.
    pub(crate) fn is_cancelled(&self) -> bool {
        self.shutdown.as_ref().is_some_and(Shutdown::requested)
            || self
                .deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
    }

    /// Go's `context.WithoutCancel` plus a timeout: for cleanup that must
    /// run even after a signal.
    pub(crate) fn detached(timeout: Duration) -> Self {
        Self {
            shutdown: None,
            deadline: Some(Instant::now() + timeout),
        }
    }
}

/// Captured output of a command.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct Output {
    pub(crate) stdout: String,
    pub(crate) stderr: String,
}

/// Why a command did not succeed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum RunError {
    /// The remote command exited non-zero.
    Remote(RemoteError),
    /// ssh itself, or starting it, failed.
    Other(String),
}

impl std::fmt::Display for RunError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Remote(error) => error.fmt(formatter),
            Self::Other(message) => formatter.write_str(message),
        }
    }
}

/// Receives streamed output; safe to call from several threads.
pub(crate) trait Sink: Sync {
    fn write(&self, data: &[u8]);
}

/// Runs commands on hosts.
pub(crate) trait Runner: Send + Sync {
    /// Runs `command` on `host` and captures its output; a non-zero exit is
    /// [`RunError::Remote`].
    fn run(&self, cancel: &Cancel, host: &str, command: &RemoteCommand)
    -> Result<Output, RunError>;
    /// Runs `command` and copies its combined output to `out` until it
    /// exits or `cancel` fires. On cancellation the command's stdin is
    /// closed first, which lets watchdog scripts stop remote processes;
    /// a cancelled stream returns `Ok`.
    fn stream(
        &self,
        cancel: &Cancel,
        host: &str,
        command: &RemoteCommand,
        out: &dyn Sink,
    ) -> Result<(), RunError>;
    /// Runs `script` attached to the local terminal.
    fn interactive(&self, cancel: &Cancel, host: &str, script: &str) -> Result<(), RunError>;
    /// Releases connections.
    fn close(&self);
}

/// The system ssh client.
pub(crate) struct SshRunner {
    pub(crate) user: String,
    pub(crate) port: i64,
    pub(crate) proxy_jump: String,
    /// Passed as `ssh -F` when set (tests use it).
    pub(crate) config_file: Option<PathBuf>,
    pub(crate) binary: PathBuf,
    pub(crate) connect_timeout: Duration,
    control_dir: Mutex<Option<PathBuf>>,
    hosts: Mutex<Vec<String>>,
}

impl SshRunner {
    /// A runner configured from `ssh:` in the configuration.
    pub(crate) fn new(ssh: &super::config::Ssh) -> Self {
        Self {
            user: ssh.user.clone(),
            port: ssh.port,
            proxy_jump: ssh.proxy_jump.clone(),
            config_file: None,
            binary: PathBuf::from("ssh"),
            connect_timeout: Duration::from_secs(10),
            control_dir: Mutex::new(None),
            hosts: Mutex::new(Vec::new()),
        }
    }

    /// A private, short directory for control sockets (their paths are
    /// limited to about 100 bytes), created 0700 on first use.
    fn control_dir(&self) -> Result<PathBuf, RunError> {
        let mut slot = lock(&self.control_dir);
        if let Some(directory) = slot.as_ref() {
            return Ok(directory.clone());
        }
        let base = std::env::temp_dir();
        for _ in 0..8 {
            let mut random = [0_u8; 6];
            getrandom::fill(&mut random).map_err(|error| {
                RunError::Other(format!("create ssh control directory: {error}"))
            })?;
            let name: String = random.iter().map(|b| format!("{b:02x}")).collect();
            let directory = base.join(format!("forge-ssh-{name}"));
            let mut builder = std::fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt as _;
                builder.mode(0o700);
            }
            match builder.create(&directory) {
                Ok(()) => {
                    *slot = Some(directory.clone());
                    return Ok(directory);
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(RunError::Other(format!(
                        "create ssh control directory: {error}"
                    )));
                }
            }
        }
        Err(RunError::Other(
            "create ssh control directory: no free name".to_owned(),
        ))
    }

    /// The ssh options and destination, exactly as Forge-Go passes them.
    pub(crate) fn args(&self, control_dir: &Path, host: &str, interactive: bool) -> Vec<String> {
        let mut args = vec![
            "-o".to_owned(),
            "ControlMaster=auto".to_owned(),
            "-o".to_owned(),
            format!("ControlPath={}/%C", control_dir.display()),
            "-o".to_owned(),
            "ControlPersist=60".to_owned(),
            "-o".to_owned(),
            format!("ConnectTimeout={}", self.connect_timeout.as_secs()),
            "-o".to_owned(),
            "ServerAliveInterval=15".to_owned(),
            "-o".to_owned(),
            "ServerAliveCountMax=3".to_owned(),
        ];
        if interactive {
            args.push("-t".to_owned());
        } else {
            args.extend(["-o".to_owned(), "BatchMode=yes".to_owned(), "-T".to_owned()]);
        }
        if let Some(config) = &self.config_file {
            args.extend(["-F".to_owned(), config.display().to_string()]);
        }
        if !self.user.is_empty() {
            args.extend(["-l".to_owned(), self.user.clone()]);
        }
        if self.port != 0 {
            args.extend(["-p".to_owned(), self.port.to_string()]);
        }
        if !self.proxy_jump.is_empty() {
            args.extend(["-J".to_owned(), self.proxy_jump.clone()]);
        }
        args.extend(["--".to_owned(), host.to_owned()]);
        args
    }

    /// The full argument vector for running `script` on `host`.
    pub(crate) fn command_args(
        &self,
        control_dir: &Path,
        host: &str,
        script: &str,
        interactive: bool,
    ) -> Vec<String> {
        let mut args = self.args(control_dir, host, interactive);
        // The script runs under sh -c regardless of the login shell.
        args.push(format!("sh -c {}", quote(script)));
        args
    }

    fn command(&self, host: &str, script: &str, interactive: bool) -> Result<Command, RunError> {
        if !valid_ssh_host(host) {
            return Err(RunError::Other(format!("invalid host {host:?}")));
        }
        let control_dir = self.control_dir()?;
        {
            let mut hosts = lock(&self.hosts);
            if !hosts.iter().any(|h| h == host) {
                hosts.push(host.to_owned());
            }
        }
        let mut command = Command::new(&self.binary);
        command.args(self.command_args(&control_dir, host, script, interactive));
        Ok(command)
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn spawn_error(program: &Path, error: &std::io::Error) -> RunError {
    RunError::Other(format!("{}: {error}", program.display()))
}

/// Waits for `child`, killing it when `cancel` fires. Returns the exit code
/// (`None` when killed by a signal) and whether it was cancelled.
fn wait(child: &mut Child, cancel: &Cancel) -> Result<(Option<i32>, bool), std::io::Error> {
    let mut cancelled = false;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok((status.code(), cancelled));
        }
        if !cancelled && cancel.is_cancelled() {
            cancelled = true;
            let _ = child.kill();
        }
        std::thread::sleep(POLL);
    }
}

fn remote_error(host: &str, code: Option<i32>, stderr: &str) -> RunError {
    match code {
        Some(255) => RunError::Other(format!("{host}: ssh failed: {}", stderr.trim())),
        Some(code) => RunError::Remote(RemoteError {
            host: host.to_owned(),
            exit_code: code,
            stderr: stderr.to_owned(),
            stdout: String::new(),
        }),
        None => RunError::Remote(RemoteError {
            host: host.to_owned(),
            exit_code: -1,
            stderr: stderr.to_owned(),
            stdout: String::new(),
        }),
    }
}

fn read_all(reader: Option<impl std::io::Read>) -> Vec<u8> {
    let mut buffer = Vec::new();
    if let Some(mut reader) = reader {
        let _ = reader.read_to_end(&mut buffer);
    }
    buffer
}

impl Runner for SshRunner {
    fn run(
        &self,
        cancel: &Cancel,
        host: &str,
        command: &RemoteCommand,
    ) -> Result<Output, RunError> {
        let mut process = self.command(host, &command.script, false)?;
        process
            .stdin(if command.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = process
            .spawn()
            .map_err(|error| spawn_error(&self.binary, &error))?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let (status, out, err) = std::thread::scope(|scope| {
            let writer = scope.spawn(|| {
                if let (Some(mut pipe), Some(data)) = (stdin, command.stdin.as_ref()) {
                    let _ = pipe.write_all(data);
                }
            });
            let out = scope.spawn(|| read_all(stdout));
            let err = scope.spawn(|| read_all(stderr));
            let status = wait(&mut child, cancel);
            let _ = writer.join();
            (
                status,
                out.join().unwrap_or_default(),
                err.join().unwrap_or_default(),
            )
        });
        let output = Output {
            stdout: String::from_utf8_lossy(&out).into_owned(),
            stderr: String::from_utf8_lossy(&err).into_owned(),
        };
        match status {
            Ok((Some(0), _)) => Ok(output),
            Ok((code, _)) => Err(match remote_error(host, code, &output.stderr) {
                // The stdout of a failed command still matters: a held
                // lock answers with its record.
                RunError::Remote(mut remote) => {
                    remote.stdout = output.stdout;
                    RunError::Remote(remote)
                }
                other => other,
            }),
            Err(error) => Err(RunError::Other(format!("{host}: {error}"))),
        }
    }

    fn stream(
        &self,
        cancel: &Cancel,
        host: &str,
        command: &RemoteCommand,
        out: &dyn Sink,
    ) -> Result<(), RunError> {
        let mut process = self.command(host, &command.script, false)?;
        process
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = process
            .spawn()
            .map_err(|error| spawn_error(&self.binary, &error))?;
        let mut stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let captured = Mutex::new(Vec::new());
        let (status, cancelled) = std::thread::scope(|scope| {
            scope.spawn(|| {
                if let Some(mut reader) = stdout {
                    let mut buffer = [0_u8; 8192];
                    while let Ok(n) = reader.read(&mut buffer) {
                        if n == 0 {
                            break;
                        }
                        out.write(&buffer[..n]);
                    }
                }
            });
            scope.spawn(|| {
                if let Some(mut reader) = stderr {
                    let mut buffer = [0_u8; 8192];
                    while let Ok(n) = reader.read(&mut buffer) {
                        if n == 0 {
                            break;
                        }
                        out.write(&buffer[..n]);
                        lock(&captured).extend_from_slice(&buffer[..n]);
                    }
                }
            });
            // Closing stdin ends the remote watchdog's read, which stops the
            // remote process; ssh then exits on its own, or is killed after
            // a grace period.
            let mut closed_at: Option<Instant> = None;
            loop {
                match child.try_wait() {
                    Ok(Some(status)) => return (Ok(status.code()), closed_at.is_some()),
                    Ok(None) => {}
                    Err(error) => return (Err(error), closed_at.is_some()),
                }
                if closed_at.is_none() && cancel.is_cancelled() {
                    stdin.take();
                    closed_at = Some(Instant::now());
                }
                if closed_at.is_some_and(|at| at.elapsed() >= WAIT_DELAY) {
                    let _ = child.kill();
                }
                std::thread::sleep(POLL);
            }
        });
        drop(stdin);
        if cancelled {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&lock(&captured)).into_owned();
        match status {
            Ok(Some(0)) => Ok(()),
            Ok(code) => Err(remote_error(host, code, &stderr)),
            Err(error) => Err(RunError::Other(format!("{host}: {error}"))),
        }
    }

    fn interactive(&self, cancel: &Cancel, host: &str, script: &str) -> Result<(), RunError> {
        let mut process = self.command(host, script, true)?;
        process
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        let mut child = process
            .spawn()
            .map_err(|error| spawn_error(&self.binary, &error))?;
        match wait(&mut child, cancel) {
            Ok((Some(0), _)) => Ok(()),
            Ok((code, _)) => Err(remote_error(host, code, "")),
            Err(error) => Err(RunError::Other(format!("{host}: {error}"))),
        }
    }

    fn close(&self) {
        let Some(control_dir) = lock(&self.control_dir).take() else {
            return;
        };
        let hosts = std::mem::take(&mut *lock(&self.hosts));
        for host in hosts {
            let mut args = vec![
                "-o".to_owned(),
                format!("ControlPath={}/%C", control_dir.display()),
            ];
            if let Some(config) = &self.config_file {
                args.extend(["-F".to_owned(), config.display().to_string()]);
            }
            if !self.user.is_empty() {
                args.extend(["-l".to_owned(), self.user.clone()]);
            }
            if self.port != 0 {
                args.extend(["-p".to_owned(), self.port.to_string()]);
            }
            args.extend(["-O".to_owned(), "exit".to_owned(), "--".to_owned(), host]);
            let spawned = Command::new(&self.binary)
                .args(&args)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
            if let Ok(mut child) = spawned {
                let _ = wait(&mut child, &Cancel::detached(Duration::from_secs(5)));
            }
        }
        let _ = std::fs::remove_dir_all(&control_dir);
    }
}

/// Runs local programs (git, docker build, hooks).
pub(crate) trait LocalRunner: Send + Sync {
    /// Runs `argv` in `dir` and returns its standard output.
    fn output(&self, cancel: &Cancel, dir: &Path, argv: &[String]) -> Result<String, String>;
    /// Runs `argv` with its output shown to the user (on stderr), `stdin`
    /// optional and `env` added to the process environment.
    fn run(
        &self,
        cancel: &Cancel,
        dir: &Path,
        argv: &[String],
        stdin: Option<&[u8]>,
        env: &[(String, String)],
    ) -> Result<(), String>;
}

/// The production [`LocalRunner`]: output goes to this process's stderr.
pub(crate) struct ExecLocal;

fn exit_text(code: Option<i32>) -> String {
    code.map_or_else(
        || "signal: killed".to_owned(),
        |code| format!("exit status {code}"),
    )
}

impl LocalRunner for ExecLocal {
    fn output(&self, cancel: &Cancel, dir: &Path, argv: &[String]) -> Result<String, String> {
        let program = argv.first().ok_or_else(|| "no command".to_owned())?;
        let mut child = Command::new(program)
            .args(&argv[1..])
            .current_dir(dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("{program}: {error}"))?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let (status, out, err) = std::thread::scope(|scope| {
            let out = scope.spawn(|| read_all(stdout));
            let err = scope.spawn(|| read_all(stderr));
            (
                wait(&mut child, cancel),
                out.join().unwrap_or_default(),
                err.join().unwrap_or_default(),
            )
        });
        match status {
            Ok((Some(0), _)) => Ok(String::from_utf8_lossy(&out).into_owned()),
            Ok((code, _)) => Err(format!(
                "{program}: {}: {}",
                exit_text(code),
                String::from_utf8_lossy(&err).trim()
            )),
            Err(error) => Err(format!("{program}: {error}")),
        }
    }

    fn run(
        &self,
        cancel: &Cancel,
        dir: &Path,
        argv: &[String],
        stdin: Option<&[u8]>,
        env: &[(String, String)],
    ) -> Result<(), String> {
        let program = argv.first().ok_or_else(|| "no command".to_owned())?;
        let mut command = Command::new(program);
        command
            .args(&argv[1..])
            .current_dir(dir)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stderr(Stdio::inherit())
            .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        // The child's stdout joins this process's stderr: stdout stays the
        // command's report (--json, the dry-run plan).
        #[cfg(unix)]
        {
            use std::os::fd::AsFd as _;
            if let Ok(fd) = std::io::stderr().as_fd().try_clone_to_owned() {
                command.stdout(Stdio::from(fd));
            }
        }
        let mut child = command
            .spawn()
            .map_err(|error| format!("{program}: {error}"))?;
        let pipe = child.stdin.take();
        let status = std::thread::scope(|scope| {
            scope.spawn(|| {
                if let (Some(mut pipe), Some(data)) = (pipe, stdin) {
                    let _ = pipe.write_all(data);
                }
            });
            wait(&mut child, cancel)
        });
        match status {
            Ok((Some(0), _)) => Ok(()),
            Ok((code, _)) => Err(format!("{program}: {}", exit_text(code))),
            Err(error) => Err(format!("{program}: {error}")),
        }
    }
}

/// Prints mutating commands instead of running them. Read-only queries run
/// on the inner runner, so the plan shows the containers that would really
/// be stopped or pruned.
pub(crate) struct DryRunner {
    pub(crate) inner: Option<Box<dyn Runner>>,
    pub(crate) out: std::sync::Arc<Mutex<Box<dyn std::io::Write + Send>>>,
    pub(crate) redactor: Redactor,
}

pub(crate) fn print_plan(
    out: &Mutex<Box<dyn std::io::Write + Send>>,
    redactor: &Redactor,
    host: &str,
    marker: &str,
    text: &str,
) {
    let mut out = lock(out);
    let _ = writeln!(out, "{host:<15} {marker} {}", redactor.redact(text));
    let _ = out.flush();
}

impl Runner for DryRunner {
    fn run(
        &self,
        cancel: &Cancel,
        host: &str,
        command: &RemoteCommand,
    ) -> Result<Output, RunError> {
        if command.read_only {
            print_plan(&self.out, &self.redactor, host, "?", &command.script);
            return match &self.inner {
                Some(inner) => inner.run(cancel, host, command),
                None => Ok(Output::default()),
            };
        }
        print_plan(&self.out, &self.redactor, host, "$", &command.script);
        if command.stdin.is_some() {
            print_plan(
                &self.out,
                &self.redactor,
                host,
                "<",
                &format!("stdin: {}", command.stdin_note),
            );
        }
        Ok(Output::default())
    }

    fn stream(
        &self,
        _cancel: &Cancel,
        host: &str,
        command: &RemoteCommand,
        _out: &dyn Sink,
    ) -> Result<(), RunError> {
        print_plan(&self.out, &self.redactor, host, "$", &command.script);
        Ok(())
    }

    fn interactive(&self, _cancel: &Cancel, host: &str, script: &str) -> Result<(), RunError> {
        print_plan(&self.out, &self.redactor, host, "$", script);
        Ok(())
    }

    fn close(&self) {
        if let Some(inner) = &self.inner {
            inner.close();
        }
    }
}

/// Prints local commands; `output` (git queries) still runs.
pub(crate) struct DryLocal {
    pub(crate) inner: Box<dyn LocalRunner>,
    pub(crate) out: std::sync::Arc<Mutex<Box<dyn std::io::Write + Send>>>,
    pub(crate) redactor: Redactor,
}

impl LocalRunner for DryLocal {
    fn output(&self, cancel: &Cancel, dir: &Path, argv: &[String]) -> Result<String, String> {
        self.inner.output(cancel, dir, argv)
    }

    fn run(
        &self,
        _cancel: &Cancel,
        _dir: &Path,
        argv: &[String],
        stdin: Option<&[u8]>,
        env: &[(String, String)],
    ) -> Result<(), String> {
        print_plan(&self.out, &self.redactor, "local", "$", &join(argv));
        if stdin.is_some() {
            print_plan(
                &self.out,
                &self.redactor,
                "local",
                "<",
                "stdin: value not shown",
            );
        }
        if !env.is_empty() {
            let mut out = lock(&self.out);
            let _ = writeln!(
                out,
                "{:<15}   with {} FORGE_* variables",
                "local",
                env.len()
            );
        }
        Ok(())
    }
}
