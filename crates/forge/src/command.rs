//! Application commands: administrative subcommands such as `setup` and
//! `seed` that an application registers with [`App::command`](crate::App::command).
//!
//! A command runs once and exits. It receives a [`CommandContext`] with its
//! arguments, the validated configuration and, when the application declared
//! a database, a pool connected with the **migration** credential
//! (`FORGE_MIGRATION_DATABASE_URL`): commands create first accounts and seed
//! data, which the least-privilege runtime role must not be able to do. The
//! serving process never receives that credential.
//!
//! Secrets such as passwords are never accepted as arguments (process lists
//! and shell history expose them): [`CommandContext::read_secret`] takes one
//! line of piped standard input, a known `FORGE_*` variable, or a terminal
//! prompt with echo turned off.

use std::{
    collections::BTreeMap,
    error::Error as StdError,
    future::Future,
    io::{self, BufRead, Write},
    pin::Pin,
};

use forge_config::AppConfig;
use thiserror::Error;

use crate::database::DatabaseHandle;

/// Boxed future of a running command.
pub(crate) type CommandFuture = Pin<Box<dyn Future<Output = Result<(), CommandError>> + Send>>;
/// A registered command body.
pub(crate) type CommandHandler = Box<dyn FnOnce(CommandContext) -> CommandFuture + Send>;

/// One command registered with `App::command`.
pub(crate) struct RegisteredCommand {
    pub(crate) name: &'static str,
    pub(crate) summary: &'static str,
    pub(crate) handler: CommandHandler,
}

/// Whether `name` is a valid command name: lowercase ASCII letters, digits
/// and `-`, starting with a letter, at most 32 bytes.
pub(crate) fn valid_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    (1..=32).contains(&bytes.len())
        && bytes.first().is_some_and(u8::is_ascii_lowercase)
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
}

type HiddenPrompt = Box<dyn FnMut(&str) -> io::Result<String> + Send>;
type EnvLookup = Box<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// Everything a running command may use.
pub struct CommandContext {
    app: &'static str,
    command: &'static str,
    arguments: Vec<String>,
    config: AppConfig,
    database: Option<DatabaseHandle>,
    input: Box<dyn BufRead + Send>,
    output: Box<dyn Write + Send>,
    terminal: bool,
    hidden_prompt: HiddenPrompt,
    environment: EnvLookup,
}

impl std::fmt::Debug for CommandContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CommandContext")
            .field("app", &self.app)
            .field("command", &self.command)
            .field("arguments", &self.arguments.len())
            .field("database", &self.database)
            .finish_non_exhaustive()
    }
}

impl CommandContext {
    /// A context reading the process's standard input and environment and
    /// writing to standard output.
    pub(crate) fn for_process(
        app: &'static str,
        command: &'static str,
        arguments: Vec<String>,
        config: AppConfig,
        database: Option<DatabaseHandle>,
    ) -> Self {
        use std::io::IsTerminal as _;

        Self {
            app,
            command,
            arguments,
            config,
            database,
            terminal: io::stdin().is_terminal(),
            input: Box::new(io::BufReader::new(io::stdin())),
            output: Box::new(io::stdout()),
            hidden_prompt: Box::new(crate::console::prompt_hidden),
            environment: Box::new(|key| std::env::var(key).ok()),
        }
    }

