//! `forge logs` and `forge app exec|shell`: streams and commands next to
//! the deployed containers.

use std::io::Write;
use std::sync::{Arc, Mutex};

use regex_automata::meta::Regex;

use super::commands::RemoteCommand;
use super::config::{ARG, IMAGE_REF, SINCE};
use super::deployer::{Deployer, lock};
use super::errors::DeployError;
use super::runner::{Cancel, Sink};
use super::secrets::Redactor;

/// What `forge logs` streams.
#[derive(Clone, Debug, Default)]
pub(crate) struct LogsOptions {
    pub(crate) role: String,
    pub(crate) host: String,
    pub(crate) follow: bool,
    pub(crate) since: String,
    pub(crate) lines: i64,
    /// Keep only lines matching this regular expression (RE2 syntax).
    pub(crate) grep: String,
}

/// What `forge app exec` runs.
#[derive(Clone, Debug, Default)]
pub(crate) struct ExecOptions {
    pub(crate) role: String,
    pub(crate) host: String,
    /// Allocate a terminal (ssh -t, docker -it).
    pub(crate) interactive: bool,
    /// Run inside the running container instead of a one-off container of
    /// the same image.
    pub(crate) reuse: bool,
    pub(crate) argv: Vec<String>,
}

/// Output shared by several streams, written a line at a time.
pub(crate) type Shared = Arc<Mutex<Box<dyn Write + Send>>>;

/// Splits output into lines, filters, redacts and prefixes them; safe for
/// concurrent use (the stdout and stderr readers of one host).
pub(crate) struct PrefixWriter {
    prefix: String,
    out: Shared,
    grep: Option<Regex>,
    redactor: Redactor,
    pending: Mutex<Vec<u8>>,
}

impl PrefixWriter {
    pub(crate) fn new(
        prefix: String,
        out: Shared,
        grep: Option<Regex>,
        redactor: Redactor,
    ) -> Self {
        Self {
            prefix,
            out,
            grep,
            redactor,
            pending: Mutex::new(Vec::new()),
        }
    }

    fn emit(&self, line: &[u8]) {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if let Some(grep) = &self.grep
            && !grep.is_match(line)
        {
            return;
        }
        let text = self.redactor.redact(&String::from_utf8_lossy(line));
        let mut out = lock(&self.out);
        let _ = writeln!(out, "{}{text}", self.prefix);
        let _ = out.flush();
    }

    /// Writes what is left without a newline.
    pub(crate) fn flush(&self) {
        let rest = std::mem::take(&mut *lock(&self.pending));
        if !rest.is_empty() {
            self.emit(&rest);
        }
    }
}

impl Sink for PrefixWriter {
    fn write(&self, data: &[u8]) {
        let mut pending = lock(&self.pending);
        pending.extend_from_slice(data);
        while let Some(index) = pending.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = pending.drain(..=index).collect();
            self.emit(&line[..line.len() - 1]);
        }
        if pending.len() > 64 << 10 {
            let line = std::mem::take(&mut *pending);
            self.emit(&line);
        }
    }
}

impl Deployer {
    fn select_hosts(&self, role: &str, host: &str) -> Result<(String, Vec<String>), DeployError> {
        let role = if role.is_empty() {
            self.config.primary_role()
        } else {
            role.to_owned()
        };
        let Some(definition) = self.config.servers.get(&role) else {
            return Err(DeployError::new(format!("unknown role {role:?}")));
        };
        if host.is_empty() {
            return Ok((role, definition.hosts.clone()));
        }
        if !definition.hosts.iter().any(|h| h == host) {
            return Err(DeployError::new(format!(
                "host {host:?} does not run role {role}"
            )));
        }
        Ok((role, vec![host.to_owned()]))
    }

