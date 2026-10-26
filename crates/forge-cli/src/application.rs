//! Building the generated application with Cargo and running its own
//! commands (`migrate`, `seed`, `db create`, ...).
//!
//! The CLI never reimplements what the application binary does and never reads
//! the database URL: the child inherits the environment and the terminal, so
//! credentials travel only from the caller's environment to the application.

use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::Value;

use crate::CliError;
use crate::lifecycle::ALLOW_DESTRUCTIVE;
use crate::process::{OwnedChild, STOP_GRACE, Shutdown, Signals};
use crate::project::Project;

/// The result of `cargo build`.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum BuildOutcome {
    /// The build succeeded; this is the binary it produced.
    Built(PathBuf),
    /// Cargo reported errors (it printed them), with its exit code.
    Failed(Option<i32>),
    /// A shutdown request ended the build.
    Interrupted,
}

/// Runs `cargo build --locked` for the package in `root` and finds the binary
/// it produced from Cargo's machine-readable messages. Diagnostics still reach
/// the terminal, rendered by Cargo. `bin` selects one binary of the package.
pub(crate) fn cargo_build(
    root: &Path,
    bin: Option<&str>,
    release: bool,
    shutdown: &Shutdown,
) -> Result<BuildOutcome, CliError> {
    let mut command = Command::new("cargo");
    command
        .args([
            "build",
            "--locked",
            "--message-format=json-render-diagnostics",
        ])
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    if release {
        command.arg("--release");
    }
    if let Some(bin) = bin {
        command.args(["--bin", bin]);
    }
    // Its own process group: a stop reaches rustc and the build scripts too.
    let mut child = OwnedChild::spawn(command, true)?;
    let stdout = child.take_stdout();
    let wanted = bin.map(str::to_owned);
    let reader = std::thread::Builder::new()
        .name("forge-cargo-messages".to_owned())
        .spawn(move || stdout.and_then(|pipe| find_binary(BufReader::new(pipe), wanted.as_deref())))
        .map_err(|source| CliError::SpawnProcess {
            program: "cargo".to_owned(),
            source,
        })?;
    let waited = child.wait(shutdown, STOP_GRACE);
    // The pipe closes when cargo exits, so the reader ends; join it either way.
    let binary = reader.join().ok().flatten();
    let (status, interrupted) = waited.map_err(|source| CliError::SpawnProcess {
        program: "cargo".to_owned(),
        source,
    })?;
    if interrupted {
        return Ok(BuildOutcome::Interrupted);
    }
    if !status.success() {
        return Ok(BuildOutcome::Failed(status.code()));
    }
    binary.map(BuildOutcome::Built).ok_or(CliError::NoBinary {
        name: bin.map(str::to_owned),
    })
}

/// The executable of the last `compiler-artifact` message for a binary target
/// (named `wanted`, when given).
fn find_binary(lines: impl std::io::BufRead, wanted: Option<&str>) -> Option<PathBuf> {
    let mut found = None;
    for line in lines.lines().map_while(Result::ok) {
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if message["reason"] != "compiler-artifact" {
            continue;
        }
        let is_bin = message["target"]["kind"]
            .as_array()
            .is_some_and(|kinds| kinds.iter().any(|kind| kind == "bin"));
        let named = wanted.is_none_or(|name| message["target"]["name"] == name);
        if let (true, true, Some(executable)) = (is_bin, named, message["executable"].as_str()) {
            found = Some(PathBuf::from(executable));
        }
    }
    found
}

/// Keeps the CLI's own settings out of the application's environment: the
/// application refuses unknown `FORGE_*` keys at startup.
pub(crate) fn scrub(command: &mut Command) {
    command.env_remove(ALLOW_DESTRUCTIVE);
}

/// What the lifecycle commands need from the generated application.
pub(crate) trait Application {
    /// Runs one of the application's own commands with its arguments, with the
    /// terminal and the environment of the caller, and returns its exit code
    /// (`None` when a signal ended it).
    fn run(&mut self, arguments: &[String]) -> Result<Option<i32>, CliError>;
}

/// The application of a [`Project`], built with Cargo on first use (once per
/// CLI invocation) and run as a direct child.
pub(crate) struct CargoApplication {
    project: Project,
    shutdown: Shutdown,
    /// The SIGINT/SIGTERM handlers, installed when the first child starts: a
    /// Ctrl-C at a confirmation prompt, before that, ends the CLI as usual.
    signals: Option<Signals>,
    binary: Option<PathBuf>,
}

impl CargoApplication {
    pub(crate) fn new(project: Project) -> Self {
        Self {
            project,
            shutdown: Shutdown::new(),
            signals: None,
            binary: None,
        }
    }

