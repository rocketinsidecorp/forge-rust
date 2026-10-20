//! Application builder and process lifecycle.

use std::{
    error::Error,
    ffi::OsString,
    fmt,
    future::Future,
    io::{self, Write},
    net::SocketAddr,
    pin::Pin,
    process::ExitCode,
    time::Duration,
};

use forge_config::{AppConfig, ConfigError, Environment, SecretString};
use forge_core::Cancellation;
use forge_http::{RouteError, Router, Server, ServerError};
use thiserror::Error;
use tokio::net::TcpListener;
use tracing::{error, info};

#[cfg(feature = "jobs")]
use crate::{
    background::{Background, BackgroundError, JobsDeclaration, JobsParts, WorkerMode},
    cli::JobsCommand,
};
use crate::{
    cli::{self, Command},
    command::{CommandContext, CommandError, RegisteredCommand},
    compose::{ComposeError, ComposeHook, Composition},
    database::{DatabaseHandle, Declaration, MigrationCommand, UrlPurpose, register_health},
    logging, probe, signal,
    worker::{RunningWorkers, Worker},
};

/// Job registrations collected by composition hooks (`()` without `jobs`).
#[cfg(feature = "jobs")]
pub(crate) type JobsSlot = Option<JobsParts>;
#[cfg(not(feature = "jobs"))]
pub(crate) type JobsSlot = ();
/// The runtime `serve` runs next to the server (`()` without `jobs`).
#[cfg(feature = "jobs")]
type BackgroundSlot = Option<Background>;
#[cfg(not(feature = "jobs"))]
type BackgroundSlot = ();

/// `sysexits.h` `EX_USAGE`-style code for an invalid command line.
const EXIT_USAGE: u8 = 2;
/// `sysexits.h` `EX_CONFIG`: the configuration is invalid (including a
/// database URL that does not match the declared backend).
const EXIT_CONFIG: u8 = 78;
/// Upper bound for runtime teardown after the server has already drained.
const RUNTIME_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);
const UNKNOWN_VERSION: &str = "unknown";

type RouteRegistration = Box<dyn FnOnce(&mut Router) -> Result<(), RouteError> + Send + 'static>;
type DatabaseCommandFuture =
    Pin<Box<dyn Future<Output = Result<(), Box<dyn Error + Send + Sync>>> + Send + 'static>>;
type DatabaseCommandHandler =
    Box<dyn Fn(SecretString) -> DatabaseCommandFuture + Send + Sync + 'static>;

/// A Forge application process.
///
/// `App` is the entry point of generated applications. It parses the command
/// line, loads [`AppConfig`] from the environment, installs logging, builds
/// the Tokio runtime and serves HTTP until `SIGTERM`/`SIGINT`. Applications do
/// not use `#[tokio::main]`; the runtime is a framework detail.
pub struct App {
    name: &'static str,
    version: &'static str,
    registrations: Vec<RouteRegistration>,
    migrate: Option<DatabaseCommandHandler>,
    rollback: Option<DatabaseCommandHandler>,
    declarations: Vec<Declaration>,
    compose_hooks: Vec<ComposeHook>,
    commands: Vec<RegisteredCommand>,
    #[cfg(feature = "jobs")]
    jobs: Option<JobsDeclaration>,
}

