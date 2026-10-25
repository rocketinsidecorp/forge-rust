use std::fmt;
use std::io;
use std::path::PathBuf;

#[derive(Debug)]
pub enum CliError {
    CurrentDirectory(io::Error),
    InvalidApplicationName {
        name: String,
        reason: &'static str,
    },
    InvalidForgePath {
        path: String,
        reason: &'static str,
    },
    InvalidMigrationName {
        name: String,
        reason: &'static str,
    },
    Clock(std::time::SystemTimeError),
    /// `forge generate scaffold` refused the request; nothing was written.
    Scaffold(String),
    /// `forge generate model|controller|mailer|job` refused the request;
    /// nothing was written.
    Generate {
        generator: &'static str,
        reason: String,
    },
    /// `forge destroy` refused the request; nothing was removed.
    Destroy(String),
    /// `forge generate authentication` refused the request; nothing was
    /// written.
    Authentication(String),
    DestinationExists(PathBuf),
    CreateDirectory {
        path: PathBuf,
        source: io::Error,
    },
    WriteFile {
        path: PathBuf,
        source: io::Error,
    },
    ReadDirectory {
        path: PathBuf,
        source: io::Error,
    },
    SpawnCargo {
        command: &'static str,
        source: io::Error,
    },
    CargoFailed {
        command: &'static str,
        code: Option<i32>,
    },
    Output(io::Error),
    /// The current directory is not a Forge application.
    NotAnApplication(String),
    /// A process could not be started or waited for.
    SpawnProcess {
        program: String,
        source: io::Error,
    },
    /// `cargo build` produced no binary of that name.
    NoBinary {
        name: Option<String>,
    },
    /// The application's own command exited unsuccessfully (`None`: killed by
    /// a signal).
    AppCommandFailed {
        command: String,
        code: Option<i32>,
    },
    /// The database's own client (`psql`, `mysql`, `sqlite3`) exited
    /// unsuccessfully (`None`: killed by a signal).
    ClientFailed {
        program: String,
        code: Option<i32>,
    },
    /// A destructive command was refused or not confirmed; nothing changed.
    Refused(String),
    /// An argument is invalid in a way the argument parser cannot see.
    Usage(String),
    /// A signal ended the command.
    Interrupted,
    /// The signal handlers could not be installed.
    Signals(io::Error),
    /// The watched tree is too large to poll.
    TooManyFiles {
        root: PathBuf,
        limit: usize,
    },
    /// The command already printed what is wrong (`forge doctor`).
    Reported,
    /// `forge i18n check` cannot read the translations.
    I18n(String),
    /// `forge i18n check` found a locale without a key or a plural form.
    TranslationsMissing,
    /// `forge credentials` failed; the message names paths and key ids,
    /// never a key or a credential value.
    Credentials(String),
    /// `forge deploy`, `forge logs` or `forge app` failed, with the
    /// command's exit code (docs/cli.md).
    Deploy {
        message: String,
        code: u8,
    },
}

impl From<crate::deploy::DeployError> for CliError {
    fn from(error: crate::deploy::DeployError) -> Self {
        Self::Deploy {
            code: error.exit_code(),
            message: error.message,
        }
    }
}

