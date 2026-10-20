//! Command-line parsing for generated application binaries.

use std::ffi::OsString;

use thiserror::Error;

/// Process command selected by the first argument.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Command {
    /// Run the HTTP server (the default).
    Serve,
    /// Probe the local liveness endpoint for container health checks.
    Healthcheck,
    /// Apply all pending embedded database migrations.
    Migrate,
    /// Revert the latest reversible database migration.
    Rollback,
    /// Print name and version.
    Version,
    /// Print usage.
    Help,
    /// An application command registered with `App::command`: its index in
    /// the registration list and its arguments.
    App(usize, Vec<String>),
    /// Run job workers or an operator command (feature `jobs`).
    #[cfg(feature = "jobs")]
    Jobs(JobsCommand),
}

/// Names of the built-in commands, which applications cannot replace.
pub(crate) const BUILT_IN: &[&str] = &[
    "serve",
    "healthcheck",
    "migrate",
    "rollback",
    "version",
    "help",
    "jobs",
];

/// `jobs` subcommand.
#[cfg(feature = "jobs")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum JobsCommand {
    /// Run workers, schedules and the outbox dispatcher until a signal.
    Work,
    /// Print job counts per queue and state.
    Status,
    /// List dead jobs, newest first.
    Dead {
        /// Maximum number listed.
        limit: u16,
    },
    /// Requeue a dead job.
    Retry(forge_jobs::JobId),
    /// Delete a dead job.
    Discard(forge_jobs::JobId),
    /// List dead outbox events (feature `events`).
    #[cfg(feature = "events")]
    DeadEvents {
        /// Maximum number listed.
        limit: u16,
    },
    /// Requeue a dead outbox event (feature `events`).
    #[cfg(feature = "events")]
    RetryEvent(forge_events::EventId),
}

/// Default number of dead jobs or events listed.
#[cfg(feature = "jobs")]
const DEFAULT_LIST_LIMIT: u16 = 50;

/// Invalid command line.
#[derive(Debug, Error, Eq, PartialEq)]
pub(crate) enum UsageError {
    /// The first argument is not a known command.
    #[error("unknown command {0:?}")]
    UnknownCommand(String),
    /// A command was followed by arguments it does not accept.
    #[error("unexpected argument {0:?}")]
    UnexpectedArgument(String),
    /// An application command argument is not valid UTF-8.
    #[error("arguments must be valid UTF-8")]
    NotUnicode,
    /// A required argument is missing.
    #[cfg(feature = "jobs")]
    #[error("missing {0}")]
    MissingArgument(&'static str),
    /// An argument is not a valid value.
    #[cfg(feature = "jobs")]
    #[error("invalid {what}: {value:?}")]
    InvalidArgument {
        /// What was expected.
        what: &'static str,
        /// The rejected argument.
        value: String,
    },
}

/// Parses process arguments, excluding the program name. `app_commands` are
/// the names registered with `App::command`, which take arguments.
pub(crate) fn parse(
    args: impl IntoIterator<Item = OsString>,
    app_commands: &[&str],
) -> Result<Command, UsageError> {
    let mut args = args.into_iter();
    let command = match args.next() {
        None => Command::Serve,
        Some(arg) => match arg.to_str() {
            Some("serve") => Command::Serve,
            Some("healthcheck") => Command::Healthcheck,
            Some("migrate") => Command::Migrate,
            Some("rollback") => Command::Rollback,
            Some("version" | "--version" | "-V") => Command::Version,
            Some("help" | "--help" | "-h") => Command::Help,
            #[cfg(feature = "jobs")]
            Some("jobs") => Command::Jobs(parse_jobs(&mut args)?),
            Some(name) if app_commands.contains(&name) => {
                let index = app_commands
                    .iter()
                    .position(|command| *command == name)
                    .unwrap_or_default();
                let arguments = args
                    .map(|argument| argument.into_string().map_err(|_| UsageError::NotUnicode))
                    .collect::<Result<Vec<_>, _>>()?;
                return Ok(Command::App(index, arguments));
            }
            _ => {
                return Err(UsageError::UnknownCommand(
                    arg.to_string_lossy().into_owned(),
                ));
            }
        },
    };
    if let Some(extra) = args.next() {
        return Err(UsageError::UnexpectedArgument(
            extra.to_string_lossy().into_owned(),
        ));
    }
    Ok(command)
}

#[cfg(feature = "jobs")]
fn parse_jobs(args: &mut impl Iterator<Item = OsString>) -> Result<JobsCommand, UsageError> {
    let Some(subcommand) = args.next() else {
        return Ok(JobsCommand::Work);
    };
    let text = |arg: OsString| arg.to_string_lossy().into_owned();
    let mut id = |what: &'static str| -> Result<String, UsageError> {
        args.next()
            .map(text)
            .ok_or(UsageError::MissingArgument(what))
    };
    match subcommand.to_str() {
        Some("work") => Ok(JobsCommand::Work),
        Some("status") => Ok(JobsCommand::Status),
        Some("dead") => Ok(JobsCommand::Dead {
            limit: parse_limit(args.next())?,
        }),
        Some("retry") => Ok(JobsCommand::Retry(parse_id("job id", id("job id")?)?)),
        Some("discard") => Ok(JobsCommand::Discard(parse_id("job id", id("job id")?)?)),
        #[cfg(feature = "events")]
        Some("dead-events") => Ok(JobsCommand::DeadEvents {
            limit: parse_limit(args.next())?,
        }),
        #[cfg(feature = "events")]
        Some("retry-event") => Ok(JobsCommand::RetryEvent(parse_id(
            "event id",
            id("event id")?,
        )?)),
        _ => Err(UsageError::UnknownCommand(format!(
            "jobs {}",
            text(subcommand)
        ))),
    }
}