    /// The application's name, as given to `App::new`.
    #[must_use]
    pub fn app_name(&self) -> &'static str {
        self.app
    }

    /// The command's arguments (everything after its name).
    #[must_use]
    pub fn arguments(&self) -> &[String] {
        &self.arguments
    }

    /// The validated process configuration.
    #[must_use]
    pub fn config(&self) -> &AppConfig {
        &self.config
    }

    /// Parses the arguments as `--name value` / `--name=value` options
    /// (each at most once) and value-less `--flag`s. Positional arguments,
    /// unknown or repeated options are usage errors, and so is anything that
    /// looks like a password argument.
    pub fn options(&self, values: &[&str], flags: &[&str]) -> Result<CommandOptions, CommandError> {
        CommandOptions::parse(&self.arguments, values, flags)
    }

    /// Writes one line to standard output.
    pub fn say(&mut self, line: &str) -> Result<(), CommandError> {
        writeln!(self.output, "{line}")
            .and_then(|()| self.output.flush())
            .map_err(|error| CommandError::failed(format!("cannot write output: {error}")))
    }

    /// Reads a secret (a password) without ever taking it from an argument.
    ///
    /// - With `from_stdin` (the command's `--password-stdin` flag): one line
    ///   of standard input, without its line ending.
    /// - Otherwise, when the environment variable `variable` is set: its
    ///   value. `variable` must be a known `FORGE_*` key (configuration
    ///   loading rejects unknown ones).
    /// - Otherwise, when standard input is a terminal: `prompt` on standard
    ///   error, typed twice with echo off; both entries must match.
    ///
    /// Anything else is an error that says how to provide the secret. The
    /// secret is never written anywhere.
    pub fn read_secret(
        &mut self,
        variable: &str,
        from_stdin: bool,
        prompt: &str,
    ) -> Result<String, CommandError> {
        let from_environment = (self.environment)(variable);
        if from_stdin {
            if from_environment.is_some() {
                return Err(CommandError::usage(format!(
                    "use either --password-stdin or {variable}, not both"
                )));
            }
            let mut line = String::new();
            self.input.read_line(&mut line).map_err(|error| {
                CommandError::failed(format!("cannot read standard input: {error}"))
            })?;
            let line = line
                .strip_suffix('\n')
                .map(|line| line.strip_suffix('\r').unwrap_or(line))
                .unwrap_or(&line);
            if line.is_empty() {
                return Err(CommandError::usage(
                    "--password-stdin read an empty line; pipe the password followed by a newline",
                ));
            }
            return Ok(line.to_owned());
        }
        if let Some(value) = from_environment {
            return Ok(value);
        }
        if !self.terminal {
            return Err(CommandError::usage(format!(
                "no password given: run in a terminal to type it, pipe one line with \
                 --password-stdin, or set {variable}; passwords are never accepted as arguments"
            )));
        }
        let first = (self.hidden_prompt)(&format!("{prompt}: "))
            .map_err(|error| CommandError::failed(format!("cannot read the password: {error}")))?;
        let second = (self.hidden_prompt)(&format!("{prompt} (again): "))
            .map_err(|error| CommandError::failed(format!("cannot read the password: {error}")))?;
        if first != second {
            return Err(CommandError::usage("the two passwords do not match"));
        }
        Ok(first)
    }

    /// The PostgreSQL pool, connected with `FORGE_MIGRATION_DATABASE_URL`.
    #[cfg(feature = "postgres")]
    pub fn postgres(&self) -> Result<&forge_db::PgDatabase, CommandError> {
        match &self.database {
            Some(DatabaseHandle::Postgres(database)) => Ok(database),
            _ => Err(undeclared("postgres")),
        }
    }

    /// The MySQL/MariaDB pool, connected with `FORGE_MIGRATION_DATABASE_URL`.
    #[cfg(feature = "mysql")]
    pub fn mysql(&self) -> Result<&forge_db::MySqlDatabase, CommandError> {
        match &self.database {
            Some(DatabaseHandle::MySql(database)) => Ok(database),
            _ => Err(undeclared("mysql")),
        }
    }

    /// The SQLite pool, connected with `FORGE_MIGRATION_DATABASE_URL`.
    #[cfg(feature = "sqlite")]
    pub fn sqlite(&self) -> Result<&forge_db::SqliteDatabase, CommandError> {
        match &self.database {
            Some(DatabaseHandle::Sqlite(database)) => Ok(database),
            _ => Err(undeclared("sqlite")),
        }
    }
}

#[cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]
fn undeclared(backend: &'static str) -> CommandError {
    CommandError::failed(format!(
        "this command needs the {backend} database, but the application did not declare it with \
         App::{backend}"
    ))
}

/// Test support: run a command body without the process lifecycle.
#[cfg(feature = "testing")]
impl CommandContext {
    /// A context with `arguments`, no database, an empty standard input, no
    /// environment variables and no terminal; output is discarded.
    #[must_use]
    pub fn for_testing(arguments: &[&str]) -> Self {
        Self {
            app: "test",
            command: "test",
            arguments: arguments
                .iter()
                .map(|argument| (*argument).to_owned())
                .collect(),
            config: AppConfig::default(),
            database: None,
            input: Box::new(io::empty()),
            output: Box::new(io::sink()),
            terminal: false,
            hidden_prompt: Box::new(|_| Err(io::Error::from(io::ErrorKind::Unsupported))),
            environment: Box::new(|_| None),
        }
    }

    /// Feeds `input` as standard input.
    #[must_use]
    pub fn with_stdin(mut self, input: &str) -> Self {
        self.input = Box::new(io::Cursor::new(input.as_bytes().to_vec()));
        self
    }

    /// Makes `variable` set to `value`.
    #[must_use]
    pub fn with_env(mut self, variable: &str, value: &str) -> Self {
        let previous = std::mem::replace(&mut self.environment, Box::new(|_| None));
        let (variable, value) = (variable.to_owned(), value.to_owned());
        self.environment = Box::new(move |key| {
            if key == variable {
                Some(value.clone())
            } else {
                previous(key)
            }
        });
        self
    }

    /// Attaches a PostgreSQL pool, for example from
    /// `forge::db::testing::TestDatabase::database`.
    #[cfg(feature = "postgres")]
    #[must_use]
    pub fn with_postgres(mut self, database: forge_db::PgDatabase) -> Self {
        self.database = Some(DatabaseHandle::Postgres(database));
        self
    }

