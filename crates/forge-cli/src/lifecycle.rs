//! The application lifecycle commands: `migrate`, `rollback`, `seed`,
//! `setup` and `forge db ...`, like Rails' `db:*` tasks, `routes` and `openapi`.
//!
//! Every command delegates to the generated application's own subcommands
//! (`<app> migrate`, `<app> db create`, ...), so development and production
//! share one implementation and this crate holds no database code and reads
//! no database credential: the application inherits the environment.
//!
//! Destructive commands (`db drop`, `db reset`, `db rollback`) are refused
//! when `FORGE_ENV` is not `development` or `test`, unless `--force-production`
//! is given and either a person types the application name at a terminal or
//! `--yes` is given with `FORGE_ALLOW_DESTRUCTIVE=true` in the process
//! environment (the same rule as Forge-Go's `forge db`). The application
//! refuses a drop of a server that is not on this machine by itself, since
//! only it sees the URL.

use std::io::{BufRead, Write};
use std::str::FromStr as _;

use forge_config::Environment;

use crate::CliError;
use crate::application::Application;

/// The environment variable that lets `--yes` skip the prompt.
pub(crate) const ALLOW_DESTRUCTIVE: &str = "FORGE_ALLOW_DESTRUCTIVE";

/// The flags of a destructive command.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct Destructive {
    /// Allow this outside development and test, after confirmation.
    pub(crate) force_production: bool,
    /// Confirm without a prompt; needs `FORGE_ALLOW_DESTRUCTIVE=true`.
    pub(crate) yes: bool,
}

/// One `forge db` subcommand.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Db {
    Create,
    Drop(Destructive),
    Reset(Destructive),
    Setup,
    Prepare,
    Migrate,
    Status,
    Seed,
    Rollback {
        steps: Option<u32>,
        destructive: Destructive,
    },
}

/// Everything a lifecycle command touches, injectable so the destructive
/// paths can be tested without a database or a terminal.
pub(crate) struct Lifecycle<'a, A: Application> {
    pub(crate) app: A,
    /// The package name, shown in messages and typed to confirm.
    pub(crate) name: &'a str,
    /// The process environment: the one place `FORGE_ENV` and
    /// `FORGE_ALLOW_DESTRUCTIVE` are read.
    pub(crate) environment: &'a dyn Fn(&str) -> Option<String>,
    pub(crate) input: &'a mut dyn BufRead,
    pub(crate) messages: &'a mut dyn Write,
    pub(crate) interactive: bool,
}

/// `db exists` exits with this when the database is not there.
const DATABASE_MISSING: i32 = 3;