impl fmt::Debug for App {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("App")
            .field("name", &self.name)
            .field("version", &self.version)
            .field("route_registrations", &self.registrations.len())
            .field("database_migrations", &self.migrate.is_some())
            .field(
                "declared_databases",
                &self
                    .declarations
                    .iter()
                    .map(Declaration::method)
                    .collect::<Vec<_>>(),
            )
            .field(
                "commands",
                &self
                    .commands
                    .iter()
                    .map(|command| command.name)
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl App {
    /// Creates an application.
    ///
    /// `name` is used in logs and `--version` output.
    #[must_use]
    pub fn new(name: &'static str) -> Self {
        Self {
            name,
            version: UNKNOWN_VERSION,
            registrations: Vec::new(),
            migrate: None,
            rollback: None,
            declarations: Vec::new(),
            compose_hooks: Vec::new(),
            commands: Vec::new(),
            #[cfg(feature = "jobs")]
            jobs: None,
        }
    }

    /// Sets the version reported in logs and `--version` output, typically
    /// `env!("CARGO_PKG_VERSION")`. Defaults to `"unknown"`.
    #[must_use]
    pub fn version(mut self, version: &'static str) -> Self {
        self.version = version;
        self
    }

    /// Registers application routes.
    ///
    /// Standard health routes (`/health`, `/health/live`, `/health/ready`) are
    /// always added by the framework before `register` runs. May be called
    /// several times; registrations run in call order. A [`RouteError`] (for
    /// example a duplicate route) aborts startup with exit code 1.
    #[must_use]
    pub fn routes(
        mut self,
        register: impl FnOnce(&mut Router) -> Result<(), RouteError> + Send + 'static,
    ) -> Self {
        self.registrations.push(Box::new(register));
        self
    }

    /// Registers database migration handlers.
    ///
    /// Generated PostgreSQL applications use this hook to keep SQLx confined to
    /// their infrastructure/bootstrap boundary while the Forge process owns
    /// command dispatch and runtime lifecycle.
    #[must_use]
    pub fn migrations<M, MFut, ME, R, RFut, RE>(mut self, migrate: M, rollback: R) -> Self
    where
        M: Fn(SecretString) -> MFut + Send + Sync + 'static,
        MFut: Future<Output = Result<(), ME>> + Send + 'static,
        ME: Error + Send + Sync + 'static,
        R: Fn(SecretString) -> RFut + Send + Sync + 'static,
        RFut: Future<Output = Result<(), RE>> + Send + 'static,
        RE: Error + Send + Sync + 'static,
    {
        self.migrate = Some(Box::new(move |url| {
            let future = migrate(url);
            Box::pin(async move {
                future
                    .await
                    .map_err(|error| Box::new(error) as Box<dyn Error + Send + Sync>)
            })
        }));
        self.rollback = Some(Box::new(move |url| {
            let future = rollback(url);
            Box::pin(async move {
                future
                    .await
                    .map_err(|error| Box::new(error) as Box<dyn Error + Send + Sync>)
            })
        }));
        self
    }

    /// Declares PostgreSQL as the application's database, with its embedded
    /// migrations (`sqlx::migrate!()`).
    ///
    /// The declaration makes `FORGE_DATABASE_URL` (`postgres://` or
    /// `postgresql://`) mandatory for `serve`, creates a lazily connecting
    /// pool that [`Composition::postgres`] hands to [`App::compose`] hooks,
    /// makes `/health/ready` ping the database, and runs `migrate` and
    /// `rollback` with `FORGE_MIGRATION_DATABASE_URL`. A URL for another
    /// engine fails startup with exit code 78. Declaring two backends exits
    /// with code 1. Takes precedence over [`App::migrations`].
    #[cfg(feature = "postgres")]
    #[must_use]
    pub fn postgres(mut self, migrator: forge_db::Migrator) -> Self {
        self.declarations.push(Declaration::Postgres(migrator));
        self
    }

    /// Declares MySQL 8.4+ or MariaDB 11.4+ as the application's database.
    ///
    /// Behaves like [`App::postgres`] with `mysql://` or `mariadb://` URLs and
    /// [`Composition::mysql`].
    #[cfg(feature = "mysql")]
    #[must_use]
    pub fn mysql(mut self, migrator: forge_db::Migrator) -> Self {
        self.declarations.push(Declaration::MySql(migrator));
        self
    }

    /// Declares SQLite as the application's database.
    ///
    /// Behaves like [`App::postgres`] with `sqlite:` URLs and
    /// [`Composition::sqlite`]; SQLite has no separate migration role, so
    /// `FORGE_MIGRATION_DATABASE_URL` normally points at the same file.
    #[cfg(feature = "sqlite")]
    #[must_use]
    pub fn sqlite(mut self, migrator: forge_db::Migrator) -> Self {
        self.declarations.push(Declaration::Sqlite(migrator));
        self
    }

    /// Declares durable jobs (feature `jobs`, ADR 0025).
    ///
    /// Requires [`App::postgres`]; other backends are refused at startup
    /// (exit code 1) instead of being emulated. The declaration makes
    /// `migrate` apply the embedded job schema before the application's
    /// migrations, hands composition hooks the registry through
    /// [`Composition::jobs`], runs the workers, cron schedules and pruning
    /// in `serve` (with [`WorkerMode::InServe`]) and in `jobs work`, and
    /// enables the `jobs status|dead|retry|discard` operator commands, which
    /// use the runtime `FORGE_DATABASE_URL`.
    #[cfg(feature = "jobs")]
    #[must_use]
    pub fn jobs(mut self, mode: WorkerMode) -> Self {
        let mut declaration = self.jobs.unwrap_or(JobsDeclaration::new(mode));
        declaration.mode = mode;
        self.jobs = Some(declaration);
        self
    }

    /// Declares the transactional outbox (feature `events`, ADR 0025).
    ///
    /// Implies [`App::jobs`] with [`WorkerMode::InServe`] unless it was
    /// declared. `migrate` applies the outbox/inbox schema; composition
    /// hooks set the publisher through [`Composition::events`]; the
    /// dispatcher runs wherever the job workers run.
    #[cfg(feature = "events")]
    #[must_use]
    pub fn events(mut self) -> Self {
        let mut declaration = self
            .jobs
            .unwrap_or(JobsDeclaration::new(WorkerMode::InServe));
        declaration.events = true;
        self.jobs = Some(declaration);
        self
    }

    /// Registers a composition hook that wires adapters and routes with the
    /// declared database.
    ///
    /// Hooks run in registration order inside the Tokio runtime, after the
    /// standard health routes and the [`App::routes`] registrations and before
    /// the server accepts connections. A [`ComposeError`] aborts startup with
    /// exit code 1.
    #[must_use]
    pub fn compose(
        mut self,
        hook: impl FnOnce(&mut Composition<'_>) -> Result<(), ComposeError> + Send + 'static,
    ) -> Self {
        self.compose_hooks.push(Box::new(hook));
        self
    }

    /// Registers an application command, `<binary> <name> [ARGS]`, such as
    /// `setup` or `seed` (see [`CommandContext`]).
    ///
    /// The command runs once inside the Tokio runtime after configuration and
    /// logging are loaded. When the application declared a database, the
    /// command's pool is connected with `FORGE_MIGRATION_DATABASE_URL`, the
    /// administrative credential. `summary` is shown by `help`. Names are
    /// lowercase letters, digits and `-`; a built-in name (`serve`,
    /// `migrate`, ...) or a repeated name makes every command exit with
    /// code 1. A [`CommandError::Usage`] exits with code 2, any other error
    /// with code 1.
    #[must_use]
    pub fn command<F, Fut>(mut self, name: &'static str, summary: &'static str, run: F) -> Self
    where
        F: FnOnce(CommandContext) -> Fut + Send + 'static,
        Fut: Future<Output = Result<(), CommandError>> + Send + 'static,
    {
        self.commands.push(RegisteredCommand {
            name,
            summary,
            handler: Box::new(move |context| Box::pin(run(context))),
        });
        self
    }

    /// Parses process arguments, runs the selected command and returns the
    /// process exit code.
    ///
    /// Commands: serve (default), healthcheck, migrate, rollback, version
    /// (--version, -V) and help (--help, -h). Exit codes: 0 success, 1 runtime
    /// failure, unhealthy probe or an invalid application declaration (more
    /// than one database backend), `2` invalid command line, `78` invalid
    /// configuration (including a database URL that does not match the
    /// declared backend).
    #[must_use]
    pub fn run(self) -> ExitCode {
        self.run_with(std::env::args_os().skip(1), AppConfig::from_env)
    }

    /// Runs with an explicit command line and configuration source so tests
    /// need no process environment.
    fn run_with(
        self,
        args: impl IntoIterator<Item = OsString>,
        load_config: impl FnOnce() -> Result<AppConfig, ConfigError>,
    ) -> ExitCode {
        let names: Vec<&str> = self.commands.iter().map(|command| command.name).collect();
        match cli::parse(args, &names) {
            Ok(Command::Serve) => self.checked(|app| app.serve(load_config)),
            Ok(Command::App(index, arguments)) => {
                self.checked(|app| app.run_application_command(index, arguments, load_config))
            }
            Ok(Command::Healthcheck) => healthcheck(),
            Ok(Command::Migrate) => {
                self.checked(|app| app.run_database_command(DatabaseCommand::Migrate, load_config))
            }
            Ok(Command::Rollback) => {
                self.checked(|app| app.run_database_command(DatabaseCommand::Rollback, load_config))
            }
            Ok(Command::Version) => write_stdout(&format!("{} {}\n", self.name, self.version)),
            Ok(Command::Help) => write_stdout(&cli::usage(self.name, &self.summaries())),
            #[cfg(feature = "jobs")]
            Ok(Command::Jobs(command)) => {
                self.checked(|app| app.run_jobs_command(command, load_config))
            }
            Err(error) => {
                write_stderr(&format!(
                    "error: {error}\n\n{}",
                    cli::usage(self.name, &self.summaries())
                ));
                ExitCode::from(EXIT_USAGE)
            }
        }
    }

    fn summaries(&self) -> Vec<(&'static str, &'static str)> {
        self.commands
            .iter()
            .map(|command| (command.name, command.summary))
            .collect()
    }

    /// Rejects an application that declared more than one database backend
    /// or registered an invalid, built-in or repeated command name.
    fn checked(self, run: impl FnOnce(Self) -> ExitCode) -> ExitCode {
        for (index, command) in self.commands.iter().enumerate() {
            let repeated = self.commands[..index]
                .iter()
                .any(|earlier| earlier.name == command.name);
            if !crate::command::valid_name(command.name)
                || cli::BUILT_IN.contains(&command.name)
                || repeated
            {
                write_stderr(&format!(
                    "error: the application registered the command {:?}; command names are \
                     unique lowercase letters, digits and '-', and cannot replace a built-in \
                     command\n",
                    command.name
                ));
                return ExitCode::FAILURE;
            }
        }
        if self.declarations.len() > 1 {
            let methods: Vec<_> = self.declarations.iter().map(Declaration::method).collect();
            write_stderr(&format!(
                "error: the application declared more than one database backend ({}); declare exactly one\n",
                methods.join(", ")
            ));
            return ExitCode::FAILURE;
        }
        #[cfg(feature = "jobs")]
        if self.jobs.is_some() {
            let scheme = self.declarations.first().map(Declaration::scheme);
            if let Err(reason) = crate::background::check_backend(scheme) {
                write_stderr(&format!("error: {reason}\n"));
                return ExitCode::FAILURE;
            }
        }
        run(self)
    }

    fn serve(self, load_config: impl FnOnce() -> Result<AppConfig, ConfigError>) -> ExitCode {
        let Self {
            name,
            version,
            registrations,
            migrate: _,
            rollback: _,
            mut declarations,
            compose_hooks,
            commands: _,
            #[cfg(feature = "jobs")]
            jobs,
        } = self;
        let declaration = declarations.pop();
        #[cfg(feature = "jobs")]
        let (jobs_slot, run_jobs_in_serve) = jobs_for_serve(jobs);
        #[cfg(not(feature = "jobs"))]
        let jobs_slot: JobsSlot = ();

        let config = match load_config() {
            Ok(config) => config,
            Err(error) => return config_error(&error),
        };
        if let Err(error) = logging::init(&config.log) {
            return config_error(&error);
        }
        let url = match declaration
            .as_ref()
            .map(|declaration| declaration.validate(&config.database, UrlPurpose::Runtime))
            .transpose()
        {
            Ok(url) => url,
            Err(error) => return config_error(&error),
        };

        let runtime = match tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => {
                error!(app = name, error = %ErrorChain(&error), "failed to start async runtime");
                return ExitCode::FAILURE;
            }
        };

        // SQLx pools and the adapters composed around them need a Tokio
        // context even before the first request arrives.
        let composed = {
            let _guard = runtime.enter();
            compose(
                name,
                &config,
                registrations,
                compose_hooks,
                declaration.zip(url),
                jobs_slot,
            )
        };
        let (router, database, workers, jobs_slot) = match composed {
            Ok(composed) => composed,
            Err(code) => return code,
        };
        let background = background_for(jobs_slot, database.as_ref());
        #[cfg(feature = "jobs")]
        let background = background.filter(|_| run_jobs_in_serve);

        let result = runtime.block_on(serve_until_signal(
            name, version, config, router, database, workers, background,
        ));
        runtime.shutdown_timeout(RUNTIME_SHUTDOWN_TIMEOUT);

        match result {
            Ok(()) => {
                info!(app = name, "shutdown complete");
                ExitCode::SUCCESS
            }
            Err(error) => {
                error!(app = name, error = %ErrorChain(&error), "server failed");
                ExitCode::FAILURE
            }
        }
    }

    fn run_database_command(
        mut self,
        command: DatabaseCommand,
        load_config: impl FnOnce() -> Result<AppConfig, ConfigError>,
    ) -> ExitCode {
        let config = match load_config() {
            Ok(config) => config,
            Err(error) => return config_error(&error),
        };
        if let Err(error) = logging::init(&config.log) {
            return config_error(&error);
        }

        let declaration = self.declarations.pop();
        let url = match &declaration {
            Some(declaration) => {
                match declaration.validate(&config.database, UrlPurpose::Migration) {
                    Ok(url) => url.clone(),
                    Err(error) => return config_error(&error),
                }
            }
            None => {
                let Some(url) = &config.database.migration_url else {
                    write_stderr(
                        "error: FORGE_MIGRATION_DATABASE_URL is required for database commands\n",
                    );
                    return ExitCode::from(EXIT_CONFIG);
                };
                url.clone()
            }
        };

        let runtime = match tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => {
                error!(app = self.name, error = %ErrorChain(&error), "failed to start async runtime");
                return ExitCode::FAILURE;
            }
        };

        if let Some(declaration) = declaration {
            let migration = match command {
                DatabaseCommand::Migrate => MigrationCommand::Migrate,
                DatabaseCommand::Rollback => MigrationCommand::Rollback,
            };
            #[cfg(feature = "jobs")]
            let framework = match (command, self.jobs) {
                (DatabaseCommand::Migrate, Some(jobs)) => jobs.migrators(),
                _ => Vec::new(),
            };
            #[cfg(not(feature = "jobs"))]
            let framework = Vec::new();
            let code = runtime.block_on(
                declaration.run_migration_command(self.name, &url, migration, &framework),
            );
            runtime.shutdown_timeout(RUNTIME_SHUTDOWN_TIMEOUT);
            return code;
        }

        // Applications written before `App::postgres`: their own handlers.
        let handler = match command {
            DatabaseCommand::Migrate => self.migrate,
            DatabaseCommand::Rollback => self.rollback,
        };
        let Some(handler) = handler else {
            write_stderr("error: database migrations are not configured for this application\n");
            return ExitCode::FAILURE;
        };

        let result = runtime.block_on(handler(url));
        runtime.shutdown_timeout(RUNTIME_SHUTDOWN_TIMEOUT);
        match result {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                error!(
                    app = self.name,
                    command = command.as_str(),
                    error = %ErrorChain(error.as_ref()),
                    "database command failed"
                );
                ExitCode::FAILURE
            }
        }
    }
}