    /// Attaches a MySQL/MariaDB pool.
    #[cfg(feature = "mysql")]
    #[must_use]
    pub fn with_mysql(mut self, database: forge_db::MySqlDatabase) -> Self {
        self.database = Some(DatabaseHandle::MySql(database));
        self
    }

    /// Attaches a SQLite pool.
    #[cfg(feature = "sqlite")]
    #[must_use]
    pub fn with_sqlite(mut self, database: forge_db::SqliteDatabase) -> Self {
        self.database = Some(DatabaseHandle::Sqlite(database));
        self
    }
}

/// Parsed `--name value` options and `--flag`s of a command.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CommandOptions {
    values: BTreeMap<String, String>,
    flags: Vec<String>,
}

impl CommandOptions {
    fn parse(arguments: &[String], values: &[&str], flags: &[&str]) -> Result<Self, CommandError> {
        let mut parsed = Self::default();
        let mut arguments = arguments.iter();
        while let Some(argument) = arguments.next() {
            let (name, inline) = match argument.split_once('=') {
                Some((name, value)) => (name, Some(value)),
                None => (argument.as_str(), None),
            };
            if !name.starts_with("--") {
                return Err(CommandError::usage(format!(
                    "unexpected argument {argument:?}; options are --name value"
                )));
            }
            if flags.contains(&name) {
                if inline.is_some() {
                    return Err(CommandError::usage(format!("{name} takes no value")));
                }
                if parsed.flags.iter().any(|flag| flag == name) {
                    return Err(CommandError::usage(format!("{name} is given twice")));
                }
                parsed.flags.push(name.to_owned());
            } else if values.contains(&name) {
                let value = match inline {
                    Some(value) => value.to_owned(),
                    None => arguments
                        .next()
                        .filter(|value| !value.starts_with("--"))
                        .cloned()
                        .ok_or_else(|| CommandError::usage(format!("{name} needs a value")))?,
                };
                if parsed.values.insert(name.to_owned(), value).is_some() {
                    return Err(CommandError::usage(format!("{name} is given twice")));
                }
            } else if name.starts_with("--password") {
                return Err(CommandError::usage(
                    "passwords are never accepted as arguments (process lists and shell history \
                     expose them); type it at the prompt or pipe it with --password-stdin",
                ));
            } else {
                return Err(CommandError::usage(format!("unknown option {name}")));
            }
        }
        Ok(parsed)
    }

    /// The value of option `name` (for example `"--tenant"`), if given.
    #[must_use]
    pub fn value(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }

    /// The value of a required option.
    pub fn required(&self, name: &str) -> Result<&str, CommandError> {
        self.value(name)
            .ok_or_else(|| CommandError::usage(format!("{name} is required")))
    }

    /// Whether flag `name` (for example `"--password-stdin"`) was given.
    #[must_use]
    pub fn flag(&self, name: &str) -> bool {
        self.flags.iter().any(|flag| flag == name)
    }
}

/// A command failed. Usage errors exit with code 2, everything else with 1.
#[derive(Debug, Error)]
pub enum CommandError {
    /// The command line or input was wrong; the message says how to fix it.
    #[error("{0}")]
    Usage(String),
    /// The command could not complete.
    #[error("{0}")]
    Failed(String),
    /// The command could not complete because of `source`.
    #[error("{message}")]
    Source {
        /// What the command was doing.
        message: String,
        /// The underlying failure.
        #[source]
        source: Box<dyn StdError + Send + Sync>,
    },
}

impl CommandError {
    /// A usage error (exit code 2).
    #[must_use]
    pub fn usage(message: impl Into<String>) -> Self {
        Self::Usage(message.into())
    }

    /// A failure (exit code 1).
    #[must_use]
    pub fn failed(message: impl Into<String>) -> Self {
        Self::Failed(message.into())
    }

    /// A failure caused by `source` (exit code 1). Messages of sources are
    /// printed, so they must not contain secrets.
    #[must_use]
    pub fn caused_by(
        message: impl Into<String>,
        source: impl StdError + Send + Sync + 'static,
    ) -> Self {
        Self::Source {
            message: message.into(),
            source: Box::new(source),
        }
    }

    pub(crate) fn is_usage(&self) -> bool {
        matches!(self, Self::Usage(_))
    }
}