    fn binary(&mut self) -> Result<PathBuf, CliError> {
        if self.signals.is_none() {
            self.signals = Some(Signals::install_for(self.shutdown.clone())?);
        }
        if let Some(binary) = &self.binary {
            return Ok(binary.clone());
        }
        match cargo_build(
            &self.project.root,
            Some(&self.project.name),
            false,
            &self.shutdown,
        )? {
            BuildOutcome::Built(binary) => {
                self.binary = Some(binary.clone());
                Ok(binary)
            }
            BuildOutcome::Failed(code) => Err(CliError::CargoFailed {
                command: "build",
                code,
            }),
            BuildOutcome::Interrupted => Err(CliError::Interrupted),
        }
    }
}

impl Application for CargoApplication {
    fn run(&mut self, arguments: &[String]) -> Result<Option<i32>, CliError> {
        let binary = self.binary()?;
        let mut command = Command::new(&binary);
        command
            .args(arguments)
            .current_dir(&self.project.root)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        scrub(&mut command);
        // Not in its own group: a password prompt reads the terminal, and
        // Ctrl-C reaches the application directly.
        let mut child = OwnedChild::spawn(command, false)?;
        let (status, interrupted) =
            child
                .wait(&self.shutdown, STOP_GRACE)
                .map_err(|source| CliError::SpawnProcess {
                    program: binary.display().to_string(),
                    source,
                })?;
        if interrupted {
            return Err(CliError::Interrupted);
        }
        Ok(status.code())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

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

        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "umask 022; cat > \"$1\" && chmod 755 \"$1\"", "sh"])
            .arg(path)
            .stdin(std::process::Stdio::piped())
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

    fn lines(text: &str) -> impl std::io::BufRead + '_ {
        std::io::Cursor::new(text.as_bytes())
    }

    const MESSAGES: &str = r#"
{"reason":"compiler-artifact","target":{"kind":["lib"],"name":"app"},"executable":null}
{"reason":"compiler-artifact","target":{"kind":["bin"],"name":"other"},"executable":"/t/debug/other"}
not json at all
{"reason":"compiler-message","message":{"rendered":"warning"}}
{"reason":"compiler-artifact","target":{"kind":["bin"],"name":"shop"},"executable":"/t/debug/shop"}
{"reason":"build-finished","success":true}
"#;

    #[test]
    fn the_clis_own_switch_never_reaches_the_application() {
        let mut command = Command::new("app");
        scrub(&mut command);
        let removed: Vec<_> = command
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .map(|(key, _)| key.to_string_lossy().into_owned())
            .collect();
        assert_eq!(removed, ["FORGE_ALLOW_DESTRUCTIVE"]);
    }

    #[cfg(unix)]
    #[test]
    fn the_application_runs_in_the_project_directory_and_its_exit_code_is_returned() {
        let directory = tempfile::tempdir().expect("tempdir");
        let script = directory.path().join("app");
        write_executable(
            &script,
            "#!/bin/sh\npwd > record\necho \"$@\" >> record\nexit 7\n",
        );
        let mut app = CargoApplication {
            project: Project {
                root: directory.path().to_path_buf(),
                name: "app".to_owned(),
            },
            shutdown: Shutdown::new(),
            signals: None,
            binary: Some(script),
        };

        let code = app
            .run(&["db".to_owned(), "create".to_owned()])
            .expect("runs");

        assert_eq!(code, Some(7));
        let record = fs::read_to_string(directory.path().join("record")).expect("record");
        let mut lines = record.lines();
        assert!(
            lines.next().is_some_and(|dir| Path::new(dir)
                .canonicalize()
                .is_ok_and(|dir| dir == directory.path().canonicalize().expect("canonical"))),
            "{record}"
        );
        assert_eq!(lines.next(), Some("db create"));
    }

    #[test]
    fn the_wanted_binary_is_found_among_cargos_messages() {
        assert_eq!(
            find_binary(lines(MESSAGES), Some("shop")),
            Some(PathBuf::from("/t/debug/shop"))
        );
        assert_eq!(
            find_binary(lines(MESSAGES), Some("other")),
            Some(PathBuf::from("/t/debug/other"))
        );
        assert_eq!(find_binary(lines(MESSAGES), Some("missing")), None);
        assert_eq!(
            find_binary(lines(MESSAGES), None),
            Some(PathBuf::from("/t/debug/shop")),
            "without a name the last binary wins"
        );
        assert_eq!(find_binary(lines("garbage\n"), None), None);
    }
}