impl App {
    /// Runs the application command at `index` with `arguments`.
    fn run_application_command(
        mut self,
        index: usize,
        arguments: Vec<String>,
        load_config: impl FnOnce() -> Result<AppConfig, ConfigError>,
    ) -> ExitCode {
        let config = match load_config() {
            Ok(config) => config,
            Err(error) => return config_error(&error),
        };
        if let Err(error) = logging::init(&config.log) {
            return config_error(&error);
        }
        if index >= self.commands.len() {
            return ExitCode::FAILURE;
        }
        let command = self.commands.swap_remove(index);
        let declaration = self.declarations.pop();
        let url = match &declaration {
            Some(declaration) => {
                match declaration.validate(&config.database, UrlPurpose::Migration) {
                    Ok(url) => Some(url.clone()),
                    Err(error) => return config_error(&error),
                }
            }
            None => None,
        };
        let runtime = match tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => {
                error!(app = self.name, error = %ErrorChain(&error), "failed to start async runtime");
                return ExitCode::FAILURE;
            }
        };
        let name = self.name;
        let result = runtime.block_on(async move {
            let database = match declaration.zip(url) {
                Some((declaration, url)) => match declaration.connect(&url).await {
                    Ok(database) => Some(database),
                    Err(error) => {
                        return Err(CommandError::caused_by(
                            "cannot connect with FORGE_MIGRATION_DATABASE_URL",
                            error,
                        ));
                    }
                },
                None => None,
            };
            let context = CommandContext::for_process(
                name,
                command.name,
                arguments,
                config,
                database.clone(),
            );
            let result = (command.handler)(context).await;
            if let Some(database) = database {
                database.close().await;
            }
            result
        });
        runtime.shutdown_timeout(RUNTIME_SHUTDOWN_TIMEOUT);
        match result {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                write_stderr(&format!("error: {}\n", ErrorChain(&error)));
                if error.is_usage() {
                    ExitCode::from(EXIT_USAGE)
                } else {
                    ExitCode::FAILURE
                }
            }
        }
    }
}