#[cfg(test)]
mod command_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn context(arguments: &[&str]) -> CommandContext {
        CommandContext {
            app: "demo",
            command: "setup",
            arguments: arguments
                .iter()
                .map(|argument| (*argument).to_owned())
                .collect(),
            config: AppConfig::default(),
            database: None,
            input: Box::new(io::empty()),
            output: Box::new(io::sink()),
            terminal: false,
            hidden_prompt: Box::new(|_| Err(io::Error::from(io::ErrorKind::Unsupported))),
            environment: Box::new(|_| None),
        }
    }

    #[test]
    fn command_names_are_restricted() {
        for valid in ["setup", "seed", "db-reset2"] {
            assert!(valid_name(valid), "{valid}");
        }
        for invalid in ["", "Setup", "2seed", "-x", "a b", &"a".repeat(33)] {
            assert!(!valid_name(invalid), "{invalid}");
        }
    }

    #[test]
    fn options_parse_values_and_flags_strictly() {
        let parsed = context(&[
            "--tenant",
            "acme",
            "--email=ada@example.com",
            "--password-stdin",
        ])
        .options(
            &["--tenant", "--email", "--organization"],
            &["--password-stdin"],
        )
        .expect("valid options");
        assert_eq!(parsed.value("--tenant"), Some("acme"));
        assert_eq!(parsed.required("--email").ok(), Some("ada@example.com"));
        assert_eq!(parsed.value("--organization"), None);
        assert!(parsed.required("--organization").is_err());
        assert!(parsed.flag("--password-stdin"));

        for (arguments, message) in [
            (&["acme"][..], "unexpected argument"),
            (&["--tenant"][..], "needs a value"),
            (&["--tenant", "--email"][..], "needs a value"),
            (&["--tenant", "a", "--tenant", "b"][..], "given twice"),
            (&["--password-stdin", "--password-stdin"][..], "given twice"),
            (&["--password-stdin=yes"][..], "takes no value"),
            (&["--nope"][..], "unknown option"),
            (
                &["--password", "hunter2hunter2"][..],
                "never accepted as arguments",
            ),
            (
                &["--password=hunter2hunter2"][..],
                "never accepted as arguments",
            ),
        ] {
            let error = context(arguments)
                .options(&["--tenant", "--email"], &["--password-stdin"])
                .expect_err("invalid options");
            assert!(error.is_usage());
            assert!(
                error.to_string().contains(message),
                "{arguments:?}: {error}"
            );
            assert!(!error.to_string().contains("hunter2"), "{error}");
        }
    }

    #[test]
    fn secrets_come_from_stdin_the_environment_or_a_hidden_prompt() {
        let mut piped = context(&[]);
        piped.input = Box::new(io::Cursor::new(b"correct horse\r\nignored\n".to_vec()));
        assert_eq!(
            piped
                .read_secret("FORGE_SETUP_PASSWORD", true, "Password")
                .ok()
                .as_deref(),
            Some("correct horse")
        );

        let mut empty = context(&[]);
        assert!(
            empty
                .read_secret("FORGE_SETUP_PASSWORD", true, "Password")
                .is_err()
        );

        let mut environment = context(&[]);
        environment.environment = Box::new(|key| {
            (key == "FORGE_SETUP_PASSWORD").then(|| "from the environment".to_owned())
        });
        assert_eq!(
            environment
                .read_secret("FORGE_SETUP_PASSWORD", false, "Password")
                .ok()
                .as_deref(),
            Some("from the environment")
        );
        let both = environment
            .read_secret("FORGE_SETUP_PASSWORD", true, "Password")
            .expect_err("ambiguous");
        assert!(both.to_string().contains("not both"));

        let unattended = context(&[])
            .read_secret("FORGE_SETUP_PASSWORD", false, "Password")
            .expect_err("no source");
        assert!(unattended.to_string().contains("--password-stdin"));

        let mut terminal = context(&[]);
        terminal.terminal = true;
        let mut answers = vec!["typed twice".to_owned(), "typed twice".to_owned()].into_iter();
        terminal.hidden_prompt = Box::new(move |_| Ok(answers.next().unwrap_or_default()));
        assert_eq!(
            terminal
                .read_secret("FORGE_SETUP_PASSWORD", false, "Password")
                .ok()
                .as_deref(),
            Some("typed twice")
        );

        let mut mismatch = context(&[]);
        mismatch.terminal = true;
        let mut answers = vec!["first".to_owned(), "second".to_owned()].into_iter();
        mismatch.hidden_prompt = Box::new(move |_| Ok(answers.next().unwrap_or_default()));
        let error = mismatch
            .read_secret("FORGE_SETUP_PASSWORD", false, "Password")
            .expect_err("mismatch");
        assert!(error.to_string().contains("do not match"));
        assert!(!error.to_string().contains("first"));
    }

    #[test]
    fn errors_classify_usage_and_keep_sources() {
        assert!(CommandError::usage("x").is_usage());
        assert!(!CommandError::failed("x").is_usage());
        let error = CommandError::caused_by("seeding failed", io::Error::other("disk full"));
        assert_eq!(error.to_string(), "seeding failed");
        assert_eq!(
            StdError::source(&error).map(ToString::to_string).as_deref(),
            Some("disk full")
        );
    }
}