#[cfg(feature = "jobs")]
fn parse_limit(arg: Option<OsString>) -> Result<u16, UsageError> {
    let Some(arg) = arg else {
        return Ok(DEFAULT_LIST_LIMIT);
    };
    let value = arg.to_string_lossy().into_owned();
    match value.parse::<u16>() {
        Ok(limit @ 1..=500) => Ok(limit),
        _ => Err(UsageError::InvalidArgument {
            what: "limit (1-500)",
            value,
        }),
    }
}

/// Parses a UUIDv7 identifier.
#[cfg(feature = "jobs")]
fn parse_id<T>(what: &'static str, value: String) -> Result<forge_core::Id<T>, UsageError> {
    value
        .parse()
        .map_err(|_| UsageError::InvalidArgument { what, value })
}

/// Usage lines of the `jobs` subcommands.
fn jobs_usage() -> &'static str {
    #[cfg(feature = "events")]
    {
        "  jobs [work]  Run job workers, cron schedules and the outbox dispatcher until SIGTERM/SIGINT
  jobs status  Print job counts per queue and state
  jobs dead [LIMIT]          List dead jobs, newest first (default 50, at most 500)
  jobs retry JOB_ID          Requeue a dead job with a fresh attempt budget
  jobs discard JOB_ID        Delete a dead job
  jobs dead-events [LIMIT]   List dead outbox events
  jobs retry-event EVENT_ID  Requeue a dead outbox event
"
    }
    #[cfg(all(feature = "jobs", not(feature = "events")))]
    {
        "  jobs [work]  Run job workers and cron schedules until SIGTERM/SIGINT
  jobs status  Print job counts per queue and state
  jobs dead [LIMIT]          List dead jobs, newest first (default 50, at most 500)
  jobs retry JOB_ID          Requeue a dead job with a fresh attempt budget
  jobs discard JOB_ID        Delete a dead job
"
    }
    #[cfg(not(feature = "jobs"))]
    {
        ""
    }
}

