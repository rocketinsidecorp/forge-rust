//! What every `CliError` says, whether it has a source, and the exit codes.

use std::error::Error as _;
use std::io;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use crate::CliError;
use crate::deploy::DeployError;

fn io_error(message: &str) -> io::Error {
    io::Error::other(message.to_owned())
}

#[test]
fn every_error_says_what_went_wrong_in_one_line() {
    let path = PathBuf::from("/app/file");
    let clock = SystemTime::UNIX_EPOCH
        .duration_since(SystemTime::UNIX_EPOCH + Duration::from_secs(1))
        .expect_err("a time before the epoch");
    let cases: Vec<(CliError, &str)> = vec![
        (
            CliError::CurrentDirectory(io_error("gone")),
            "could not determine the current directory: gone",
        ),
        (
            CliError::InvalidApplicationName {
                name: "A b".to_owned(),
                reason: "no spaces",
            },
            "invalid application name `A b`: no spaces",
        ),
        (
            CliError::InvalidForgePath {
                path: "x y".to_owned(),
                reason: "no spaces",
            },
            "invalid --forge-path \"x y\": no spaces",
        ),
        (
            CliError::InvalidMigrationName {
                name: "Bad".to_owned(),
                reason: "lowercase",
            },
            "invalid migration name \"Bad\": lowercase",
        ),
        (
            CliError::Clock(clock),
            "system clock is before the Unix epoch: second time provided was later than self",
        ),
        (
            CliError::Scaffold("no views".to_owned()),
            "cannot generate the scaffold: no views",
        ),
        (
            CliError::Generate {
                generator: "job",
                reason: "no queue".to_owned(),
            },
            "cannot generate the job: no queue",
        ),
        (
            CliError::Destroy("nothing to destroy".to_owned()),
            "nothing to destroy",
        ),
        (
            CliError::Authentication("no views".to_owned()),
            "cannot generate authentication: no views",
        ),
        (
            CliError::DestinationExists(path.clone()),
            "destination `/app/file` already exists; refusing to overwrite it",
        ),
        (
            CliError::CreateDirectory {
                path: path.clone(),
                source: io_error("denied"),
            },
            "could not create `/app/file`: denied",
        ),
        (
            CliError::WriteFile {
                path: path.clone(),
                source: io_error("full"),
            },
            "could not write `/app/file`: full",
        ),
        (
            CliError::ReadDirectory {
                path: path.clone(),
                source: io_error("io"),
            },
            "could not read `/app/file`: io",
        ),
        (
            CliError::SpawnCargo {
                command: "check",
                source: io_error("missing"),
            },
            "could not run `cargo check`: missing",
        ),
        (
            CliError::CargoFailed {
                command: "test",
                code: Some(101),
            },
            "`cargo test` failed with exit code 101",
        ),
        (
            CliError::CargoFailed {
                command: "test",
                code: None,
            },
            "`cargo test` was terminated by a signal",
        ),
        (
            CliError::Output(io_error("closed")),
            "could not write command output: closed",
        ),
        (
            CliError::NotAnApplication("no Cargo.toml".to_owned()),
            "no Cargo.toml: run this inside a Forge application",
        ),
        (
            CliError::SpawnProcess {
                program: "app".to_owned(),
                source: io_error("noexec"),
            },
            "could not run `app`: noexec",
        ),
        (
            CliError::NoBinary {
                name: Some("shop".to_owned()),
            },
            "`cargo build` produced no binary named `shop`",
        ),
        (
            CliError::NoBinary { name: None },
            "`cargo build` produced no binary",
        ),
        (
            CliError::AppCommandFailed {
                command: "migrate".to_owned(),
                code: Some(2),
            },
            "application command \"migrate\" failed with status 2",
        ),
        (
            CliError::AppCommandFailed {
                command: "migrate".to_owned(),
                code: None,
            },
            "application command \"migrate\" was terminated by a signal",
        ),
        (
            CliError::Refused("not confirmed".to_owned()),
            "not confirmed",
        ),
        (CliError::Usage("bad flag".to_owned()), "bad flag"),
        (CliError::Interrupted, "interrupted"),
        (
            CliError::Signals(io_error("no handler")),
            "could not install the signal handlers: no handler",
        ),
        (
            CliError::TooManyFiles {
                root: path.clone(),
                limit: 5,
            },
            "more than 5 watched files under `/app/file`: is a build output directory missing from the ignore rules?",
        ),
        (CliError::Reported, "checks failed"),
        (CliError::I18n("unreadable".to_owned()), "unreadable"),
        (CliError::TranslationsMissing, "translations are missing"),
        (CliError::Credentials("no key".to_owned()), "no key"),
        (
            CliError::Deploy {
                message: "lock held".to_owned(),
                code: 3,
            },
            "lock held",
        ),
    ];
    for (error, want) in cases {
        assert_eq!(error.to_string(), want);
    }
}