impl<A: Application> Lifecycle<'_, A> {
    /// `forge migrate [status]`.
    pub(crate) fn migrate(&mut self, status: bool) -> Result<(), CliError> {
        if status {
            self.call(&["migrate", "status"])
        } else {
            self.call(&["migrate"])
        }
    }

    /// `forge rollback [--steps N]`.
    pub(crate) fn rollback(&mut self, steps: Option<u32>) -> Result<(), CliError> {
        self.call_with("rollback", &[], &steps_flag(steps))
    }

    /// `forge seed`.
    /// `forge openapi [--output FILE | --check FILE]`.
    pub(crate) fn openapi(&mut self, arguments: &[String]) -> Result<(), CliError> {
        self.call_with("openapi", &[], arguments)
    }

    /// `forge routes`.
    pub(crate) fn routes(&mut self) -> Result<(), CliError> {
        self.call(&["routes"])
    }

    pub(crate) fn seed(&mut self) -> Result<(), CliError> {
        self.call(&["seed"])
    }

    /// `forge setup`: the first tenant and its owner. Standard input stays
    /// the terminal's, so the password is typed without echo or piped with
    /// `--password-stdin`; it is never an argument.
    pub(crate) fn setup(&mut self, arguments: &[String]) -> Result<(), CliError> {
        let mut all = vec!["setup".to_owned()];
        all.extend_from_slice(arguments);
        self.run(&all)
    }

    /// `forge db <subcommand>`.
    pub(crate) fn db(&mut self, command: &Db) -> Result<(), CliError> {
        match command {
            Db::Create => self.call(&["db", "create"]),
            Db::Drop(flags) => {
                self.confirm("drop", *flags)?;
                self.drop_database(*flags)
            }
            Db::Reset(flags) => {
                self.confirm("drop", *flags)?;
                self.drop_database(*flags)?;
                self.setup_database()
            }
            Db::Setup => self.setup_database(),
            Db::Prepare => self.prepare(),
            Db::Migrate => self.call(&["migrate"]),
            Db::Status => self.call(&["migrate", "status"]),
            Db::Seed => self.call(&["seed"]),
            Db::Rollback { steps, destructive } => {
                // Rolling back can destroy data: production needs the same
                // explicit confirmation as dropping.
                self.confirm("roll back", *destructive)?;
                self.call_with("rollback", &[], &steps_flag(*steps))
            }
        }
    }

    fn drop_database(&mut self, flags: Destructive) -> Result<(), CliError> {
        if flags.force_production {
            self.call(&["db", "drop", "--force-production"])
        } else {
            self.call(&["db", "drop"])
        }
    }

    /// Create, migrate, seed.
    fn setup_database(&mut self) -> Result<(), CliError> {
        self.call(&["db", "create"])?;
        self.call(&["migrate"])?;
        self.call(&["seed"])
    }

    /// Set up a missing database, migrate an existing one; safe to repeat.
    fn prepare(&mut self) -> Result<(), CliError> {
        let arguments = owned(&["db", "exists"]);
        match self.app.run(&arguments)? {
            Some(0) => self.call(&["migrate"]),
            Some(DATABASE_MISSING) => self.setup_database(),
            code => Err(failed(&arguments, code)),
        }
    }

    fn call(&mut self, arguments: &[&str]) -> Result<(), CliError> {
        self.run(&owned(arguments))
    }

    fn call_with(
        &mut self,
        command: &str,
        fixed: &[&str],
        extra: &[String],
    ) -> Result<(), CliError> {
        let mut arguments = vec![command.to_owned()];
        arguments.extend(fixed.iter().map(|argument| (*argument).to_owned()));
        arguments.extend_from_slice(extra);
        self.run(&arguments)
    }

    fn run(&mut self, arguments: &[String]) -> Result<(), CliError> {
        match self.app.run(arguments)? {
            Some(0) => Ok(()),
            code => Err(failed(arguments, code)),
        }
    }

    /// Why this environment is protected, or `None` for development and test.
    /// An unset `FORGE_ENV` is development (the application's own default);
    /// a value the framework would reject is protected rather than guessed.
    fn protection_reason(&self) -> Option<String> {
        let value = (self.environment)("FORGE_ENV")?;
        match Environment::from_str(&value) {
            Ok(Environment::Development | Environment::Test) => None,
            Ok(Environment::Production) => Some("FORGE_ENV is production".to_owned()),
            Err(_) => Some("FORGE_ENV is not a known environment".to_owned()),
        }
    }

    /// The guard in front of every destructive command; see the module
    /// documentation. `--force-production` always asks for confirmation, so a
    /// server the CLI cannot see the address of is protected too.
    fn confirm(&mut self, action: &str, flags: Destructive) -> Result<(), CliError> {
        let reason = self.protection_reason();
        if let (Some(reason), false) = (&reason, flags.force_production) {
            return Err(CliError::Refused(format!(
                "refusing to {action} the database of \"{}\": {reason}; \
                 pass --force-production, and confirm",
                self.name
            )));
        }
        if !flags.force_production {
            return Ok(());
        }
        if flags.yes {
            if (self.environment)(ALLOW_DESTRUCTIVE).as_deref() != Some("true") {
                return Err(CliError::Refused(format!(
                    "--yes here also needs {ALLOW_DESTRUCTIVE}=true in the process environment"
                )));
            }
            return Ok(());
        }
        if !self.interactive {
            return Err(CliError::Refused(format!(
                "confirming a destructive command here needs an interactive terminal, \
                 or --yes with {ALLOW_DESTRUCTIVE}=true"
            )));
        }
        let why = reason.unwrap_or_else(|| "a protected database was requested".to_owned());
        write!(
            self.messages,
            "This will {action} the protected database of \"{name}\" ({why}).\n\
             Type the application name ({name}) to continue: ",
            name = self.name
        )
        .and_then(|()| self.messages.flush())
        .map_err(CliError::Output)?;
        let mut line = String::new();
        let read = self.input.read_line(&mut line).map_err(CliError::Output)?;
        if read == 0 {
            return Err(CliError::Refused("no confirmation given".to_owned()));
        }
        if line.trim() != self.name {
            return Err(CliError::Refused(
                "confirmation did not match: nothing was changed".to_owned(),
            ));
        }
        Ok(())
    }
}