#[cfg(feature = "jobs")]
impl App {
    /// `jobs work` runs the job runtime without HTTP until a signal; the
    /// other subcommands are operator queries over the runtime URL.
    fn run_jobs_command(
        mut self,
        command: JobsCommand,
        load_config: impl FnOnce() -> Result<AppConfig, ConfigError>,
    ) -> ExitCode {
        let config = match load_config() {
            Ok(config) => config,
            Err(error) => return config_error(&error),
        };
        if let Err(error) = logging::init(&config.log) {
            return config_error(&error);
        }
        let (Some(jobs), Some(declaration)) = (self.jobs, self.declarations.pop()) else {
            write_stderr("error: the jobs commands require App::postgres and App::jobs\n");
            return ExitCode::FAILURE;
        };
        let url = match declaration.validate(&config.database, UrlPurpose::Runtime) {
            Ok(url) => url.clone(),
            Err(error) => return config_error(&error),
        };
        let runtime = match tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => {
                error!(app = self.name, error = %ErrorChain(&error), "failed to start async runtime");
                return ExitCode::FAILURE;
            }
        };
        if command != JobsCommand::Work {
            let output = runtime.block_on(crate::operator::execute(self.name, &url, command));
            runtime.shutdown_timeout(RUNTIME_SHUTDOWN_TIMEOUT);
            return match output {
                Ok(text) => write_stdout(&text),
                Err(code) => code,
            };
        }

        let composed = {
            let _guard = runtime.enter();
            compose(
                self.name,
                &config,
                self.registrations,
                self.compose_hooks,
                Some((declaration, &url)),
                Some(JobsParts::new(jobs)),
            )
        };
        // Composition workers belong to the HTTP server's lifecycle (they
        // start with `serve`); a dedicated job worker process does not run them.
        let (_router, database, _workers, jobs_slot) = match composed {
            Ok(composed) => composed,
            Err(code) => return code,
        };
        let Some(background) = background_for(jobs_slot, database.as_ref()) else {
            write_stderr("error: the jobs commands require App::postgres and App::jobs\n");
            return ExitCode::FAILURE;
        };
        let name = self.name;
        let result = runtime.block_on(async move {
            let result = work_until_signal(name, background).await;
            if let Some(database) = database {
                database.close().await;
            }
            result
        });
        runtime.shutdown_timeout(RUNTIME_SHUTDOWN_TIMEOUT);
        match result {
            Ok(()) => {
                info!(app = name, "job workers stopped");
                ExitCode::SUCCESS
            }
            Err(error) => {
                error!(app = name, error = %ErrorChain(&error), "job workers failed");
                ExitCode::FAILURE
            }
        }
    }
}

/// The registry handed to `serve`'s composition hooks and whether `serve`
/// runs it. Hooks always see the registry, so the same composition function
/// works for `serve` and `jobs work` in both modes.
#[cfg(feature = "jobs")]
fn jobs_for_serve(jobs: Option<JobsDeclaration>) -> (JobsSlot, bool) {
    let run = jobs.is_some_and(|jobs| jobs.mode == WorkerMode::InServe);
    (jobs.map(JobsParts::new), run)
}

/// The job runtime for the composed registrations and the PostgreSQL pool.
#[cfg(feature = "jobs")]
fn background_for(jobs: JobsSlot, database: Option<&DatabaseHandle>) -> BackgroundSlot {
    let parts = jobs?;
    #[allow(irrefutable_let_patterns)]
    let Some(DatabaseHandle::Postgres(database)) = database else {
        return None;
    };
    Some(parts.into_background(database.clone()))
}

#[cfg(not(feature = "jobs"))]
fn background_for(jobs: JobsSlot, database: Option<&DatabaseHandle>) -> BackgroundSlot {
    let _ = (jobs, database);
}

/// Runs the job runtime alone until a shutdown signal.
#[cfg(feature = "jobs")]
async fn work_until_signal(name: &'static str, background: Background) -> Result<(), ServeError> {
    let signal = signal::shutdown_signal().map_err(ServeError::Signal)?;
    let shutdown = Cancellation::new();
    info!(app = name, "job workers started");
    let run = async {
        let result = background.run(shutdown.clone()).await;
        shutdown.cancel();
        result
    };
    let ((), result) = tokio::join!(watch_signal(name, signal, shutdown.clone()), run);
    result.map_err(ServeError::Background)
}

/// Cancels `shutdown` on the first signal (or returns once it is cancelled
/// for another reason).
async fn watch_signal(
    name: &'static str,
    signal: impl Future<Output = &'static str>,
    shutdown: Cancellation,
) {
    tokio::select! {
        signal = signal => info!(app = name, signal, "shutdown started; draining connections and jobs"),
        () = shutdown.cancelled() => {}
    }
    shutdown.cancel();
}

/// Builds the route table and the declared database pool, then runs the
/// composition hooks. Must run inside the Tokio runtime context.
fn compose(
    name: &'static str,
    config: &AppConfig,
    registrations: Vec<RouteRegistration>,
    hooks: Vec<ComposeHook>,
    declared: Option<(Declaration, &forge_config::SecretString)>,
    jobs: JobsSlot,
) -> Result<(Router, Option<DatabaseHandle>, Vec<Worker>, JobsSlot), ExitCode> {
    #[cfg_attr(not(feature = "jobs"), allow(unused_mut))]
    let mut jobs = jobs;
    let database = match declared {
        Some((declaration, url)) => match declaration.connect_lazy(url) {
            Ok(database) => Some(database),
            Err(error) => {
                error!(app = name, error = %ErrorChain(&error), "failed to create the database pool");
                return Err(ExitCode::FAILURE);
            }
        },
        None => None,
    };
    let mut router = match build_router(registrations, database.clone()) {
        Ok(router) => router,
        Err(error) => {
            error!(app = name, error = %ErrorChain(&error), "failed to register routes");
            return Err(ExitCode::FAILURE);
        }
    };
    let mut workers = Vec::new();
    if !hooks.is_empty() {
        let mut composition = Composition::new(&mut router, config, database.clone());
        #[cfg(feature = "jobs")]
        if let Some(parts) = jobs.as_mut() {
            composition.attach_jobs(parts);
        }
        for hook in hooks {
            if let Err(error) = hook(&mut composition) {
                error!(app = name, error = %ErrorChain(&error), "composition hook failed");
                return Err(ExitCode::FAILURE);
            }
        }
        workers = composition.into_workers();
    }
    Ok((router, database, workers, jobs))
}

#[derive(Clone, Copy)]
enum DatabaseCommand {
    Migrate,
    Rollback,
}

impl DatabaseCommand {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Migrate => "migrate",
            Self::Rollback => "rollback",
        }
    }
}

/// Builds the route table: framework health routes first, then the
/// application's registrations in order.
fn build_router(
    registrations: Vec<RouteRegistration>,
    database: Option<DatabaseHandle>,
) -> Result<Router, RouteError> {
    let mut router = Router::new();
    register_health(&mut router, database)?;
    for register in registrations {
        register(&mut router)?;
    }
    Ok(router)
}

