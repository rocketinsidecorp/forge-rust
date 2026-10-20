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
        }
    }
}

impl std::error::Error for CliError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::CurrentDirectory(error)
            | Self::Output(error)
            | Self::CreateDirectory { source: error, .. }
            | Self::WriteFile { source: error, .. }
            | Self::ReadDirectory { source: error, .. }
            | Self::SpawnCargo { source: error, .. } => Some(error),
            Self::Clock(error) => Some(error),
            Self::InvalidApplicationName { .. }
            | Self::InvalidForgePath { .. }
            | Self::InvalidMigrationName { .. }
            | Self::Scaffold(_)
            | Self::Authentication(_)
            | Self::DestinationExists(_)
            | Self::CargoFailed { .. } => None,
        }
    }
}