fn steps_flag(steps: Option<u32>) -> Vec<String> {
    steps.map_or_else(Vec::new, |steps| {
        vec!["--steps".to_owned(), steps.to_string()]
    })
}

fn owned(arguments: &[&str]) -> Vec<String> {
    arguments
        .iter()
        .map(|argument| (*argument).to_owned())
        .collect()
}

fn failed(arguments: &[String], code: Option<i32>) -> CliError {
    CliError::AppCommandFailed {
        command: arguments.join(" "),
        code,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::io::Cursor;

    use super::*;

    /// A fake application: records every call, answers from a script.
    #[derive(Default)]
    struct Fake {
        calls: Vec<Vec<String>>,
        /// Exit code by the first two arguments joined; default 0.
        codes: HashMap<String, Option<i32>>,
    }

    impl Application for &mut Fake {
        fn run(&mut self, arguments: &[String]) -> Result<Option<i32>, CliError> {
            self.calls.push(arguments.to_vec());
            let key = arguments
                .iter()
                .take(2)
                .cloned()
                .collect::<Vec<_>>()
                .join(" ");
            Ok(self.codes.get(&key).copied().unwrap_or(Some(0)))
        }
    }

    struct Harness {
        fake: Fake,
        environment: HashMap<&'static str, &'static str>,
        input: &'static str,
        interactive: bool,
        prompt: Vec<u8>,
    }

    impl Harness {
        fn new() -> Self {
            Self {
                fake: Fake::default(),
                environment: HashMap::new(),
                input: "",
                interactive: false,
                prompt: Vec::new(),
            }
        }

        fn env(mut self, key: &'static str, value: &'static str) -> Self {
            self.environment.insert(key, value);
            self
        }

        fn typed(mut self, input: &'static str) -> Self {
            self.input = input;
            self.interactive = true;
            self
        }

        fn run<T>(&mut self, command: impl FnOnce(&mut Lifecycle<'_, &mut Fake>) -> T) -> T {
            let environment = self.environment.clone();
            let lookup = move |key: &str| environment.get(key).map(|value| (*value).to_owned());
            let mut input = Cursor::new(self.input.as_bytes().to_vec());
            let mut lifecycle = Lifecycle {
                app: &mut self.fake,
                name: "shop",
                environment: &lookup,
                input: &mut input,
                messages: &mut self.prompt,
                interactive: self.interactive,
            };
            command(&mut lifecycle)
        }

        fn calls(&self) -> Vec<String> {
            self.fake.calls.iter().map(|call| call.join(" ")).collect()
        }
    }

    const FORCE: Destructive = Destructive {
        force_production: true,
        yes: false,
    };

    #[test]
    fn migrate_rollback_seed_and_setup_delegate_to_the_application() {
        let mut harness = Harness::new();
        harness.run(|l| l.migrate(false)).expect("migrate");
        harness.run(|l| l.migrate(true)).expect("status");
        harness.run(|l| l.rollback(None)).expect("rollback");
        harness.run(|l| l.rollback(Some(3))).expect("rollback 3");
        harness.run(|l| l.seed()).expect("seed");
        harness
            .run(|l| {
                l.setup(&[
                    "--tenant".to_owned(),
                    "acme".to_owned(),
                    "--password-stdin".to_owned(),
                ])
            })
            .expect("setup");
        assert_eq!(
            harness.calls(),
            [
                "migrate",
                "migrate status",
                "rollback",
                "rollback --steps 3",
                "seed",
                "setup --tenant acme --password-stdin",
            ]
        );
    }

    #[test]
    fn the_applications_failure_becomes_the_commands_failure() {
        let mut harness = Harness::new();
        harness.fake.codes.insert("migrate".to_owned(), Some(78));
        let error = harness.run(|l| l.migrate(false)).expect_err("fails");
        assert_eq!(
            error.to_string(),
            "application command \"migrate\" failed with status 78"
        );
        harness.fake.codes.insert("seed".to_owned(), None);
        let error = harness.run(|l| l.seed()).expect_err("fails");
        assert!(error.to_string().contains("signal"), "{error}");
    }

    #[test]
    fn db_setup_creates_migrates_and_seeds_in_that_order_and_stops_at_a_failure() {
        let mut harness = Harness::new();
        harness.run(|l| l.db(&Db::Setup)).expect("setup");
        assert_eq!(harness.calls(), ["db create", "migrate", "seed"]);

        let mut harness = Harness::new();
        harness.fake.codes.insert("migrate".to_owned(), Some(1));
        assert!(harness.run(|l| l.db(&Db::Setup)).is_err());
        assert_eq!(
            harness.calls(),
            ["db create", "migrate"],
            "seed is not reached"
        );
    }

    #[test]
    fn db_prepare_sets_up_a_missing_database_and_only_migrates_an_existing_one() {
        let mut harness = Harness::new();
        harness.run(|l| l.db(&Db::Prepare)).expect("existing");
        assert_eq!(harness.calls(), ["db exists", "migrate"]);

        let mut harness = Harness::new();
        harness.fake.codes.insert("db exists".to_owned(), Some(3));
        harness.run(|l| l.db(&Db::Prepare)).expect("missing");
        assert_eq!(
            harness.calls(),
            ["db exists", "db create", "migrate", "seed"]
        );

        let mut harness = Harness::new();
        harness.fake.codes.insert("db exists".to_owned(), Some(78));
        assert!(harness.run(|l| l.db(&Db::Prepare)).is_err());
        assert_eq!(harness.calls(), ["db exists"], "a failure is not 'missing'");
    }

    #[test]
    fn plain_db_subcommands_map_to_application_commands() {
        let mut harness = Harness::new();
        for command in [Db::Create, Db::Migrate, Db::Status, Db::Seed] {
            harness.run(|l| l.db(&command)).expect("delegates");
        }
        assert_eq!(
            harness.calls(),
            ["db create", "migrate", "migrate status", "seed"]
        );
    }

    #[test]
    fn destructive_commands_run_freely_in_development_and_test() {
        for environment in [None, Some("development"), Some("test"), Some("dev")] {
            let mut harness = Harness::new();
            if let Some(value) = environment {
                harness = harness.env("FORGE_ENV", value);
            }
            harness
                .run(|l| l.db(&Db::Drop(Destructive::default())))
                .expect("drop");
            harness
                .run(|l| l.db(&Db::Reset(Destructive::default())))
                .expect("reset");
            assert_eq!(
                harness.calls(),
                ["db drop", "db drop", "db create", "migrate", "seed"],
                "{environment:?}"
            );
        }
    }

    #[test]
    fn production_refuses_drop_reset_and_rollback_without_touching_the_application() {
        for value in ["production", "prod", "Production", "staging", ""] {
            for command in [
                Db::Drop(Destructive::default()),
                Db::Reset(Destructive::default()),
                Db::Rollback {
                    steps: None,
                    destructive: Destructive::default(),
                },
            ] {
                let mut harness = Harness::new().env("FORGE_ENV", value);
                let error = harness.run(|l| l.db(&command)).expect_err("refused");
                let message = error.to_string();
                assert!(
                    message.starts_with("refusing to ")
                        && message.contains("the database of \"shop\"")
                        && message.contains("pass --force-production, and confirm"),
                    "{value:?} {message}"
                );
                assert!(
                    harness.fake.calls.is_empty(),
                    "{value:?}: {:?}",
                    harness.calls()
                );
            }
        }
        let mut harness = Harness::new().env("FORGE_ENV", "production");
        let message = harness
            .run(|l| l.db(&Db::Drop(Destructive::default())))
            .expect_err("refused")
            .to_string();
        assert!(message.contains("FORGE_ENV is production"), "{message}");
    }

    #[test]
    fn force_production_needs_the_application_name_typed_at_a_terminal() {
        let mut harness = Harness::new()
            .env("FORGE_ENV", "production")
            .typed("shop\n");
        harness.run(|l| l.db(&Db::Drop(FORCE))).expect("confirmed");
        assert_eq!(harness.calls(), ["db drop --force-production"]);
        let prompt = String::from_utf8(harness.prompt.clone()).expect("utf-8");
        assert!(
            prompt.contains(
                "This will drop the protected database of \"shop\" (FORGE_ENV is production)"
            ) && prompt.contains("Type the application name (shop) to continue: "),
            "{prompt}"
        );

        for wrong in ["Shop\n", "shop-prod\n", "\n", ""] {
            let mut harness = Harness::new().env("FORGE_ENV", "production").typed(wrong);
            let error = harness.run(|l| l.db(&Db::Drop(FORCE))).expect_err("wrong");
            assert!(
                error.to_string().contains("confirmation did not match")
                    || error.to_string().contains("no confirmation given"),
                "{wrong:?}: {error}"
            );
            assert!(harness.fake.calls.is_empty());
        }
    }

    #[test]
    fn reset_confirms_once_then_drops_and_sets_up() {
        let mut harness = Harness::new()
            .env("FORGE_ENV", "production")
            .typed("shop\n");
        harness.run(|l| l.db(&Db::Reset(FORCE))).expect("reset");
        assert_eq!(
            harness.calls(),
            ["db drop --force-production", "db create", "migrate", "seed"]
        );
    }

    #[test]
    fn a_failed_drop_stops_reset_before_setup() {
        let mut harness = Harness::new();
        harness.fake.codes.insert("db drop".to_owned(), Some(1));
        assert!(
            harness
                .run(|l| l.db(&Db::Reset(Destructive::default())))
                .is_err()
        );
        assert_eq!(harness.calls(), ["db drop"]);
    }

    #[test]
    fn yes_needs_the_process_environment_to_allow_it() {
        let yes = Destructive {
            force_production: true,
            yes: true,
        };
        for allowed in [None, Some("1"), Some("TRUE"), Some("yes"), Some("")] {
            let mut harness = Harness::new().env("FORGE_ENV", "production");
            if let Some(value) = allowed {
                harness = harness.env(ALLOW_DESTRUCTIVE, value);
            }
            let error = harness.run(|l| l.db(&Db::Drop(yes))).expect_err("refused");
            assert!(
                error.to_string().contains("FORGE_ALLOW_DESTRUCTIVE=true"),
                "{allowed:?}: {error}"
            );
            assert!(harness.fake.calls.is_empty());
        }
        let mut harness = Harness::new()
            .env("FORGE_ENV", "production")
            .env(ALLOW_DESTRUCTIVE, "true");
        harness.run(|l| l.db(&Db::Drop(yes))).expect("allowed");
        assert_eq!(harness.calls(), ["db drop --force-production"]);
    }

    #[test]
    fn confirming_without_a_terminal_is_refused() {
        let mut harness = Harness::new().env("FORGE_ENV", "production");
        let error = harness.run(|l| l.db(&Db::Drop(FORCE))).expect_err("no tty");
        assert!(
            error.to_string().contains("needs an interactive terminal"),
            "{error}"
        );
        assert!(harness.fake.calls.is_empty());
    }

    #[test]
    fn force_production_asks_even_in_development_because_the_server_may_be_remote() {
        // Without the flag nothing is asked (the application refuses a remote
        // server itself); with it the person must confirm.
        let mut harness = Harness::new().typed("shop\n");
        harness.run(|l| l.db(&Db::Drop(FORCE))).expect("confirmed");
        assert_eq!(harness.calls(), ["db drop --force-production"]);
        assert!(
            String::from_utf8_lossy(&harness.prompt).contains("a protected database was requested")
        );

        let mut harness = Harness::new();
        assert!(harness.run(|l| l.db(&Db::Drop(FORCE))).is_err());
        assert!(harness.fake.calls.is_empty());
    }

    #[test]
    fn db_rollback_is_guarded_but_top_level_rollback_is_not() {
        let mut harness = Harness::new().env("FORGE_ENV", "production");
        harness
            .run(|l| l.rollback(Some(2)))
            .expect("unguarded, like Forge-Go");
        assert_eq!(harness.calls(), ["rollback --steps 2"]);

        let mut harness = Harness::new()
            .env("FORGE_ENV", "production")
            .typed("shop\n");
        harness
            .run(|l| {
                l.db(&Db::Rollback {
                    steps: Some(2),
                    destructive: FORCE,
                })
            })
            .expect("confirmed");
        assert_eq!(
            harness.calls(),
            ["rollback --steps 2"],
            "the flags are consumed here"
        );
    }
}