/// Failure while running the `serve` command.
#[derive(Debug, Error)]
enum ServeError {
    #[error("failed to install shutdown signal handlers")]
    Signal(#[source] io::Error),
    #[error("failed to bind HTTP listener on {address}")]
    Bind {
        address: SocketAddr,
        #[source]
        source: io::Error,
    },
    #[error("HTTP server failed")]
    Server(#[from] ServerError),
    #[cfg(feature = "jobs")]
    #[error("background jobs failed")]
    Background(#[source] BackgroundError),
}

async fn serve_until_signal(
    name: &'static str,
    version: &'static str,
    config: AppConfig,
    router: Router,
    database: Option<DatabaseHandle>,
    workers: Vec<Worker>,
    background: BackgroundSlot,
) -> Result<(), ServeError> {
    let workers = RunningWorkers::start(workers);
    let result = serve(name, version, config, router, background).await;
    // Workers stop after the server drained, before the pool closes.
    workers.stop().await;
    if let Some(database) = database {
        database.close().await;
    }
    result
}

async fn serve(
    name: &'static str,
    version: &'static str,
    config: AppConfig,
    router: Router,
    background: BackgroundSlot,
) -> Result<(), ServeError> {
    let signal = signal::shutdown_signal().map_err(ServeError::Signal)?;
    let bind = config.server.bind;
    let listener = TcpListener::bind(bind)
        .await
        .map_err(|source| ServeError::Bind {
            address: bind,
            source,
        })?;
    let address = listener.local_addr().unwrap_or(bind);
    info!(
        app = name,
        version,
        environment = environment_name(config.environment),
        %address,
        "listening"
    );

    // One shutdown for the server and the job runtime: a signal, or either
    // of them failing, stops both.
    let shutdown = Cancellation::new();
    let server = async {
        let stop = shutdown.clone();
        let result = Server::new(config.server, router)
            .serve(listener, async move { stop.cancelled().await })
            .await;
        shutdown.cancel();
        result.map_err(ServeError::from)
    };
    #[cfg(feature = "jobs")]
    let jobs = async {
        let Some(background) = background else {
            return Ok(());
        };
        let result = background.run(shutdown.clone()).await;
        shutdown.cancel();
        result.map_err(ServeError::Background)
    };
    #[cfg(not(feature = "jobs"))]
    let jobs = async {
        let () = background;
        Ok::<(), ServeError>(())
    };
    let ((), served, ran) =
        tokio::join!(watch_signal(name, signal, shutdown.clone()), server, jobs);
    served.and(ran)
}

fn environment_name(environment: Environment) -> &'static str {
    match environment {
        Environment::Development => "development",
        Environment::Test => "test",
        Environment::Production => "production",
    }
}

/// Container health probe: exit 0 only when `GET /health/live` returns 200.
fn healthcheck() -> ExitCode {
    let config = match AppConfig::from_env() {
        Ok(config) => config,
        Err(error) => {
            write_stderr(&format!("healthcheck: {error}\n"));
            return ExitCode::FAILURE;
        }
    };
    let target = probe::probe_target(config.server.bind);
    match probe::probe(target, probe::PROBE_TIMEOUT) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            write_stderr(&format!("healthcheck: {error}\n"));
            ExitCode::FAILURE
        }
    }
}

