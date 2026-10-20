//! Command-line parsing for generated application binaries.

use std::ffi::OsString;
use std::path::PathBuf;

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
    /// List every migration and whether it is applied, without applying any.
    MigrationStatus,
    /// Revert the latest `steps` reversible database migrations.
    Rollback {
        /// How many migrations to revert, at least one.
        steps: u32,
    },
    /// Server-level administration of the application's database.
    Db(DbCommand),
    /// Print, write or check the OpenAPI document.
    OpenApi(OpenApiOutput),
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

/// `db` subcommand.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DbCommand {
    /// Create the database unless it exists.
    Create,
    /// Drop the database and everything in it.
    Drop {
        /// The caller confirmed a drop outside development and test, or of a
        /// server that is not on this machine.
        force_production: bool,
    },
    /// Exit 0 when the database exists and 3 when it does not.
    Exists,
}

/// Where `openapi` puts the document.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum OpenApiOutput {
    /// Standard output.
    Stdout,
    /// `--output FILE`: write it.
    Output(PathBuf),
    /// `--check FILE`: exit 1 when FILE differs.
    Check(PathBuf),
}

/// Names of the built-in commands, which applications cannot replace.
pub(crate) const BUILT_IN: &[&str] = &[
    "serve",
    "healthcheck",
    "migrate",
    "rollback",
    "db",
    "openapi",
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

/// Most migrations one `rollback --steps` reverts.
const MAX_ROLLBACK_STEPS: u32 = 1000;

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
    #[error("missing {0}")]
    MissingArgument(&'static str),
    /// An argument is not a valid value.
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
            Some("migrate") => match args.next() {
                None => Command::Migrate,
                Some(argument) if argument.to_str() == Some("status") => Command::MigrationStatus,
                Some(argument) => {
                    return Err(UsageError::UnexpectedArgument(
                        argument.to_string_lossy().into_owned(),
                    ));
                }
            },
            Some("rollback") => Command::Rollback {
                steps: parse_steps(&mut args)?,
            },
            Some("db") => Command::Db(parse_db(&mut args)?),
            Some("openapi") => Command::OpenApi(parse_openapi(&mut args)?),
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

/// `[--steps N]` after `rollback`; one step by default.
fn parse_steps(args: &mut impl Iterator<Item = OsString>) -> Result<u32, UsageError> {
    let Some(flag) = args.next() else {
        return Ok(1);
    };
    let flag = flag.to_string_lossy().into_owned();
    let value = if flag == "--steps" {
        args.next()
            .map(|value| value.to_string_lossy().into_owned())
            .ok_or(UsageError::MissingArgument("--steps value"))?
    } else if let Some(value) = flag.strip_prefix("--steps=") {
        value.to_owned()
    } else {
        return Err(UsageError::UnexpectedArgument(flag));
    };
    match value.parse::<u32>() {
        Ok(steps @ 1..=MAX_ROLLBACK_STEPS) => Ok(steps),
        _ => Err(UsageError::InvalidArgument {
            what: "--steps (1-1000)",
            value,
        }),
    }
}

/// `[--output FILE | --check FILE]` after `openapi` (also `--output=FILE`).
fn parse_openapi(args: &mut impl Iterator<Item = OsString>) -> Result<OpenApiOutput, UsageError> {
    let Some(flag) = args.next() else {
        return Ok(OpenApiOutput::Stdout);
    };
    let text = flag.to_string_lossy().into_owned();
    let (name, value) = match text.split_once('=') {
        Some((name, value)) => (name.to_owned(), Some(OsString::from(value))),
        None => (text.clone(), None),
    };
    let wrap: fn(PathBuf) -> OpenApiOutput = match name.as_str() {
        "--output" => OpenApiOutput::Output,
        "--check" => OpenApiOutput::Check,
        _ => return Err(UsageError::UnexpectedArgument(text)),
    };
    let file = match value {
        Some(value) => value,
        None => args.next().ok_or(UsageError::MissingArgument(
            "FILE after --output or --check",
        ))?,
    };
    if file.is_empty() {
        return Err(UsageError::MissingArgument(
            "FILE after --output or --check",
        ));
    }
    Ok(wrap(PathBuf::from(file)))
}

/// `create`, `exists` or `drop [--force-production]` after `db`.
fn parse_db(args: &mut impl Iterator<Item = OsString>) -> Result<DbCommand, UsageError> {
    let Some(subcommand) = args.next() else {
        return Err(UsageError::MissingArgument(
            "db command (create, drop, exists)",
        ));
    };
    match subcommand.to_str() {
        Some("create") => Ok(DbCommand::Create),
        Some("exists") => Ok(DbCommand::Exists),
        Some("drop") => match args.next() {
            None => Ok(DbCommand::Drop {
                force_production: false,
            }),
            Some(flag) if flag.to_str() == Some("--force-production") => Ok(DbCommand::Drop {
                force_production: true,
            }),
            Some(other) => Err(UsageError::UnexpectedArgument(
                other.to_string_lossy().into_owned(),
            )),
        },
        _ => Err(UsageError::UnknownCommand(format!(
            "db {}",
            subcommand.to_string_lossy()
        ))),
    }
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
  migrate status               List every migration as applied, pending, modified, disabled or missing
  rollback     Revert the latest reversible database migration (--steps N reverts N)
  db create    Create the database of FORGE_MIGRATION_DATABASE_URL unless it exists
  db exists    Exit 0 when that database exists, 3 when it does not
  db drop      Drop that database; outside development and test, or for a server that is not
               on this machine, only with --force-production
  openapi      Print the OpenAPI document; --output FILE writes it, --check FILE exits 1
               when FILE is out of date (logs go to standard error)
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
  FORGE_I18N_DEFAULT_LOCALE   default locale, spelled canonically (en, pt-BR); needs config/locales/LOCALE.json
                              (default: the application's, set by forge new --locale)
  FORGE_OPENAPI_ENABLED       true serves GET /openapi.json in production too; false never
                              (default: development and test only)
  FORGE_MAIL_URL              smtp://user:pass@host:587 (STARTTLS), smtps://user:pass@host:465 (implicit TLS)
                              or file:tmp/mail (development .eml files; refused in production) (default: mail off)
  FORGE_MAIL_FROM             default sender, App <app@example.com>; required with FORGE_MAIL_URL
  FORGE_MAIL_ALLOW_PLAINTEXT  true continues without STARTTLS; credentials still go only over TLS or to loopback
  FORGE_MAIL_TIMEOUT          one delivery attempt, at most 10m (default: 30s)
  FORGE_MAIL_PREVIEW          true serves mail previews (development and test only; default: false)
  FORGE_MAIL_TLS_CA_FILE      PEM roots trusted in addition to the Mozilla roots (private relays)
  FORGE_AUTH_PASSWORD_RESET_URL public URL of /passwords/edit for reset emails (https; http only on localhost)
  FORGE_TELEMETRY_ENABLED     false installs no tracing, metrics or log export (default: true)
  FORGE_TELEMETRY_SERVICE_NAME    service.name (default: the application name)
  FORGE_TELEMETRY_SERVICE_VERSION service.version (default: the application version)
  FORGE_TELEMETRY_EXPORTER    none | otlp | log (spans and pushed metrics; log is refused in production; default: none)
  FORGE_TELEMETRY_LOGS_EXPORTER   none | otlp (log records, in addition to stdout; default: none)
  FORGE_TELEMETRY_LOGS_LEVEL  debug | info | warn | error, lowest exported level (default: info)
  FORGE_TELEMETRY_OTLP_ENDPOINT   collector base URL; https required in production
  FORGE_TELEMETRY_OTLP_HEADERS    name=value pairs, values percent-encoded and secret
  FORGE_TELEMETRY_OTLP_TIMEOUT    export deadline, retries included, at most 1m (default: 10s)
  FORGE_TELEMETRY_OTLP_COMPRESSION none | gzip (default: none)
  FORGE_TELEMETRY_OTLP_ALLOW_PLAINTEXT true permits an http:// endpoint in production
  FORGE_TELEMETRY_SAMPLE_RATIO    fraction of new traces sampled, 0 to 1 (default: 1)
  FORGE_TELEMETRY_TRUST_TRACEPARENT true continues incoming traceparent (default: false)
  FORGE_TELEMETRY_METRICS_ADDR    separate Prometheus listener serving GET /metrics (default: off)
  FORGE_TELEMETRY_METRICS_PUBLIC  true confirms a non-loopback metrics address
  FORGE_TELEMETRY_METRICS_INTERVAL OTLP metric push interval, at least 1s (default: 1m)

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
    fn openapi_takes_one_output_or_check_file() {
        assert_eq!(
            parse_builtin(args(&["openapi"])),
            Ok(Command::OpenApi(OpenApiOutput::Stdout))
        );
        assert_eq!(
            parse_builtin(args(&["openapi", "--output", "openapi.json"])),
            Ok(Command::OpenApi(OpenApiOutput::Output(
                "openapi.json".into()
            )))
        );
        assert_eq!(
            parse_builtin(args(&["openapi", "--check=api.json"])),
            Ok(Command::OpenApi(OpenApiOutput::Check("api.json".into())))
        );
        assert!(parse_builtin(args(&["openapi", "--output"])).is_err());
        assert!(parse_builtin(args(&["openapi", "--output="])).is_err());
        assert!(parse_builtin(args(&["openapi", "--yaml"])).is_err());
        assert_eq!(
            parse_builtin(args(&["openapi", "--output", "a", "--check", "b"])),
            Err(UsageError::UnexpectedArgument("--check".to_owned()))
        );
        assert!(BUILT_IN.contains(&"openapi"));
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
        assert_eq!(
            parse_builtin(args(&["rollback"])),
            Ok(Command::Rollback { steps: 1 })
        );
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
    fn migrate_status_and_rollback_steps_parse_and_validate() {
        assert_eq!(
            parse_builtin(args(&["migrate", "status"])),
            Ok(Command::MigrationStatus)
        );
        assert_eq!(
            parse_builtin(args(&["migrate", "now"])),
            Err(UsageError::UnexpectedArgument("now".to_owned()))
        );
        assert_eq!(
            parse_builtin(args(&["rollback", "--steps", "3"])),
            Ok(Command::Rollback { steps: 3 })
        );
        assert_eq!(
            parse_builtin(args(&["rollback", "--steps=2"])),
            Ok(Command::Rollback { steps: 2 })
        );
        for bad in ["0", "1001", "-1", "x", ""] {
            assert!(
                matches!(
                    parse_builtin(args(&["rollback", "--steps", bad])),
                    Err(UsageError::InvalidArgument { .. })
                ),
                "{bad:?}"
            );
        }
        assert_eq!(
            parse_builtin(args(&["rollback", "--steps"])),
            Err(UsageError::MissingArgument("--steps value"))
        );
        assert_eq!(
            parse_builtin(args(&["rollback", "3"])),
            Err(UsageError::UnexpectedArgument("3".to_owned()))
        );
        assert_eq!(
            parse_builtin(args(&["rollback", "--steps", "2", "x"])),
            Err(UsageError::UnexpectedArgument("x".to_owned()))
        );
    }

    #[test]
    fn db_subcommands_parse_and_validate() {
        assert_eq!(
            parse_builtin(args(&["db", "create"])),
            Ok(Command::Db(DbCommand::Create))
        );
        assert_eq!(
            parse_builtin(args(&["db", "exists"])),
            Ok(Command::Db(DbCommand::Exists))
        );
        assert_eq!(
            parse_builtin(args(&["db", "drop"])),
            Ok(Command::Db(DbCommand::Drop {
                force_production: false
            }))
        );
        assert_eq!(
            parse_builtin(args(&["db", "drop", "--force-production"])),
            Ok(Command::Db(DbCommand::Drop {
                force_production: true
            }))
        );
        assert!(matches!(
            parse_builtin(args(&["db"])),
            Err(UsageError::MissingArgument(_))
        ));
        assert!(matches!(
            parse_builtin(args(&["db", "reset"])),
            Err(UsageError::UnknownCommand(_))
        ));
        assert!(matches!(
            parse_builtin(args(&["db", "drop", "--yes"])),
            Err(UsageError::UnexpectedArgument(_))
        ));
        assert!(matches!(
            parse_builtin(args(&["db", "create", "--force-production"])),
            Err(UsageError::UnexpectedArgument(_))
        ));
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