    /// Streams the logs of the current role container from every selected
    /// host, each line prefixed `host|`, until every stream ended or
    /// `cancel` fired; cancellation stops the remote `docker logs`. One
    /// scoped thread per host.
    pub(crate) fn logs(
        &self,
        cancel: &Cancel,
        out: Shared,
        mut options: LogsOptions,
    ) -> Result<(), DeployError> {
        self.prepare()?;
        let (role, hosts) = self.select_hosts(&options.role, &options.host)?;
        if !options.since.is_empty() && !SINCE.matches(&options.since) {
            return Err(DeployError::new(
                "--since must be a duration such as 1h or a timestamp",
            ));
        }
        if !(0..=100_000).contains(&options.lines) {
            return Err(DeployError::new("--lines must be between 0 and 100000"));
        }
        if options.lines == 0 && options.since.is_empty() {
            options.lines = if options.follow { 10 } else { 100 };
        }
        let grep = if options.grep.is_empty() {
            None
        } else {
            Some(
                Regex::new(&options.grep)
                    .map_err(|error| DeployError::new(format!("--grep: {error}")))?,
            )
        };
        let command = self
            .cmds
            .logs(&role, options.follow, &options.since, options.lines);
        let errors: Vec<DeployError> = std::thread::scope(|scope| {
            let handles: Vec<_> = hosts
                .iter()
                .map(|host| {
                    let writer = PrefixWriter::new(
                        format!("{host}|"),
                        Arc::clone(&out),
                        grep.clone(),
                        self.redactor.clone(),
                    );
                    let command = &command;
                    scope.spawn(move || {
                        let result = self.stream(cancel, host, command, &writer);
                        writer.flush();
                        result
                    })
                })
                .collect();
            handles
                .into_iter()
                .filter_map(|handle| match handle.join() {
                    Ok(result) => result.err(),
                    Err(_) => Some(DeployError::new("log stream failed")),
                })
                .collect()
        });
        DeployError::join(errors).map_or(Ok(()), Err)
    }

    /// Runs a command on one host of the role (the first unless `host`).
    /// By default in a one-off container of the image the role currently
    /// runs, with its env file, so the arguments go to the image entrypoint:
    /// `forge app exec migrate status` runs `<binary> migrate status`.
    pub(crate) fn exec(
        &self,
        cancel: &Cancel,
        out: Shared,
        options: &ExecOptions,
    ) -> Result<(), DeployError> {
        self.prepare()?;
        if options.argv.is_empty() {
            return Err(DeployError::new("a command is required"));
        }
        if options.argv.iter().any(|arg| !ARG.matches(arg)) {
            return Err(DeployError::new(
                "command arguments must not contain control characters",
            ));
        }
        let (role, hosts) = self.select_hosts(&options.role, &options.host)?;
        let host = &hosts[0];
        let script = if options.reuse {
            self.cmds
                .exec_reuse(&role, &options.argv, options.interactive)
        } else {
            self.cmds
                .exec_one_off(&role, &options.argv, options.interactive)
        };
        if options.interactive {
            return self.interactive(cancel, host, &script);
        }
        let writer = PrefixWriter::new(String::new(), out, None, self.redactor.clone());
        let command = RemoteCommand {
            script,
            ..RemoteCommand::default()
        };
        let result = self.stream(cancel, host, &command, &writer);
        writer.flush();
        result
    }

    /// Opens an interactive shell on one host of the role. Distroless
    /// images have no shell: pass a debug `image` (busybox), which runs as a
    /// sidecar in the container's network and process namespaces, without
    /// the application's environment.
    pub(crate) fn shell(
        &self,
        cancel: &Cancel,
        role: &str,
        host: &str,
        image: &str,
    ) -> Result<(), DeployError> {
        self.prepare()?;
        let (role, hosts) = self.select_hosts(role, host)?;
        let script = if image.is_empty() {
            self.cmds.exec_reuse(&role, &["sh".to_owned()], true)
        } else {
            if !IMAGE_REF.matches(image) {
                return Err(DeployError::new(format!(
                    "--image {image:?} is not an image reference"
                )));
            }
            self.cmds.debug_shell(&role, image)
        };
        self.interactive(cancel, &hosts[0], &script)
    }
}