fn config_error(error: &(dyn Error + 'static)) -> ExitCode {
    write_stderr(&format!("error: {}\n", ErrorChain(error)));
    ExitCode::from(EXIT_CONFIG)
}

/// Writes to stdout; a closed pipe yields a failure code instead of a panic.
fn write_stdout(text: &str) -> ExitCode {
    let mut stdout = io::stdout().lock();
    match stdout
        .write_all(text.as_bytes())
        .and_then(|()| stdout.flush())
    {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::FAILURE,
    }
}

/// Best-effort stderr write; there is nowhere left to report a failure.
fn write_stderr(text: &str) {
    let mut stderr = io::stderr().lock();
    if stderr.write_all(text.as_bytes()).is_err() {
        return;
    }
    // Ignoring is deliberate: stderr is the last-resort channel.
    drop(stderr.flush());
}

/// Displays an error followed by each of its sources, separated by `: `.
pub(crate) struct ErrorChain<'a>(pub(crate) &'a (dyn Error + 'static));

impl fmt::Display for ErrorChain<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)?;
        let mut source = self.0.source();
        while let Some(error) = source {
            write!(formatter, ": {error}")?;
            source = error.source();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use forge_http::{Method, Response, StatusCode};

    use super::*;

    fn no_config() -> Result<AppConfig, ConfigError> {
        Ok(AppConfig::default())
    }

    fn config_with(vars: &[(&str, &str)]) -> Result<AppConfig, ConfigError> {
        AppConfig::from_vars(
            vars.iter()
                .map(|(key, value)| (OsString::from(key), OsString::from(value))),
        )
    }

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn version_defaults_to_unknown() {
        let app = App::new("demo");

        assert_eq!(app.version, "unknown");
        assert_eq!(app.version("1.2.3").version, "1.2.3");
    }

    #[test]
    fn informational_commands_succeed() {
        for command in ["--version", "-V", "version", "--help", "-h", "help"] {
            let code = App::new("demo").run_with(args(&[command]), no_config);
            assert_eq!(code, ExitCode::SUCCESS, "{command}");
        }
    }

    #[test]
    fn migration_handlers_can_be_registered() {
        let app = App::new("demo").migrations(
            |_url| async { Ok::<(), std::io::Error>(()) },
            |_url| async { Ok::<(), std::io::Error>(()) },
        );

        assert!(app.migrate.is_some());
        assert!(app.rollback.is_some());
    }

    #[test]
    fn application_commands_receive_their_arguments_and_map_errors_to_exit_codes() {
        use std::sync::{Arc, Mutex};

        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&seen);
        let app = App::new("demo")
            .command(
                "setup",
                "Create the first owner",
                move |context| async move {
                    if let Ok(mut seen) = recorded.lock() {
                        seen.extend(context.arguments().iter().cloned());
                    }
                    let options = context.options(&["--tenant"], &[])?;
                    options.required("--tenant")?;
                    Ok(())
                },
            )
            .command("seed", "Load seed data", |_| async {
                Err(CommandError::failed("seeding failed"))
            });

        assert_eq!(
            app.run_with(args(&["setup", "--tenant", "acme"]), no_config),
            ExitCode::SUCCESS
        );
        assert_eq!(
            seen.lock().map(|seen| seen.clone()).unwrap_or_default(),
            vec!["--tenant".to_owned(), "acme".to_owned()]
        );

        let usage = App::new("demo").command("setup", "x", |context| async move {
            context.options(&["--tenant"], &[])?.required("--tenant")?;
            Ok(())
        });
        assert_eq!(
            usage.run_with(args(&["setup"]), no_config),
            ExitCode::from(EXIT_USAGE)
        );
        let failing = App::new("demo").command("seed", "x", |_| async {
            Err(CommandError::failed("seeding failed"))
        });
        assert_eq!(
            failing.run_with(args(&["seed"]), no_config),
            ExitCode::FAILURE
        );
        // Commands load the configuration like every other command.
        let configured = App::new("demo").command("seed", "x", |_| async { Ok(()) });
        assert_eq!(
            configured.run_with(args(&["seed"]), || {
                Err(ConfigError::NotUnicode { key: "FORGE_BIND" })
            }),
            ExitCode::from(EXIT_CONFIG)
        );
    }

    #[test]
    fn invalid_or_built_in_command_names_are_refused() {
        for name in ["migrate", "Setup", ""] {
            let app = App::new("demo").command(name, "x", |_| async { Ok(()) });
            assert_eq!(
                app.run_with(args(&["version"]), no_config),
                ExitCode::SUCCESS,
                "informational commands do not check"
            );
            let app = App::new("demo").command(name, "x", |_| async { Ok(()) });
            assert_eq!(app.run_with(args(&["serve"]), no_config), ExitCode::FAILURE);
        }
        let repeated = App::new("demo")
            .command("seed", "x", |_| async { Ok(()) })
            .command("seed", "y", |_| async { Ok(()) });
        assert_eq!(
            repeated.run_with(args(&["seed"]), no_config),
            ExitCode::FAILURE
        );
    }

    #[cfg(feature = "postgres")]
    #[test]
    fn database_commands_need_the_migration_credential() {
        let app = App::new("demo")
            .postgres(forge_db::Migrator::DEFAULT)
            .command("seed", "x", |_| async { Ok(()) });
        let vars = [("FORGE_DATABASE_URL", "postgres://u:p@h/app")];
        assert_eq!(
            app.run_with(args(&["seed"]), || config_with(&vars)),
            ExitCode::from(EXIT_CONFIG)
        );
    }

    #[test]
    fn invalid_command_line_exits_with_usage_code() {
        let code = App::new("demo").run_with(args(&["frobnicate"]), no_config);

        assert_eq!(code, ExitCode::from(EXIT_USAGE));
    }

    #[test]
    fn router_contains_health_and_application_routes() {
        let app = App::new("demo")
            .routes(|router| {
                router.route(Method::GET, "/a", |_| async {
                    Response::empty(StatusCode::OK)
                })?;
                Ok(())
            })
            .routes(|router| {
                router.route(Method::GET, "/b", |_| async {
                    Response::empty(StatusCode::OK)
                })?;
                Ok(())
            });

        let router = build_router(app.registrations, None).expect("routes must register");
        let debug = format!("{router:?}");

        for route in [
            "GET /health",
            "GET /health/live",
            "GET /health/ready",
            "GET /a",
            "GET /b",
        ] {
            assert!(debug.contains(route), "{route} missing from {debug}");
        }
    }

    fn register_index(router: &mut Router) -> Result<(), RouteError> {
        router.route(Method::GET, "/", |_| async {
            Response::empty(StatusCode::OK)
        })?;
        Ok(())
    }

    #[test]
    fn function_items_can_register_routes() {
        let app = App::new("demo").routes(register_index);

        let router = build_router(app.registrations, None).expect("routes must register");

        assert!(format!("{router:?}").contains("\"GET /\""));
    }

    #[test]
    fn application_cannot_replace_health_routes() {
        let app = App::new("demo").routes(|router| {
            router.route(Method::GET, "/health/live", |_| async {
                Response::empty(StatusCode::OK)
            })?;
            Ok(())
        });

        let error = build_router(app.registrations, None).expect_err("duplicate must be rejected");

        assert!(matches!(error, RouteError::Duplicate { .. }));
    }

    #[test]
    fn error_chain_includes_sources() {
        let error = ServeError::Bind {
            address: "127.0.0.1:1".parse().expect("valid address"),
            source: io::Error::new(io::ErrorKind::AddrInUse, "address in use"),
        };

        assert_eq!(
            ErrorChain(&error).to_string(),
            "failed to bind HTTP listener on 127.0.0.1:1: address in use"
        );
    }

    #[cfg(all(feature = "postgres", feature = "mysql"))]
    #[test]
    fn declaring_two_backends_exits_with_failure_before_touching_configuration() {
        let migrator = || forge_db::Migrator::DEFAULT;
        for command in ["serve", "migrate", "rollback"] {
            let app = App::new("demo").postgres(migrator()).mysql(migrator());
            let code = app.run_with(args(&[command]), || {
                Err(ConfigError::NotUnicode { key: "FORGE_BIND" })
            });
            assert_eq!(code, ExitCode::FAILURE, "{command}");
        }
        // Informational commands do not care.
        let app = App::new("demo").postgres(migrator()).mysql(migrator());
        assert_eq!(
            app.run_with(args(&["--version"]), no_config),
            ExitCode::SUCCESS
        );
    }

    #[cfg(feature = "postgres")]
    #[test]
    fn a_url_for_another_engine_is_a_configuration_error_that_never_echoes_it() {
        let config = config_with(&[
            (
                "FORGE_DATABASE_URL",
                "mysql://user:hunter2@db.example.test/app",
            ),
            ("FORGE_MIGRATION_DATABASE_URL", "sqlite:secret.db"),
        ])
        .expect("both URLs are supported schemes");
        let declaration = Declaration::Postgres(forge_db::Migrator::DEFAULT);

        for purpose in [UrlPurpose::Runtime, UrlPurpose::Migration] {
            let error = declaration
                .validate(&config.database, purpose)
                .expect_err("scheme must not match");
            let message = error.to_string();
            assert!(
                message.contains("postgres:// or postgresql://"),
                "{message}"
            );
            assert!(message.contains("App::postgres"), "{message}");
            assert!(!message.contains("hunter2") && !message.contains("secret.db"));
        }

        let missing = declaration
            .validate(&AppConfig::default().database, UrlPurpose::Runtime)
            .expect_err("URL is required");
        assert_eq!(
            missing.to_string(),
            "FORGE_DATABASE_URL is required by App::postgres"
        );
    }

    #[cfg(feature = "postgres")]
    #[test]
    fn a_mismatched_url_exits_with_the_configuration_code() {
        let vars = [("FORGE_DATABASE_URL", "mysql://u:p@h/app")];
        let app = App::new("demo").postgres(forge_db::Migrator::DEFAULT);
        assert_eq!(
            app.run_with(args(&["serve"]), || config_with(&vars)),
            ExitCode::from(EXIT_CONFIG)
        );
        let vars = [("FORGE_MIGRATION_DATABASE_URL", "mariadb://u:p@h/app")];
        let app = App::new("demo").postgres(forge_db::Migrator::DEFAULT);
        assert_eq!(
            app.run_with(args(&["migrate"]), || config_with(&vars)),
            ExitCode::from(EXIT_CONFIG)
        );
        let app = App::new("demo").postgres(forge_db::Migrator::DEFAULT);
        assert_eq!(
            app.run_with(args(&["rollback"]), no_config),
            ExitCode::from(EXIT_CONFIG)
        );
    }

    #[test]
    fn legacy_migration_handlers_still_run_without_a_declaration() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };

        let ran = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&ran);
        let app = App::new("demo").migrations(
            move |_url| {
                let flag = Arc::clone(&flag);
                async move {
                    flag.store(true, Ordering::SeqCst);
                    Ok::<(), io::Error>(())
                }
            },
            |_url| async { Ok::<(), io::Error>(()) },
        );
        let vars = [("FORGE_MIGRATION_DATABASE_URL", "postgres://u:p@h/app")];

        let code = app.run_with(args(&["migrate"]), || config_with(&vars));

        assert_eq!(code, ExitCode::SUCCESS);
        assert!(ran.load(Ordering::SeqCst));
    }

    #[cfg(feature = "sqlite")]
    mod sqlite {
        use forge_http::{HeaderMap, Request, Uri};

        use super::*;

        fn migrations() -> forge_db::Migrator {
            sqlx::migrate!("tests/migrations/sqlite")
        }

        fn url(directory: &tempfile::TempDir) -> String {
            format!(
                "sqlite://{}?mode=rwc",
                directory.path().join("app.db").display()
            )
        }

        async fn table_exists(url: &str) -> bool {
            let database = forge_db::SqliteDatabase::connect(&SecretString::new(url))
                .await
                .expect("connect");
            let exists = sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM sqlite_master WHERE name = 'notes'",
            )
            .fetch_one(database.pool())
            .await
            .expect("query")
                == 1;
            database.close().await;
            exists
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn migrate_and_rollback_commands_use_the_declared_backend() {
            let directory = tempfile::tempdir().expect("tempdir");
            let url = url(&directory);
            let run = |command: &'static str| {
                let url = url.clone();
                tokio::task::spawn_blocking(move || {
                    App::new("demo")
                        .sqlite(migrations())
                        .run_with(args(&[command]), || {
                            config_with(&[("FORGE_MIGRATION_DATABASE_URL", url.as_str())])
                        })
                })
            };

            assert_eq!(run("migrate").await.expect("join"), ExitCode::SUCCESS);
            assert!(table_exists(&url).await);
            assert_eq!(run("migrate").await.expect("join"), ExitCode::SUCCESS);
            assert_eq!(run("rollback").await.expect("join"), ExitCode::SUCCESS);
            assert!(!table_exists(&url).await);
            assert_eq!(run("rollback").await.expect("join"), ExitCode::SUCCESS);
        }

        fn request(path: &str) -> Request {
            Request::new_for_test(
                Method::GET,
                path.parse::<Uri>().expect("uri"),
                HeaderMap::new(),
                bytes::Bytes::new(),
                "127.0.0.1:1".parse().expect("address"),
                uuid::Uuid::now_v7(),
            )
        }

        #[tokio::test]
        async fn readiness_pings_the_declared_database() {
            let database = forge_db::SqliteDatabase::connect(&SecretString::new("sqlite::memory:"))
                .await
                .expect("memory database");
            let router = build_router(Vec::new(), Some(DatabaseHandle::Sqlite(database.clone())))
                .expect("routes");

            for path in ["/health", "/health/live", "/health/ready"] {
                let response = router.dispatch(request(path)).await;
                assert_eq!(response.status(), StatusCode::OK, "{path}");
                assert_eq!(response.headers()["content-type"], "application/json");
            }
            let ready = router.dispatch(request("/health/ready")).await;
            assert_eq!(ready.body(), r#"{"status":"ready"}"#);

            database.close().await;
            let down = router.dispatch(request("/health/ready")).await;
            assert_eq!(down.status(), StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(
                down.body(),
                r#"{"status":"unavailable","checks":{"database":"unavailable"}}"#
            );
            // Liveness never depends on the database.
            let live = router.dispatch(request("/health/live")).await;
            assert_eq!(live.status(), StatusCode::OK);
        }

        #[tokio::test]
        async fn composition_receives_only_the_declared_database() {
            let directory = tempfile::tempdir().expect("tempdir");
            let config =
                config_with(&[("FORGE_DATABASE_URL", url(&directory).as_str())]).expect("config");
            let url = config.database.url.as_ref().expect("url");

            let hook: ComposeHook = Box::new(|composition| {
                composition.sqlite()?;
                #[cfg(feature = "postgres")]
                assert!(matches!(
                    composition.postgres(),
                    Err(ComposeError::DatabaseNotDeclared {
                        requested: "postgres"
                    })
                ));
                composition
                    .router()
                    .route(Method::GET, "/composed", |_| async {
                        Response::empty(StatusCode::NO_CONTENT)
                    })?;
                Ok(())
            });
            let (router, database, _workers, _) = compose(
                "demo",
                &config,
                Vec::new(),
                vec![hook],
                Some((Declaration::Sqlite(forge_db::Migrator::DEFAULT), url)),
                JobsSlot::default(),
            )
            .expect("composition succeeds");
            assert!(format!("{router:?}").contains("GET /composed"));
            database.expect("pool was created").close().await;

            // A hook asking for an undeclared backend aborts startup.
            #[cfg(feature = "postgres")]
            {
                let hook: ComposeHook = Box::new(|composition| {
                    composition.postgres()?;
                    Ok(())
                });
                let outcome = compose(
                    "demo",
                    &config,
                    Vec::new(),
                    vec![hook],
                    Some((Declaration::Sqlite(forge_db::Migrator::DEFAULT), url)),
                    JobsSlot::default(),
                );
                assert_eq!(outcome.err(), Some(ExitCode::FAILURE));
            }
        }
    }

    #[cfg(feature = "jobs")]
    mod jobs {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };

        use async_trait::async_trait;
        use forge_jobs::{
            EnqueueOptions, Job, JobContext, JobError, JobHandler, postgres::PgJobStore,
        };
        use serde::{Deserialize, Serialize};

        use super::*;
        use crate::background::{JobsDeclaration, WorkerMode};

        #[test]
        fn jobs_require_postgres_and_are_refused_elsewhere() {
            // Without a database.
            let code = App::new("demo")
                .jobs(WorkerMode::InServe)
                .run_with(args(&["serve"]), no_config);
            assert_eq!(code, ExitCode::FAILURE);
            assert_eq!(
                crate::background::check_backend(None).expect_err("refused"),
                "App::jobs requires App::postgres (durable jobs are stored in PostgreSQL)"
            );
            for scheme in [
                forge_config::DatabaseScheme::MySql,
                forge_config::DatabaseScheme::Sqlite,
            ] {
                let reason = crate::background::check_backend(Some(scheme)).expect_err("refused");
                assert!(reason.contains("refused rather than emulated"), "{reason}");
            }
            assert!(
                crate::background::check_backend(Some(forge_config::DatabaseScheme::Postgres))
                    .is_ok()
            );
            #[cfg(feature = "sqlite")]
            for command in ["serve", "migrate", "jobs"] {
                let code = App::new("demo")
                    .sqlite(forge_db::Migrator::DEFAULT)
                    .jobs(WorkerMode::Dedicated)
                    .run_with(args(&[command]), no_config);
                assert_eq!(code, ExitCode::FAILURE, "{command}");
            }
        }

        #[test]
        fn declarations_select_mode_and_framework_schemas() {
            let app = App::new("demo").jobs(WorkerMode::Dedicated);
            let declaration = app.jobs.expect("declared");
            assert_eq!(declaration.mode, WorkerMode::Dedicated);
            assert_eq!(declaration.migrators().len(), 1);
            #[cfg(feature = "events")]
            {
                let app = App::new("demo").jobs(WorkerMode::Dedicated).events();
                let declaration = app.jobs.expect("declared");
                assert_eq!(
                    declaration.mode,
                    WorkerMode::Dedicated,
                    "events keep the mode"
                );
                assert_eq!(declaration.migrators().len(), 2);
                let implied = App::new("demo").events().jobs.expect("implied");
                assert_eq!(implied.mode, WorkerMode::InServe);
            }
            let config = AppConfig::default();
            let mut router = Router::new();
            let mut composition = Composition::new(&mut router, &config, None);
            assert!(matches!(
                composition.jobs(),
                Err(ComposeError::JobsNotDeclared)
            ));
            let mut parts = JobsParts::new(JobsDeclaration::new(WorkerMode::InServe));
            composition.attach_jobs(&mut parts);
            assert!(composition.jobs().is_ok());
            #[cfg(feature = "events")]
            assert!(matches!(
                composition.events(),
                Err(ComposeError::EventsNotDeclared)
            ));

            // Hooks get the registry in both modes; only InServe runs it in `serve`.
            let (slot, run) = jobs_for_serve(Some(JobsDeclaration::new(WorkerMode::Dedicated)));
            assert!(slot.is_some() && !run);
            let (slot, run) = jobs_for_serve(Some(JobsDeclaration::new(WorkerMode::InServe)));
            assert!(slot.is_some() && run);
            let (slot, run) = jobs_for_serve(None);
            assert!(slot.is_none() && !run);
        }

        #[derive(Serialize, Deserialize)]
        struct Greet;

        impl Job for Greet {
            const NAME: &'static str = "demo.greet";
        }

        struct Greeter(Arc<AtomicUsize>);

        #[async_trait]
        impl JobHandler for Greeter {
            type Job = Greet;

            async fn perform(&self, _: &JobContext, _: Greet) -> Result<(), JobError> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        }

        #[cfg(feature = "events")]
        #[derive(Serialize, Deserialize)]
        struct Greeted;

        #[cfg(feature = "events")]
        impl forge_events::IntegrationEvent for Greeted {
            const TYPE: &'static str = "demo.greeted";
        }

        #[cfg(feature = "events")]
        struct Listener(Arc<AtomicUsize>);

        #[cfg(feature = "events")]
        #[async_trait]
        impl forge_events::EventSubscriber for Listener {
            async fn handle(
                &self,
                _: &forge_events::EventEnvelope,
            ) -> Result<(), forge_events::PublishError> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        }

        async fn run_command(command: &'static [&'static str], url: String) -> ExitCode {
            tokio::task::spawn_blocking(move || {
                let app = App::new("demo")
                    .postgres(forge_db::Migrator::DEFAULT)
                    .jobs(WorkerMode::Dedicated);
                #[cfg(feature = "events")]
                let app = app.events();
                app.run_with(args(command), || {
                    config_with(&[
                        ("FORGE_DATABASE_URL", url.as_str()),
                        ("FORGE_MIGRATION_DATABASE_URL", url.as_str()),
                    ])
                })
            })
            .await
            .expect("join")
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        #[ignore = "needs FORGE_TEST_DATABASE_URL"]
        async fn migrate_operator_commands_and_the_composed_runtime_work_end_to_end() {
            let test = forge_db::testing::TestDatabase::<forge_db::sqlx::Postgres>::create_empty()
                .await
                .expect("needs FORGE_TEST_DATABASE_URL");
            let url = test.url().expose().to_owned();

            assert_eq!(
                run_command(&["migrate"], url.clone()).await,
                ExitCode::SUCCESS
            );
            assert_eq!(
                run_command(&["migrate"], url.clone()).await,
                ExitCode::SUCCESS,
                "idempotent"
            );
            let tables: Vec<String> = sqlx::query_scalar(
                "SELECT tablename::text FROM pg_tables WHERE tablename LIKE 'forge\\_%' ORDER BY 1",
            )
            .fetch_all(test.database().pool())
            .await
            .expect("tables");
            assert!(tables.contains(&"forge_jobs".to_owned()), "{tables:?}");
            #[cfg(feature = "events")]
            assert!(
                tables.contains(&"forge_outbox_events".to_owned()),
                "{tables:?}"
            );
            for command in [&["jobs", "status"][..], &["jobs", "dead", "5"]] {
                assert_eq!(
                    run_command(command, url.clone()).await,
                    ExitCode::SUCCESS,
                    "{command:?}"
                );
            }
            assert_eq!(
                run_command(
                    &["jobs", "retry", "01a1567c-bf31-73d9-966c-0123456789ab"],
                    url.clone()
                )
                .await,
                ExitCode::FAILURE,
                "no such dead job"
            );

            // The runtime `serve`/`jobs work` build from composition hooks.
            let greeted = Arc::new(AtomicUsize::new(0));
            let heard = Arc::new(AtomicUsize::new(0));
            let config = config_with(&[("FORGE_DATABASE_URL", url.as_str())]).expect("config");
            let runtime_url = config.database.url.clone().expect("url");
            let hook_greeted = Arc::clone(&greeted);
            let hook_heard = Arc::clone(&heard);
            let hook: ComposeHook = Box::new(move |composition| {
                composition
                    .jobs()?
                    .register(Greeter(hook_greeted))
                    .map_err(ComposeError::other)?;
                #[cfg(feature = "events")]
                {
                    let mut subscribers = forge_events::Subscribers::new();
                    subscribers
                        .subscribe::<Greeted>(Listener(hook_heard))
                        .map_err(ComposeError::other)?;
                    composition.events()?.publish_to(subscribers);
                }
                #[cfg(not(feature = "events"))]
                drop(hook_heard);
                Ok(())
            });
            #[cfg_attr(not(feature = "events"), allow(unused_mut))]
            let mut declaration = JobsDeclaration::new(WorkerMode::InServe);
            #[cfg(feature = "events")]
            {
                declaration.events = true;
            }
            let (_router, database, _workers, slot) = compose(
                "demo",
                &config,
                Vec::new(),
                vec![hook],
                Some((
                    Declaration::Postgres(forge_db::Migrator::DEFAULT),
                    &runtime_url,
                )),
                Some(JobsParts::new(declaration)),
            )
            .expect("composition");
            let background = background_for(slot, database.as_ref()).expect("background");

            let store = PgJobStore::new(test.database().clone());
            store
                .enqueue(&Greet, EnqueueOptions::new())
                .await
                .expect("enqueue");
            #[cfg(feature = "events")]
            {
                let mut transaction = test.database().begin().await.expect("begin");
                forge_events::postgres::append(&mut transaction, &Greeted)
                    .await
                    .expect("append");
                transaction.commit().await.expect("commit");
            }
            let shutdown = Cancellation::new();
            let running = tokio::spawn(background.run(shutdown.clone()));
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
            let expected_events = usize::from(cfg!(feature = "events"));
            while greeted.load(Ordering::SeqCst) == 0
                || heard.load(Ordering::SeqCst) < expected_events
            {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "the runtime did not run the job"
                );
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            shutdown.cancel();
            running.await.expect("join").expect("clean shutdown");
            if let Some(database) = database {
                database.close().await;
            }
            test.cleanup().await.expect("cleanup");
        }
    }
}
