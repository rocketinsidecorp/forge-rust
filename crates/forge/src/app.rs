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
use forge_http::{Method, OPENAPI_PATH, RouteError, Router, Server, ServerError};
use forge_openapi::Info;
use forge_telemetry::{MetricError, Telemetry, pool::PoolObservation};
use thiserror::Error;
use tokio::{net::TcpListener, time::Instant};
use tracing::{error, info, warn};

#[cfg(feature = "jobs")]
use crate::{
    background::{Background, BackgroundError, JobsDeclaration, JobsParts, WorkerMode},
    cli::JobsCommand,
};
use crate::{
    cli::{self, Command, DbCommand, OpenApiOutput},
    command::{CommandContext, CommandError, RegisteredCommand},
    compose::{ComposeError, ComposeHook, Composition},
    database::{DatabaseHandle, Declaration, MigrationCommand, UrlPurpose, register_health},
    logging, observability as process_telemetry, probe, signal,
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
pub(crate) const UNKNOWN_VERSION: &str = "unknown";

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
    api_info: Option<Info>,
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
            api_info: None,
            #[cfg(feature = "jobs")]
            jobs: None,
        }
    }

    /// Sets the title, version and description of the application's OpenAPI
    /// document (ADR 0039); they default to the application's name and
    /// [`App::version`].
    #[must_use]
    pub fn api_info(mut self, info: Info) -> Self {
        self.api_info = Some(info);
        self
    }

    fn openapi_info(&self) -> Info {
        self.api_info
            .clone()
            .unwrap_or_else(|| Info::new(self.name, self.version))
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

    /// Announces enqueued jobs and lets the job workers listen for them
    /// (`LISTEN/NOTIFY`, ADR 0046), with the meaning of Forge-Go's
    /// `queuepostgres.WithNotify()`; default off. Implies [`App::jobs`] with
    /// [`WorkerMode::InServe`] unless it was declared.
    ///
    /// When on, every connection of the application's pool sets
    /// `forge.jobs_notify = on`, so each due job enqueued through it
    /// (`forge::jobs::postgres::enqueue`, schedule ticks) sends
    /// `NOTIFY forge_jobs` when its transaction commits, and the job runtime
    /// (`serve` with [`WorkerMode::InServe`], `jobs work`) holds one `LISTEN`
    /// connection: an idle worker claims a new job within milliseconds
    /// instead of at its next poll. `FORGE_JOBS_NOTIFY=true|false` overrides
    /// it per deployment.
    #[cfg(feature = "jobs")]
    #[must_use]
    pub fn jobs_notify(mut self, on: bool) -> Self {
        let mut declaration = self
            .jobs
            .unwrap_or(JobsDeclaration::new(WorkerMode::InServe));
        declaration.notify = on;
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
            Ok(Command::MigrationStatus) => {
                self.checked(|app| app.run_database_command(DatabaseCommand::Status, load_config))
            }
            Ok(Command::Rollback { steps }) => self.checked(|app| {
                app.run_database_command(DatabaseCommand::Rollback { steps }, load_config)
            }),
            Ok(Command::Db(command)) => {
                self.checked(|app| app.run_db_command(command, load_config))
            }
            Ok(Command::OpenApi(output)) => {
                self.checked(|app| app.run_openapi_command(&output, load_config))
            }
            Ok(Command::Routes) => self.checked(|app| app.run_routes_command(load_config)),
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
        let info = self.openapi_info();
        let Self {
            name,
            version,
            registrations,
            migrate: _,
            rollback: _,
            mut declarations,
            compose_hooks,
            commands: _,
            api_info: _,
            #[cfg(feature = "jobs")]
            jobs,
        } = self;
        let declaration = declarations.pop();
        #[cfg(feature = "jobs")]
        let (mut jobs_slot, run_jobs_in_serve) = jobs_for_serve(jobs);
        #[cfg(not(feature = "jobs"))]
        let jobs_slot: JobsSlot = ();

        #[cfg_attr(not(feature = "jobs"), allow(unused_mut))]
        let mut config = match load_config() {
            Ok(config) => config,
            Err(error) => return config_error(&error),
        };
        #[cfg(feature = "jobs")]
        {
            resolve_jobs_notify(&mut config, jobs.as_ref());
            if let Some(parts) = jobs_slot.as_mut() {
                parts.notify = config.jobs.notify == Some(true);
            }
        }
        let telemetry = match process_telemetry::build(&config, name, version) {
            Ok(telemetry) => telemetry,
            Err(error) => return config_error(&error),
        };
        if let Err(error) = logging::init(
            &config.log,
            telemetry.as_ref().and_then(Telemetry::log_layer),
        ) {
            return config_error(&error);
        }
        process_telemetry::warn_public_metrics(&config);
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

        // One shutdown signal for the whole process: cancelled when shutdown
        // begins (a signal, or the server or job runtime failing). Composed
        // adapters see it through `Composition::shutdown_started`.
        let shutdown = Cancellation::new();
        // The pool is checked before telemetry starts and before any
        // composition hook sees it (ADR 0044).
        let database = match open_checked_pool(name, &runtime, declaration.zip(url), &config) {
            Ok(database) => database,
            Err(code) => return code,
        };
        // SQLx pools and the adapters composed around them need a Tokio
        // context even before the first request arrives.
        let composed = {
            let _guard = runtime.enter();
            if let Some(telemetry) = &telemetry {
                telemetry.start();
            }
            compose(
                name,
                &config,
                registrations,
                compose_hooks,
                database,
                jobs_slot,
                &shutdown,
                telemetry.as_ref(),
            )
        };
        let Built {
            mut router,
            database,
            workers,
            jobs: jobs_slot,
            pool,
        } = match composed {
            Ok(built) => built,
            Err(code) => return code,
        };
        if let Err(code) = mount_openapi(name, &mut router, &config, &info) {
            return code;
        }
        // Without `jobs` the slot types are `()`; the bindings keep one code path.
        #[cfg_attr(not(feature = "jobs"), allow(clippy::let_unit_value))]
        let background = background_for(jobs_slot, database.as_ref(), telemetry.as_ref());
        #[cfg(feature = "jobs")]
        let background = background.filter(|_| run_jobs_in_serve);

        let composed = Composed {
            router,
            database,
            workers,
            background,
            shutdown,
            telemetry,
            pool,
        };
        let result = runtime.block_on(serve_until_signal(name, version, config, composed));
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

    /// `db create|exists|drop`: server-level administration of the declared
    /// database, with the migration credential.
    fn run_db_command(
        mut self,
        command: DbCommand,
        load_config: impl FnOnce() -> Result<AppConfig, ConfigError>,
    ) -> ExitCode {
        let config = match load_config() {
            Ok(config) => config,
            Err(error) => return config_error(&error),
        };
        let Some(declaration) = self.declarations.pop() else {
            write_stderr(
                "error: `db` commands need a declared database \
                 (App::postgres, App::mysql or App::sqlite)\n",
            );
            return ExitCode::FAILURE;
        };
        let url = match declaration.validate(&config.database, UrlPurpose::Migration) {
            Ok(url) => url.clone(),
            Err(error) => return config_error(&error),
        };
        let runtime = match tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => {
                write_stderr(&format!(
                    "error: failed to start async runtime: {}\n",
                    ErrorChain(&error)
                ));
                return ExitCode::FAILURE;
            }
        };
        let code =
            runtime.block_on(declaration.run_admin_command(&url, command, config.environment));
        runtime.shutdown_timeout(RUNTIME_SHUTDOWN_TIMEOUT);
        code
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
        if let Err(error) = logging::init(&config.log, None) {
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
                DatabaseCommand::Status => MigrationCommand::Status,
                DatabaseCommand::Rollback { steps } => MigrationCommand::Rollback { steps },
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

        // Applications written before `App::postgres`: their own handlers,
        // which know neither `migrate status` nor several rollback steps.
        let handler = match command {
            DatabaseCommand::Migrate => self.migrate,
            DatabaseCommand::Rollback { steps: 1 } => self.rollback,
            DatabaseCommand::Status | DatabaseCommand::Rollback { .. } => {
                write_stderr(
                    "error: `migrate status` and `rollback --steps` need a declared database \
                     (App::postgres, App::mysql or App::sqlite)\n",
                );
                return ExitCode::FAILURE;
            }
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
    /// `openapi [--output FILE | --check FILE]`: composes the application
    /// without listening and prints its OpenAPI document (logs go to
    /// standard error), writes it to FILE, or exits 1 when FILE differs.
    /// It needs the configuration `serve` needs; database pools connect
    /// lazily, so no database has to be running unless a composition hook
    /// queries one.
    fn run_openapi_command(
        self,
        output: &OpenApiOutput,
        load_config: impl FnOnce() -> Result<AppConfig, ConfigError>,
    ) -> ExitCode {
        let info = self.openapi_info();
        let inspected = match self.compose_for_inspection("openapi", load_config) {
            Ok(inspected) => inspected,
            Err(code) => return code,
        };
        let document = inspected.built.router.openapi(&info);
        inspected.finish();
        let document = match document {
            Ok(document) => document,
            Err(error) => {
                write_stderr(&format!("{error}\n"));
                return ExitCode::FAILURE;
            }
        };
        write_openapi(&document, output)
    }

    /// `routes`: composes the application without listening and prints its
    /// routes as a table (logs go to standard error). Same requirements as
    /// `openapi`.
    fn run_routes_command(
        self,
        load_config: impl FnOnce() -> Result<AppConfig, ConfigError>,
    ) -> ExitCode {
        let inspected = match self.compose_for_inspection("routes", load_config) {
            Ok(inspected) => inspected,
            Err(code) => return code,
        };
        let table = crate::routes::render(&inspected.built.router);
        inspected.finish();
        write_stdout(&table)
    }

    /// Composes the application the way `serve` does, without telemetry,
    /// jobs running or a listener, for the commands that only inspect it.
    /// Logs go to standard error: standard output is the command's data.
    fn compose_for_inspection(
        self,
        command: &str,
        load_config: impl FnOnce() -> Result<AppConfig, ConfigError>,
    ) -> Result<Inspected, ExitCode> {
        let Self {
            name,
            registrations,
            mut declarations,
            compose_hooks,
            #[cfg(feature = "jobs")]
            jobs,
            ..
        } = self;
        #[cfg(feature = "jobs")]
        let (jobs_slot, _) = jobs_for_serve(jobs);
        #[cfg(not(feature = "jobs"))]
        let jobs_slot: JobsSlot = ();
        let declaration = declarations.pop();
        let config = match load_config() {
            Ok(config) => config,
            Err(error) => return Err(config_error(&error)),
        };
        if let Err(error) = logging::init_stderr(&config.log) {
            return Err(config_error(&error));
        }
        let url = match declaration
            .as_ref()
            .map(|declaration| declaration.validate(&config.database, UrlPurpose::Runtime))
            .transpose()
        {
            Ok(url) => url,
            Err(error) => return Err(config_error(&error)),
        };
        let runtime = match tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => {
                write_stderr(&format!(
                    "{command}: failed to start async runtime: {}\n",
                    ErrorChain(&error)
                ));
                return Err(ExitCode::FAILURE);
            }
        };
        let shutdown = Cancellation::new();
        let composed = {
            let _guard = runtime.enter();
            // Inspection never queries, so the role is not checked.
            open_pool(name, declaration.zip(url), &config).and_then(|database| {
                compose(
                    name,
                    &config,
                    registrations,
                    compose_hooks,
                    database,
                    jobs_slot,
                    &shutdown,
                    None,
                )
            })
        };
        Ok(Inspected {
            built: composed?,
            runtime,
            shutdown,
        })
    }

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
        if let Err(error) = logging::init(&config.log, None) {
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
        let mut config = match load_config() {
            Ok(config) => config,
            Err(error) => return config_error(&error),
        };
        resolve_jobs_notify(&mut config, self.jobs.as_ref());
        // Only `jobs work` runs long enough to export telemetry.
        let telemetry = if command == JobsCommand::Work {
            match process_telemetry::build(&config, self.name, self.version) {
                Ok(telemetry) => telemetry,
                Err(error) => return config_error(&error),
            }
        } else {
            None
        };
        if let Err(error) = logging::init(
            &config.log,
            telemetry.as_ref().and_then(Telemetry::log_layer),
        ) {
            return config_error(&error);
        }
        if telemetry.is_some() {
            process_telemetry::warn_public_metrics(&config);
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
            let output = runtime.block_on(crate::operator::execute(
                self.name,
                &url,
                command,
                config.environment,
            ));
            runtime.shutdown_timeout(RUNTIME_SHUTDOWN_TIMEOUT);
            return match output {
                Ok(text) => write_stdout(&text),
                Err(code) => code,
            };
        }

        // `jobs work` serves no HTTP; composed adapters that watch for
        // shutdown see this signal end with the process.
        let shutdown = Cancellation::new();
        // Checked before telemetry starts and before any composition hook
        // sees the pool (ADR 0044).
        let database =
            match open_checked_pool(self.name, &runtime, Some((declaration, &url)), &config) {
                Ok(database) => database,
                Err(code) => return code,
            };
        let composed = {
            let _guard = runtime.enter();
            if let Some(telemetry) = &telemetry {
                telemetry.start();
            }
            compose(
                self.name,
                &config,
                self.registrations,
                self.compose_hooks,
                database,
                Some(JobsParts::new(JobsDeclaration {
                    notify: config.jobs.notify == Some(true),
                    ..jobs
                })),
                &shutdown,
                telemetry.as_ref(),
            )
        };
        // Composition workers belong to the HTTP server's lifecycle (they
        // start with `serve`); a dedicated job worker process does not run them.
        let Built {
            database,
            jobs: jobs_slot,
            pool,
            ..
        } = match composed {
            Ok(built) => built,
            Err(code) => return code,
        };
        let Some(background) = background_for(jobs_slot, database.as_ref(), telemetry.as_ref())
        else {
            write_stderr("error: the jobs commands require App::postgres and App::jobs\n");
            return ExitCode::FAILURE;
        };
        let name = self.name;
        let grace = config.server.shutdown_grace;
        let metrics = config.telemetry.metrics_addr;
        let result = runtime.block_on(async move {
            let (result, began) =
                work_until_signal(name, background, telemetry.as_ref(), metrics).await;
            shutdown.cancel();
            drop(pool);
            if let Some(database) = database {
                database.close().await;
            }
            process_telemetry::shutdown(telemetry, began + grace).await;
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

/// Settles `FORGE_JOBS_NOTIFY` over `App::jobs_notify` (ADR 0046) into
/// `config.jobs.notify`, which the pool ([`open_pool`]) and the job store
/// follow. Without `App::jobs` nothing is announced.
#[cfg(feature = "jobs")]
fn resolve_jobs_notify(config: &mut AppConfig, jobs: Option<&JobsDeclaration>) {
    config.jobs.notify = Some(jobs.is_some_and(|jobs| jobs.notifies(&config.jobs)));
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
fn background_for(
    jobs: JobsSlot,
    database: Option<&DatabaseHandle>,
    telemetry: Option<&Telemetry>,
) -> BackgroundSlot {
    let parts = jobs?;
    #[allow(irrefutable_let_patterns)]
    let Some(DatabaseHandle::Postgres(database)) = database else {
        return None;
    };
    Some(parts.into_background(database.clone(), telemetry))
}

#[cfg(not(feature = "jobs"))]
fn background_for(
    jobs: JobsSlot,
    database: Option<&DatabaseHandle>,
    telemetry: Option<&Telemetry>,
) -> BackgroundSlot {
    let _ = (jobs, database, telemetry);
}

/// Runs the job runtime (and the metrics listener, when configured) alone
/// until a shutdown signal; also returns when shutdown began.
#[cfg(feature = "jobs")]
async fn work_until_signal(
    name: &'static str,
    background: Background,
    telemetry: Option<&Telemetry>,
    metrics: Option<SocketAddr>,
) -> (Result<(), ServeError>, Instant) {
    let shutdown = Cancellation::new();
    let began = async {
        shutdown.cancelled().await;
        Instant::now()
    };
    let work = async {
        let result = work(name, background, telemetry, metrics, &shutdown).await;
        shutdown.cancel();
        result
    };
    let (began, result) = tokio::join!(began, work);
    (result, began)
}

#[cfg(feature = "jobs")]
async fn work(
    name: &'static str,
    background: Background,
    telemetry: Option<&Telemetry>,
    metrics: Option<SocketAddr>,
    shutdown: &Cancellation,
) -> Result<(), ServeError> {
    let signal = signal::shutdown_signal().map_err(ServeError::Signal)?;
    let metrics = bind_metrics(telemetry, metrics).await?;
    info!(app = name, "job workers started");
    let run = async {
        let result = background.run(shutdown.clone()).await;
        shutdown.cancel();
        result.map_err(ServeError::Background)
    };
    let ((), result, served) = tokio::join!(
        watch_signal(name, signal, shutdown.clone()),
        run,
        serve_metrics(metrics, shutdown)
    );
    result.and(served)
}

/// The metrics listener, bound before serving starts so a taken port fails
/// startup.
async fn bind_metrics(
    telemetry: Option<&Telemetry>,
    address: Option<SocketAddr>,
) -> Result<Option<(Server, TcpListener)>, ServeError> {
    let (Some(telemetry), Some(address)) = (telemetry, address) else {
        return Ok(None);
    };
    let listener = TcpListener::bind(address)
        .await
        .map_err(|source| ServeError::Bind { address, source })?;
    let local = listener.local_addr().unwrap_or(address);
    #[cfg(test)]
    lifecycle_tests::listening(lifecycle_tests::Bound::Metrics, local);
    info!(address = %local, "metrics listening on GET /metrics");
    Ok(Some((
        process_telemetry::metrics_server(telemetry, address),
        listener,
    )))
}

/// Serves the metrics listener until `shutdown`.
async fn serve_metrics(
    metrics: Option<(Server, TcpListener)>,
    shutdown: &Cancellation,
) -> Result<(), ServeError> {
    let Some((server, listener)) = metrics else {
        return Ok(());
    };
    let stop = shutdown.clone();
    let result = server
        .serve(listener, async move { stop.cancelled().await })
        .await;
    shutdown.cancel();
    result.map_err(ServeError::from)
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

/// Serves the OpenAPI document at `GET /openapi.json` when
/// `FORGE_OPENAPI_ENABLED` allows it (development and test by default),
/// unless the application registered that route itself. The document is
/// built now, from the finished route table, so an inaccurate description
/// stops startup rather than failing a request.
fn mount_openapi(
    name: &'static str,
    router: &mut Router,
    config: &AppConfig,
    info: &Info,
) -> Result<(), ExitCode> {
    if !config.serves_openapi() {
        return Ok(());
    }
    if router.has_route(&Method::GET, OPENAPI_PATH) {
        info!(
            app = name,
            path = OPENAPI_PATH,
            "the application serves its own OpenAPI route; Forge's document is not mounted there"
        );
        return Ok(());
    }
    let document = router.openapi(info).map_err(|failure| {
        error!(app = name, error = %failure, "the OpenAPI document cannot be built");
        ExitCode::FAILURE
    })?;
    router.serve_openapi(&document).map_err(|failure| {
        error!(app = name, error = %failure, "failed to mount the OpenAPI document");
        ExitCode::FAILURE
    })?;
    if config.environment == Environment::Production {
        warn!(
            app = name,
            path = OPENAPI_PATH,
            "serving the OpenAPI document, which lists every documented route; \
             FORGE_OPENAPI_ENABLED=true enabled it"
        );
    }
    Ok(())
}

/// Prints the document, writes it to a file, or compares it with one, and
/// lists the undescribed routes on standard error.
fn write_openapi(document: &forge_openapi::Document, output: &OpenApiOutput) -> ExitCode {
    let undocumented = document.undocumented();
    if !undocumented.is_empty() {
        write_stderr(&format!(
            "openapi: {} routes are not described (forge::openapi::describe or typed): {}\n",
            undocumented.len(),
            undocumented.join(", ")
        ));
    }
    match output {
        OpenApiOutput::Stdout => {
            let mut stdout = io::stdout().lock();
            if let Err(error) = stdout
                .write_all(document.json())
                .and_then(|()| stdout.flush())
            {
                write_stderr(&format!("openapi: {error}\n"));
                return ExitCode::FAILURE;
            }
            ExitCode::SUCCESS
        }
        OpenApiOutput::Output(file) => {
            if let Err(error) = std::fs::write(file, document.json()) {
                write_stderr(&format!("openapi: write {}: {error}\n", file.display()));
                return ExitCode::FAILURE;
            }
            write_stderr(&format!(
                "openapi: wrote {} ({} paths)\n",
                file.display(),
                document.paths().len()
            ));
            ExitCode::SUCCESS
        }
        OpenApiOutput::Check(file) => match std::fs::read(file) {
            Err(error) => {
                write_stderr(&format!("openapi: read {}: {error}\n", file.display()));
                ExitCode::FAILURE
            }
            Ok(committed) if committed != document.json() => {
                write_stderr(&format!(
                    "openapi: {file} is out of date; run forge openapi --output {file}\n",
                    file = file.display()
                ));
                ExitCode::FAILURE
            }
            Ok(_) => {
                write_stderr(&format!("openapi: {} is up to date\n", file.display()));
                ExitCode::SUCCESS
            }
        },
    }
}

/// Refuses to start `serve`, `jobs work` or a `jobs` operator command when
/// the declared PostgreSQL pool's role could bypass row-level security
/// outside development and test, or cannot be checked (ADR 0044). The pool is
/// closed before returning the exit code; the error names the role and the
/// fix, never the URL.
pub(crate) async fn require_row_security(
    name: &'static str,
    database: Option<&DatabaseHandle>,
    environment: Environment,
) -> Result<(), ExitCode> {
    let Some(database) = database else {
        return Ok(());
    };
    match database.require_row_security(environment).await {
        Ok(()) => Ok(()),
        Err(error) => {
            error!(
                app = name,
                error = %ErrorChain(&error),
                "refusing to start: the database role could bypass row-level security, or could not be checked"
            );
            database.close().await;
            Err(ExitCode::FAILURE)
        }
    }
}

/// What composition built.
struct Built {
    router: Router,
    database: Option<DatabaseHandle>,
    workers: Vec<Worker>,
    jobs: JobsSlot,
    /// Reads the pool for the metrics until dropped (before it closes).
    pool: Option<PoolObservation>,
}

/// Creates the lazily connecting pool of the declared database. Must run
/// inside the Tokio runtime context.
fn open_pool(
    name: &'static str,
    declared: Option<(Declaration, &forge_config::SecretString)>,
    config: &AppConfig,
) -> Result<Option<DatabaseHandle>, ExitCode> {
    match declared {
        Some((declaration, url)) => {
            match declaration.connect_lazy(url, &config.database, config.jobs.notify == Some(true))
            {
                Ok(database) => Ok(Some(database)),
                Err(error) => {
                    error!(app = name, error = %ErrorChain(&error), "failed to create the database pool");
                    Err(ExitCode::FAILURE)
                }
            }
        }
        None => Ok(None),
    }
}

/// [`open_pool`] followed by [`require_row_security`], for the commands that
/// serve or work with the pool: the role is refused before telemetry starts
/// and before any composition hook receives the pool.
fn open_checked_pool(
    name: &'static str,
    runtime: &tokio::runtime::Runtime,
    declared: Option<(Declaration, &forge_config::SecretString)>,
    config: &AppConfig,
) -> Result<Option<DatabaseHandle>, ExitCode> {
    let database = {
        let _guard = runtime.enter();
        open_pool(name, declared, config)?
    };
    runtime.block_on(require_row_security(
        name,
        database.as_ref(),
        config.environment,
    ))?;
    Ok(database)
}

/// Builds the route table around the declared database pool, then runs the
/// composition hooks. Must run inside the Tokio runtime context.
#[allow(clippy::too_many_arguments)]
fn compose(
    name: &'static str,
    config: &AppConfig,
    registrations: Vec<RouteRegistration>,
    hooks: Vec<ComposeHook>,
    database: Option<DatabaseHandle>,
    jobs: JobsSlot,
    shutdown: &Cancellation,
    telemetry: Option<&Telemetry>,
) -> Result<Built, ExitCode> {
    #[cfg_attr(not(feature = "jobs"), allow(unused_mut, clippy::let_unit_value))]
    let mut jobs = jobs;
    let pool = telemetry
        .zip(database.as_ref())
        .and_then(|(telemetry, database)| process_telemetry::observe_database(telemetry, database));
    let mut router = match build_router(registrations, database.clone()) {
        Ok(router) => router,
        Err(error) => {
            error!(app = name, error = %ErrorChain(&error), "failed to register routes");
            return Err(ExitCode::FAILURE);
        }
    };
    let mut workers = Vec::new();
    if !hooks.is_empty() {
        let mut composition = Composition::new(
            &mut router,
            config,
            database.clone(),
            shutdown.clone(),
            telemetry.cloned(),
        );
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
    Ok(Built {
        router,
        database,
        workers,
        jobs,
        pool,
    })
}

#[derive(Clone, Copy)]
enum DatabaseCommand {
    Migrate,
    Status,
    Rollback { steps: u32 },
}

impl DatabaseCommand {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Migrate => "migrate",
            Self::Status => "migrate status",
            Self::Rollback { .. } => "rollback",
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

/// An application composed without listening (see
/// `App::compose_for_inspection`).
struct Inspected {
    built: Built,
    runtime: tokio::runtime::Runtime,
    shutdown: Cancellation,
}

impl Inspected {
    /// Stops what composition started: the shutdown signal, the database
    /// pool and the runtime.
    fn finish(self) {
        self.shutdown.cancel();
        if let Some(database) = self.built.database {
            self.runtime.block_on(database.close());
        }
        self.runtime.shutdown_timeout(RUNTIME_SHUTDOWN_TIMEOUT);
    }
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
    #[error("the HTTP telemetry instruments could not be registered")]
    Telemetry(#[source] MetricError),
    #[cfg(feature = "jobs")]
    #[error("background jobs failed")]
    Background(#[source] BackgroundError),
}

/// What `serve` runs: the composed application and its shutdown signal.
struct Composed {
    router: Router,
    database: Option<DatabaseHandle>,
    workers: Vec<Worker>,
    background: BackgroundSlot,
    shutdown: Cancellation,
    telemetry: Option<Telemetry>,
    pool: Option<PoolObservation>,
}

async fn serve_until_signal(
    name: &'static str,
    version: &'static str,
    config: AppConfig,
    composed: Composed,
) -> Result<(), ServeError> {
    let Composed {
        router,
        database,
        workers,
        background,
        shutdown,
        telemetry,
        pool,
    } = composed;
    let grace = config.server.shutdown_grace;
    let workers = RunningWorkers::start(workers);
    let began = async {
        shutdown.cancelled().await;
        Instant::now()
    };
    let run = async {
        let result = serve(
            name,
            version,
            config,
            router,
            background,
            shutdown.clone(),
            telemetry.as_ref(),
        )
        .await;
        shutdown.cancel();
        result
    };
    let (began, result) = tokio::join!(began, run);
    // Workers stop after the server drained, before the pool closes.
    workers.stop().await;
    drop(pool);
    if let Some(database) = database {
        database.close().await;
    }
    // Last, so the spans of everything above are exported: what is queued
    // is flushed within what remains of the shutdown grace period.
    process_telemetry::shutdown(telemetry, began + grace).await;
    result
}

async fn serve(
    name: &'static str,
    version: &'static str,
    config: AppConfig,
    router: Router,
    background: BackgroundSlot,
    shutdown: Cancellation,
    telemetry: Option<&Telemetry>,
) -> Result<(), ServeError> {
    let signal = signal::shutdown_signal().map_err(ServeError::Signal)?;
    let instrumentation = telemetry
        .map(Telemetry::http_server)
        .transpose()
        .map_err(ServeError::Telemetry)?;
    let bind = config.server.bind;
    let listener = TcpListener::bind(bind)
        .await
        .map_err(|source| ServeError::Bind {
            address: bind,
            source,
        })?;
    let metrics = bind_metrics(telemetry, config.telemetry.metrics_addr).await?;
    let address = listener.local_addr().unwrap_or(bind);
    #[cfg(test)]
    lifecycle_tests::listening(lifecycle_tests::Bound::Http, address);
    info!(
        app = name,
        version,
        environment = environment_name(config.environment),
        %address,
        trusted_proxies = config.server.trusted_proxies.len(),
        telemetry = telemetry.is_some(),
        "listening"
    );
    if config.environment == Environment::Production
        && config.server.transport == forge_config::HttpTransport::Plain
    {
        // Only reachable with a configuration built in code: loading it
        // from the environment refuses plain in production.
        warn!(
            "FORGE_ENV is production but the HTTP transport is plain: responses carry no \
             Strict-Transport-Security; use trusted-proxy behind TLS termination"
        );
    }

    // One shutdown for the server and the job runtime: a signal, or either
    // of them failing, stops both.
    let server = async {
        let stop = shutdown.clone();
        let mut server = Server::new(config.server, router);
        if let Some(instrumentation) = instrumentation {
            server = server.with_telemetry(instrumentation);
        }
        let result = server
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
    let ((), served, ran, metrics) = tokio::join!(
        watch_signal(name, signal, shutdown.clone()),
        server,
        jobs,
        serve_metrics(metrics, &shutdown)
    );
    served.and(ran).and(metrics)
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
pub(crate) fn write_stdout(text: &str) -> ExitCode {
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
pub(crate) fn write_stderr(text: &str) {
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
mod lifecycle_tests;
#[cfg(all(test, feature = "mysql"))]
mod mysql_tests;
#[cfg(all(test, feature = "postgres"))]
mod postgres_tests;

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

    fn described_app() -> App {
        App::new("demo").version("1.2.3").routes(|router| {
            router.route(Method::GET, "/plain", |_| async {
                Response::empty(StatusCode::OK)
            })?;
            Ok(())
        })
    }

    #[test]
    fn the_openapi_command_writes_and_checks_the_document() {
        let directory = tempfile::tempdir().expect("tempdir");
        let file = directory.path().join("openapi.json");
        let path = file.to_string_lossy().into_owned();

        let code = described_app().run_with(args(&["openapi", "--output", &path]), no_config);
        assert_eq!(code, ExitCode::SUCCESS);
        let written = std::fs::read_to_string(&file).expect("written");
        let document: serde_json::Value = serde_json::from_str(&written).expect("JSON");
        assert_eq!(document["info"]["title"], "demo");
        assert_eq!(document["info"]["version"], "1.2.3");
        assert!(document["paths"]["/health/live"]["get"].is_object());
        assert!(document["paths"].get("/plain").is_none(), "undescribed");

        let check = |app: App| app.run_with(args(&["openapi", "--check", &path]), no_config);
        assert_eq!(check(described_app()), ExitCode::SUCCESS);
        let renamed = described_app().api_info(Info::new("renamed", "1.2.3"));
        assert_eq!(check(renamed), ExitCode::FAILURE, "stale");
        assert_eq!(
            described_app().run_with(args(&["openapi", "--yaml"]), no_config),
            ExitCode::from(EXIT_USAGE)
        );
    }

    #[test]
    fn the_routes_command_composes_without_serving() {
        assert_eq!(
            described_app().run_with(args(&["routes"]), no_config),
            ExitCode::SUCCESS
        );
        assert_eq!(
            described_app().run_with(args(&["routes", "--json"]), no_config),
            ExitCode::from(EXIT_USAGE)
        );
        // An application cannot replace the built-in command.
        let replaced = described_app().command("routes", "mine", |_| async { Ok(()) });
        assert_eq!(
            replaced.run_with(args(&["help"]), no_config),
            ExitCode::SUCCESS
        );
        assert_eq!(
            described_app()
                .command("routes", "mine", |_| async { Ok(()) })
                .run_with(args(&["serve"]), no_config),
            ExitCode::FAILURE
        );
    }

    fn served(config: &AppConfig, own: bool) -> Result<Router, ExitCode> {
        let mut router = build_router(Vec::new(), None).expect("routes");
        if own {
            router
                .route(Method::GET, OPENAPI_PATH, |_| async {
                    Response::new(StatusCode::OK, "mine")
                })
                .expect("route");
        }
        mount_openapi("demo", &mut router, config, &Info::new("demo", "1"))?;
        Ok(router)
    }

    #[test]
    fn the_document_is_served_where_the_configuration_allows_it() {
        let development = config_with(&[]).expect("config");
        let router = served(&development, false).expect("mounted");
        assert!(router.has_route(&Method::GET, OPENAPI_PATH));
        let production = config_with(&[("FORGE_ENV", "production")]).expect("config");
        let router = served(&production, false).expect("not mounted");
        assert!(!router.has_route(&Method::GET, OPENAPI_PATH));
        let enabled = config_with(&[
            ("FORGE_ENV", "production"),
            ("FORGE_OPENAPI_ENABLED", "true"),
        ])
        .expect("config");
        assert!(
            served(&enabled, false)
                .expect("mounted")
                .has_route(&Method::GET, OPENAPI_PATH)
        );
        // The application's own route wins.
        assert!(served(&development, true).is_ok());
    }

    #[test]
    fn an_inaccurate_description_stops_startup() {
        let mut router = Router::new();
        router
            .route(
                Method::GET,
                "/x",
                forge_http::describe(
                    |_| async { Response::empty(StatusCode::OK) },
                    forge_openapi::Operation::new(),
                ),
            )
            .expect("route");
        let config = config_with(&[]).expect("config");
        let result = mount_openapi("demo", &mut router, &config, &Info::new("demo", "1"));
        assert_eq!(result, Err(ExitCode::FAILURE));
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
                Err(ConfigError::NotUnicode {
                    key: "FORGE_HTTP_ADDR",
                })
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
                Err(ConfigError::NotUnicode {
                    key: "FORGE_HTTP_ADDR",
                })
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
            has_table(url, "notes").await
        }

        async fn has_table(url: &str, table: &str) -> bool {
            let database = forge_db::SqliteDatabase::connect(&SecretString::new(url))
                .await
                .expect("connect");
            let exists = sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?",
            )
            .bind(table)
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

        #[tokio::test(flavor = "multi_thread")]
        async fn status_lists_migrations_and_rollback_steps_revert_several() {
            let directory = tempfile::tempdir().expect("tempdir");
            let url = url(&directory);
            let run = |arguments: &'static [&'static str]| {
                let url = url.clone();
                tokio::task::spawn_blocking(move || {
                    App::new("demo")
                        .sqlite(sqlx::migrate!("tests/migrations/sqlite_steps"))
                        .run_with(args(arguments), || {
                            config_with(&[("FORGE_MIGRATION_DATABASE_URL", url.as_str())])
                        })
                })
            };

            // Status works before anything ran, applies nothing and does not
            // even create the bookkeeping table.
            assert_eq!(
                run(&["migrate", "status"]).await.expect("join"),
                ExitCode::SUCCESS
            );
            assert!(!has_table(&url, "notes").await);
            assert!(!has_table(&url, "_sqlx_migrations").await);

            assert_eq!(run(&["migrate"]).await.expect("join"), ExitCode::SUCCESS);
            assert!(has_table(&url, "labels").await);
            assert_eq!(
                run(&["migrate", "status"]).await.expect("join"),
                ExitCode::SUCCESS
            );

            assert_eq!(
                run(&["rollback", "--steps", "2"]).await.expect("join"),
                ExitCode::SUCCESS
            );
            assert!(!has_table(&url, "labels").await && !has_table(&url, "tags").await);
            assert!(has_table(&url, "notes").await, "the oldest is kept");

            // More steps than migrations stops at the last one.
            assert_eq!(
                run(&["rollback", "--steps", "5"]).await.expect("join"),
                ExitCode::SUCCESS
            );
            assert!(!has_table(&url, "notes").await);
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn db_commands_create_check_and_drop_with_a_production_guard() {
            let directory = tempfile::tempdir().expect("tempdir");
            let url = url(&directory);
            let file = directory.path().join("app.db");
            let run = |arguments: &'static [&'static str], environment: &'static str| {
                let url = url.clone();
                tokio::task::spawn_blocking(move || {
                    App::new("demo")
                        .sqlite(migrations())
                        .run_with(args(arguments), || {
                            config_with(&[
                                ("FORGE_ENV", environment),
                                ("FORGE_MIGRATION_DATABASE_URL", url.as_str()),
                            ])
                        })
                })
            };

            assert_eq!(
                run(&["db", "exists"], "development").await.expect("join"),
                ExitCode::from(crate::database::EXIT_DATABASE_MISSING)
            );
            assert_eq!(
                run(&["db", "create"], "development").await.expect("join"),
                ExitCode::SUCCESS
            );
            assert!(file.exists());
            assert_eq!(
                run(&["db", "create"], "development").await.expect("join"),
                ExitCode::SUCCESS,
                "creating twice is not an error"
            );
            assert_eq!(
                run(&["db", "exists"], "development").await.expect("join"),
                ExitCode::SUCCESS
            );

            // Production refuses without --force-production, and changes nothing.
            assert_eq!(
                run(&["db", "drop"], "production").await.expect("join"),
                ExitCode::FAILURE
            );
            assert!(file.exists());
            assert_eq!(
                run(&["db", "drop", "--force-production"], "production")
                    .await
                    .expect("join"),
                ExitCode::SUCCESS
            );
            assert!(!file.exists());
            assert_eq!(
                run(&["db", "drop"], "development").await.expect("join"),
                ExitCode::SUCCESS,
                "dropping a missing database is not an error"
            );
        }

        #[test]
        fn db_commands_need_a_declared_database_and_a_matching_url() {
            let undeclared = App::new("demo").run_with(args(&["db", "create"]), no_config);
            assert_eq!(undeclared, ExitCode::FAILURE);
            let missing = App::new("demo")
                .sqlite(migrations())
                .run_with(args(&["db", "create"]), no_config);
            assert_eq!(missing, ExitCode::from(EXIT_CONFIG));
            let legacy = App::new("demo")
                .migrations(
                    |_| async { Ok::<(), io::Error>(()) },
                    |_| async { Ok::<(), io::Error>(()) },
                )
                .run_with(args(&["migrate", "status"]), || {
                    config_with(&[("FORGE_MIGRATION_DATABASE_URL", "sqlite:x.db")])
                });
            assert_eq!(legacy, ExitCode::FAILURE);
        }

        /// Regression (rocketinsidecorp/forge-rust#10): with the shipped
        /// cache and rate-limit migrations merged in (versions higher than the
        /// application's), `rollback` reverts the application's migration and
        /// refuses instead of dropping a framework table; `migrate` keeps
        /// working after the features are turned off.
        #[cfg(all(feature = "cache", feature = "ratelimit"))]
        #[tokio::test(flavor = "multi_thread")]
        async fn rollback_keeps_framework_tables_and_migrate_tolerates_disabled_features() {
            let directory = tempfile::tempdir().expect("tempdir");
            let url = url(&directory);
            let run = |command: &'static str, with_framework: bool| {
                let url = url.clone();
                tokio::task::spawn_blocking(move || {
                    let app = migrations();
                    let migrator = if with_framework {
                        forge_db::combine_migrations(&[
                            &app,
                            &forge_cache::sqlite::MIGRATIONS,
                            &forge_ratelimit::sqlite::MIGRATIONS,
                        ])
                        .expect("combine")
                    } else {
                        app
                    };
                    App::new("demo")
                        .sqlite(migrator)
                        .run_with(args(&[command]), || {
                            config_with(&[("FORGE_MIGRATION_DATABASE_URL", url.as_str())])
                        })
                })
            };
            let framework_tables = || async {
                has_table(&url, "forge_cache_entries").await
                    && has_table(&url, "forge_rate_limits").await
            };

            assert_eq!(run("migrate", true).await.expect("join"), ExitCode::SUCCESS);
            assert!(table_exists(&url).await && framework_tables().await);

            assert_eq!(
                run("rollback", true).await.expect("join"),
                ExitCode::SUCCESS
            );
            assert!(
                !table_exists(&url).await,
                "the application migration was reverted"
            );
            assert!(
                framework_tables().await,
                "framework tables survive rollback"
            );

            // Only framework migrations remain: rollback fails, reverts nothing.
            assert_eq!(
                run("rollback", true).await.expect("join"),
                ExitCode::FAILURE
            );
            assert!(framework_tables().await);

            // The features are turned off: their applied versions are tolerated.
            assert_eq!(
                run("migrate", false).await.expect("join"),
                ExitCode::SUCCESS
            );
            assert!(table_exists(&url).await);
            assert_eq!(
                run("rollback", false).await.expect("join"),
                ExitCode::SUCCESS
            );
            assert!(!table_exists(&url).await && framework_tables().await);
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
            // Without `jobs` the slot is `()`, as in `serve`.
            #[cfg_attr(not(feature = "jobs"), allow(clippy::unit_arg))]
            let Built {
                router, database, ..
            } = compose(
                "demo",
                &config,
                Vec::new(),
                vec![hook],
                open_pool(
                    "demo",
                    Some((Declaration::Sqlite(forge_db::Migrator::DEFAULT), url)),
                    &config,
                )
                .expect("pool"),
                JobsSlot::default(),
                &Cancellation::new(),
                None,
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
                #[cfg_attr(not(feature = "jobs"), allow(clippy::unit_arg))]
                let outcome = compose(
                    "demo",
                    &config,
                    Vec::new(),
                    vec![hook],
                    open_pool(
                        "demo",
                        Some((Declaration::Sqlite(forge_db::Migrator::DEFAULT), url)),
                        &config,
                    )
                    .expect("pool"),
                    JobsSlot::default(),
                    &Cancellation::new(),
                    None,
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
        fn forge_jobs_notify_overrides_the_code_choice_and_needs_app_jobs() {
            // App::jobs_notify declares jobs (InServe) unless they were declared.
            let declaration = App::new("demo").jobs_notify(true).jobs.expect("declared");
            assert!(declaration.notify && declaration.mode == WorkerMode::InServe);
            let declaration = App::new("demo")
                .jobs(WorkerMode::Dedicated)
                .jobs_notify(true)
                .jobs
                .expect("declared");
            assert!(declaration.notify && declaration.mode == WorkerMode::Dedicated);
            assert!(!JobsParts::new(JobsDeclaration::new(WorkerMode::InServe)).notify);
            assert!(JobsParts::new(declaration).notify);

            let off = JobsDeclaration::new(WorkerMode::InServe);
            for (code, env, declared, expected) in [
                (false, None, true, false),
                (true, None, true, true),
                (false, Some(true), true, true),
                (true, Some(false), true, false),
                // Without App::jobs nothing announces, whatever the variable.
                (true, Some(true), false, false),
            ] {
                let mut config = AppConfig::default();
                config.jobs.notify = env;
                let declaration = JobsDeclaration {
                    notify: code,
                    ..off
                };
                resolve_jobs_notify(&mut config, declared.then_some(&declaration));
                assert_eq!(
                    config.jobs.notify,
                    Some(expected),
                    "{code} {env:?} {declared}"
                );
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
            let mut composition =
                Composition::new(&mut router, &config, None, Cancellation::new(), None);
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
            // FORGE_JOBS_NOTIFY: the pool announces and the runtime listens.
            let mut config = config_with(&[
                ("FORGE_DATABASE_URL", url.as_str()),
                (forge_config::JOBS_NOTIFY_KEY, "true"),
            ])
            .expect("config");
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
            resolve_jobs_notify(&mut config, Some(&declaration));
            declaration.notify = config.jobs.notify == Some(true);
            let Built {
                database,
                jobs: slot,
                ..
            } = compose(
                "demo",
                &config,
                Vec::new(),
                vec![hook],
                open_pool(
                    "demo",
                    Some((
                        Declaration::Postgres(forge_db::Migrator::DEFAULT),
                        &runtime_url,
                    )),
                    &config,
                )
                .expect("pool"),
                Some(JobsParts::new(declaration)),
                &Cancellation::new(),
                None,
            )
            .expect("composition");
            let background = background_for(slot, database.as_ref(), None).expect("background");
            #[allow(irrefutable_let_patterns)]
            let Some(DatabaseHandle::Postgres(pool)) = database.as_ref() else {
                panic!("a PostgreSQL pool");
            };
            let setting: Option<String> =
                sqlx::query_scalar("SELECT current_setting('forge.jobs_notify', true)")
                    .fetch_one(pool.pool())
                    .await
                    .expect("setting");
            assert_eq!(setting.as_deref(), Some("on"));

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
            while sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM pg_stat_activity
                 WHERE datname = current_database() AND query = 'LISTEN \"forge_jobs\"'",
            )
            .fetch_one(test.database().pool())
            .await
            .expect("listeners")
                != 1
            {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "the runtime did not listen"
                );
                // real-clock: polls the runtime's LISTEN connection on a real database, opened by a background task, until a deadline
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            let expected_events = usize::from(cfg!(feature = "events"));
            while greeted.load(Ordering::SeqCst) == 0
                || heard.load(Ordering::SeqCst) < expected_events
            {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "the runtime did not run the job"
                );
                // real-clock: polls state written by a background task through a real database, until a deadline
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