impl CliError {
    /// The process exit code: 1, or the deploy commands' documented codes
    /// (3 lock held, 4 unhealthy and reverted, 5 migration failed, 6 hook
    /// failed, 7 inconsistent).
    #[must_use]
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Deploy { code, .. } => *code,
            _ => 1,
        }
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CurrentDirectory(error) => {
                write!(
                    formatter,
                    "could not determine the current directory: {error}"
                )
            }
            Self::InvalidApplicationName { name, reason } => {
                write!(formatter, "invalid application name `{name}`: {reason}")
            }
            Self::InvalidForgePath { path, reason } => {
                write!(formatter, "invalid --forge-path {path:?}: {reason}")
            }
            Self::InvalidMigrationName { name, reason } => {
                write!(formatter, "invalid migration name {name:?}: {reason}")
            }
            Self::Clock(error) => {
                write!(formatter, "system clock is before the Unix epoch: {error}")
            }
            Self::Scaffold(reason) => write!(formatter, "cannot generate the scaffold: {reason}"),
            Self::Generate { generator, reason } => {
                write!(formatter, "cannot generate the {generator}: {reason}")
            }
            Self::Destroy(reason) => formatter.write_str(reason),
            Self::Authentication(reason) => {
                write!(formatter, "cannot generate authentication: {reason}")
            }
            Self::DestinationExists(path) => write!(
                formatter,
                "destination `{}` already exists; refusing to overwrite it",
                path.display()
            ),
            Self::CreateDirectory { path, source } => {
                write!(formatter, "could not create `{}`: {source}", path.display())
            }
            Self::WriteFile { path, source } => {
                write!(formatter, "could not write `{}`: {source}", path.display())
            }
            Self::ReadDirectory { path, source } => {
                write!(formatter, "could not read `{}`: {source}", path.display())
            }
            Self::SpawnCargo { command, source } => {
                write!(formatter, "could not run `cargo {command}`: {source}")
            }
            Self::CargoFailed { command, code } => match code {
                Some(code) => write!(formatter, "`cargo {command}` failed with exit code {code}"),
                None => write!(formatter, "`cargo {command}` was terminated by a signal"),
            },
            Self::Output(error) => write!(formatter, "could not write command output: {error}"),
            Self::NotAnApplication(reason) => {
                write!(formatter, "{reason}: run this inside a Forge application")
            }
            Self::SpawnProcess { program, source } => {
                write!(formatter, "could not run `{program}`: {source}")
            }
            Self::NoBinary { name } => match name {
                Some(name) => write!(formatter, "`cargo build` produced no binary named `{name}`"),
                None => write!(formatter, "`cargo build` produced no binary"),
            },
            Self::AppCommandFailed { command, code } => match code {
                Some(code) => write!(
                    formatter,
                    "application command \"{command}\" failed with status {code}"
                ),
                None => write!(
                    formatter,
                    "application command \"{command}\" was terminated by a signal"
                ),
            },
            Self::ClientFailed { program, code } => match code {
                Some(code) => write!(formatter, "{program} exited with status {code}"),
                None => write!(formatter, "{program} was terminated by a signal"),
            },
            Self::Refused(reason) | Self::Usage(reason) => formatter.write_str(reason),
            Self::Interrupted => formatter.write_str("interrupted"),
            Self::Signals(error) => {
                write!(formatter, "could not install the signal handlers: {error}")
            }
            Self::TooManyFiles { root, limit } => write!(
                formatter,
                "more than {limit} watched files under `{}`: is a build output directory missing from the ignore rules?",
                root.display()
            ),
            Self::Reported => formatter.write_str("checks failed"),
            Self::I18n(reason) | Self::Credentials(reason) => formatter.write_str(reason),
            Self::TranslationsMissing => formatter.write_str("translations are missing"),
            Self::Deploy { message, .. } => formatter.write_str(message),
        }
    }
}

impl std::error::Error for CliError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::CurrentDirectory(error)
            | Self::Output(error)
            | Self::Signals(error)
            | Self::SpawnProcess { source: error, .. }
            | Self::CreateDirectory { source: error, .. }
            | Self::WriteFile { source: error, .. }
            | Self::ReadDirectory { source: error, .. }
            | Self::SpawnCargo { source: error, .. } => Some(error),
            Self::Clock(error) => Some(error),
            Self::InvalidApplicationName { .. }
            | Self::InvalidForgePath { .. }
            | Self::InvalidMigrationName { .. }
            | Self::Scaffold(_)
            | Self::Generate { .. }
            | Self::Destroy(_)
            | Self::Authentication(_)
            | Self::DestinationExists(_)
            | Self::NotAnApplication(_)
            | Self::NoBinary { .. }
            | Self::AppCommandFailed { .. }
            | Self::ClientFailed { .. }
            | Self::Refused(_)
            | Self::Usage(_)
            | Self::Interrupted
            | Self::TooManyFiles { .. }
            | Self::Reported
            | Self::I18n(_)
            | Self::Credentials(_)
            | Self::TranslationsMissing
            | Self::Deploy { .. }
            | Self::CargoFailed { .. } => None,
        }
    }
}