/// Human-readable usage, including the application's commands (name and
/// summary) and every supported configuration variable.
pub(crate) fn usage(name: &str, app_commands: &[(&str, &str)]) -> String {
    let mut commands = String::new();
    for (command, summary) in app_commands {
        commands.push_str(&format!("  {command:<12} {summary}\n"));
    }
    let jobs = jobs_usage();
    format!(
        "\
Usage: {name} [COMMAND] [ARGS]

Commands:
  serve        Run the HTTP server (default)
  healthcheck  Probe GET /health/live on the configured address; exit 0 when healthy
  migrate      Apply all pending embedded database migrations
  rollback     Revert the latest reversible database migration
{commands}  version      Print the application name and version (also --version, -V)
  help         Print this help (also --help, -h)
{jobs}
Environment:
  FORGE_ENV                   development | test | production (default: development)
  FORGE_BIND                  listen socket address (default: 127.0.0.1:3000)
  FORGE_REQUEST_TIMEOUT_SECS  per-request deadline in seconds (default: 30)
  FORGE_SHUTDOWN_GRACE_SECS   connection drain period on shutdown in seconds (default: 15)
  FORGE_MAX_BODY_BYTES        maximum request body size in bytes (default: 1048576)
  FORGE_MAX_CONNECTIONS       maximum concurrent connections (default: 10000)
  FORGE_HTTP_TRUSTED_PROXIES  comma-separated CIDRs or addresses of reverse proxies whose
                              X-Forwarded-For is believed (default: empty, trust nobody)
  FORGE_LOG                   log filter directives (default: info)
  FORGE_LOG_FORMAT            json | text (default: json in production, text otherwise)
  FORGE_DATABASE_URL           least-privilege runtime URL: postgres://, mysql://, mariadb:// or sqlite:
  FORGE_MIGRATION_DATABASE_URL migration URL, same schemes (required for migrate/rollback
                               and application commands that use the database)
  FORGE_SETUP_PASSWORD         owner password for setup when not typed or piped (never for serve)
  FORGE_RATE_LIMIT_MAX_KEYS   live buckets of the in-memory rate limiter (default: 100000)
  FORGE_CACHE_MAX_ENTRIES     entry bound of every cache (default: 10000)
  FORGE_CACHE_MAX_BYTES       memory bound of the in-memory cache (default: 67108864)
  FORGE_CACHE_MAX_VALUE_BYTES largest cached value, at most 8388608 (default: 1048576)
  FORGE_CACHE_DEFAULT_TTL_SECS ttl of writes that pass none (default: 3600)
  FORGE_STORAGE_ROOT          disk storage directory (absolute in production)
  FORGE_STORAGE_SIGNING_KEY   unpadded base64url of 32+ bytes; required in production with a root
  FORGE_STORAGE_MAX_UPLOAD_BYTES largest uploaded file (default: 524288)
  FORGE_STORAGE_LINK_TTL_SECS default signed link lifetime (default: 900)
  FORGE_STREAMS_KEY           base64 of 32+ bytes signing live-update stream names; required in production when streams are served

Unknown FORGE_* variables are rejected at startup.
"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    fn parse_builtin(args: Vec<OsString>) -> Result<Command, UsageError> {
        parse(args, &[])
    }

    #[test]
    fn application_commands_take_arguments() {
        assert_eq!(
            parse(args(&["seed"]), &["setup", "seed"]),
            Ok(Command::App(1, Vec::new()))
        );
        assert_eq!(
            parse(args(&["setup", "--tenant", "acme"]), &["setup"]),
            Ok(Command::App(
                0,
                vec!["--tenant".to_owned(), "acme".to_owned()]
            ))
        );
        assert_eq!(
            parse(args(&["setup"]), &[]),
            Err(UsageError::UnknownCommand("setup".to_owned()))
        );
    }

    #[test]
    fn no_argument_serves() {
        assert_eq!(parse_builtin(args(&[])), Ok(Command::Serve));
        assert_eq!(parse_builtin(args(&["serve"])), Ok(Command::Serve));
    }

    #[test]
    fn known_commands_and_aliases_are_recognized() {
        assert_eq!(
            parse_builtin(args(&["healthcheck"])),
            Ok(Command::Healthcheck)
        );
        assert_eq!(parse_builtin(args(&["migrate"])), Ok(Command::Migrate));
        assert_eq!(parse_builtin(args(&["rollback"])), Ok(Command::Rollback));
        for alias in ["version", "--version", "-V"] {
            assert_eq!(
                parse_builtin(args(&[alias])),
                Ok(Command::Version),
                "{alias}"
            );
        }
        for alias in ["help", "--help", "-h"] {
            assert_eq!(parse_builtin(args(&[alias])), Ok(Command::Help), "{alias}");
        }
    }

    #[test]
    fn unknown_commands_are_rejected() {
        assert_eq!(
            parse_builtin(args(&["frobnicate"])),
            Err(UsageError::UnknownCommand("frobnicate".to_owned()))
        );
        assert_eq!(
            parse_builtin(args(&["-v"])),
            Err(UsageError::UnknownCommand("-v".to_owned()))
        );
    }

    #[test]
    fn trailing_arguments_are_rejected() {
        assert_eq!(
            parse_builtin(args(&["serve", "--port"])),
            Err(UsageError::UnexpectedArgument("--port".to_owned()))
        );
    }

    #[cfg(unix)]
    #[test]
    fn non_unicode_arguments_are_rejected() {
        use std::os::unix::ffi::OsStringExt;

        let result = parse_builtin(vec![OsString::from_vec(vec![0xff])]);

        assert!(matches!(result, Err(UsageError::UnknownCommand(_))));
    }

    #[cfg(feature = "jobs")]
    #[test]
    fn jobs_subcommands_parse_and_validate_their_arguments() {
        let id = forge_jobs::JobId::new();
        let text = id.to_string();
        assert_eq!(
            parse_builtin(args(&["jobs"])),
            Ok(Command::Jobs(JobsCommand::Work))
        );
        assert_eq!(
            parse_builtin(args(&["jobs", "work"])),
            Ok(Command::Jobs(JobsCommand::Work))
        );
        assert_eq!(
            parse_builtin(args(&["jobs", "status"])),
            Ok(Command::Jobs(JobsCommand::Status))
        );
        assert_eq!(
            parse_builtin(args(&["jobs", "dead"])),
            Ok(Command::Jobs(JobsCommand::Dead { limit: 50 }))
        );
        assert_eq!(
            parse_builtin(args(&["jobs", "dead", "7"])),
            Ok(Command::Jobs(JobsCommand::Dead { limit: 7 }))
        );
        assert_eq!(
            parse_builtin(args(&["jobs", "retry", &text])),
            Ok(Command::Jobs(JobsCommand::Retry(id)))
        );
        assert_eq!(
            parse_builtin(args(&["jobs", "discard", &text])),
            Ok(Command::Jobs(JobsCommand::Discard(id)))
        );
        assert_eq!(
            parse_builtin(args(&["jobs", "retry"])),
            Err(UsageError::MissingArgument("job id"))
        );
        // UUIDv4 and garbage are rejected.
        for bad in ["not-a-uuid", "0b9b6c2e-6d1e-4c3a-9f5e-2a7c1d3e4f50"] {
            assert!(matches!(
                parse_builtin(args(&["jobs", "retry", bad])),
                Err(UsageError::InvalidArgument { what: "job id", .. })
            ));
        }
        for bad in ["0", "501", "x"] {
            assert!(
                parse_builtin(args(&["jobs", "dead", bad])).is_err(),
                "{bad}"
            );
        }
        assert!(matches!(
            parse_builtin(args(&["jobs", "status", "extra"])),
            Err(UsageError::UnexpectedArgument(_))
        ));
        assert!(matches!(
            parse_builtin(args(&["jobs", "frobnicate"])),
            Err(UsageError::UnknownCommand(_))
        ));
        #[cfg(feature = "events")]
        {
            let event = forge_events::EventId::new();
            assert_eq!(
                parse_builtin(args(&["jobs", "retry-event", &event.to_string()])),
                Ok(Command::Jobs(JobsCommand::RetryEvent(event)))
            );
            assert_eq!(
                parse_builtin(args(&["jobs", "dead-events", "3"])),
                Ok(Command::Jobs(JobsCommand::DeadEvents { limit: 3 }))
            );
        }
        let usage = usage("demo", &[]);
        for line in [
            "jobs [work]",
            "jobs status",
            "jobs dead",
            "jobs retry",
            "jobs discard",
        ] {
            assert!(usage.contains(line), "{line}");
        }
    }

    #[test]
    fn usage_documents_every_configuration_key() {
        let text = usage("demo", &[("setup", "Create the first tenant and owner")]);

        assert!(text.starts_with("Usage: demo [COMMAND]"));
        assert!(text.contains("  setup        Create the first tenant and owner\n"));
        for command in [
            "serve",
            "healthcheck",
            "migrate",
            "rollback",
            "version",
            "help",
        ] {
            assert!(text.contains(command), "{command}");
        }
        for key in forge_config::KNOWN_KEYS {
            assert!(text.contains(key), "{key}");
        }
    }
}