#[test]
fn errors_wrapping_an_io_error_expose_it_and_the_others_do_not() {
    let wrapping = [
        CliError::CurrentDirectory(io_error("a")),
        CliError::Output(io_error("a")),
        CliError::Signals(io_error("a")),
        CliError::SpawnProcess {
            program: "p".to_owned(),
            source: io_error("a"),
        },
        CliError::CreateDirectory {
            path: PathBuf::new(),
            source: io_error("a"),
        },
        CliError::WriteFile {
            path: PathBuf::new(),
            source: io_error("a"),
        },
        CliError::ReadDirectory {
            path: PathBuf::new(),
            source: io_error("a"),
        },
        CliError::SpawnCargo {
            command: "check",
            source: io_error("a"),
        },
    ];
    for error in wrapping {
        assert_eq!(
            error.source().map(ToString::to_string).as_deref(),
            Some("a"),
            "{error}"
        );
    }
    let clock = SystemTime::UNIX_EPOCH
        .duration_since(SystemTime::UNIX_EPOCH + Duration::from_secs(1))
        .expect_err("a time before the epoch");
    assert!(CliError::Clock(clock).source().is_some());
    let plain = [
        CliError::InvalidApplicationName {
            name: String::new(),
            reason: "r",
        },
        CliError::InvalidForgePath {
            path: String::new(),
            reason: "r",
        },
        CliError::InvalidMigrationName {
            name: String::new(),
            reason: "r",
        },
        CliError::Scaffold(String::new()),
        CliError::Generate {
            generator: "job",
            reason: String::new(),
        },
        CliError::Destroy(String::new()),
        CliError::Authentication(String::new()),
        CliError::DestinationExists(PathBuf::new()),
        CliError::NotAnApplication(String::new()),
        CliError::NoBinary { name: None },
        CliError::AppCommandFailed {
            command: String::new(),
            code: None,
        },
        CliError::Refused(String::new()),
        CliError::Usage(String::new()),
        CliError::Interrupted,
        CliError::TooManyFiles {
            root: PathBuf::new(),
            limit: 1,
        },
        CliError::Reported,
        CliError::I18n(String::new()),
        CliError::Credentials(String::new()),
        CliError::TranslationsMissing,
        CliError::Deploy {
            message: String::new(),
            code: 1,
        },
        CliError::CargoFailed {
            command: "check",
            code: None,
        },
    ];
    for error in plain {
        assert!(error.source().is_none(), "{error:?}");
    }
}

#[test]
fn only_the_deploy_commands_exit_with_their_own_codes() {
    assert_eq!(CliError::Interrupted.exit_code(), 1);
    assert_eq!(CliError::Reported.exit_code(), 1);
    for code in [3, 4, 5, 6, 7] {
        assert_eq!(
            CliError::Deploy {
                message: String::new(),
                code
            }
            .exit_code(),
            code
        );
    }
    let converted = CliError::from(DeployError::new("migration failed").with_code(5));
    assert_eq!(converted.exit_code(), 5);
    assert_eq!(converted.to_string(), "migration failed");
}
