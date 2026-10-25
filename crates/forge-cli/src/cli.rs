use std::time::Duration;

use clap::{ArgGroup, Args, Parser, Subcommand, ValueEnum};

use crate::dev::{parse_debounce, parse_poll_interval};
use crate::generate::{Database, Locale};

#[derive(Debug, Parser)]
#[command(
    name = "forge",
    version,
    about = "Build and maintain Forge applications",
    arg_required_else_help = true
)]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) command: Command,
}

impl Cli {
    pub(crate) fn parse_process() -> Self {
        Self::parse()
    }
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Generate a sortable UUIDv7 identifier.
    Id,
    /// Create a new full-stack application (server-rendered views; `--api`
    /// for JSON only) with hexagonal boundaries, Docker and CI.
    New {
        /// Cargo package name and destination directory.
        name: String,
        /// Do not generate Dockerfile, .dockerignore, compose.yaml or the image CI job.
        #[arg(long)]
        skip_docker: bool,
        /// Do not generate the GitHub Actions workflow and Dependabot configuration.
        #[arg(long)]
        skip_ci: bool,
        /// Database backend: postgresql (alias postgres), mysql, mariadb,
        /// sqlite3 (alias sqlite) or none (no SQLx, migrations or database services).
        #[arg(
            short = 'd',
            long,
            value_enum,
            value_name = "DATABASE",
            default_value_t = Database::Postgresql
        )]
        database: Database,
        /// Generate an API-only application (like `rails new --api`): JSON
        /// handlers without views, templates, assets, Askama or the HTML
        /// request layers.
        #[arg(long)]
        api: bool,
        /// Do not run `cargo generate-lockfile` after generation.
        #[arg(long)]
        skip_lockfile: bool,
        /// Depend on a local Forge checkout instead of the release tag. Relative
        /// paths are resolved by Cargo from the new application's directory.
        #[arg(long, value_name = "PATH")]
        forge_path: Option<String>,
        /// Default locale of the pages: en, es or pt-BR. Writes
        /// config/locales/en.json and the locale's file (plus the sign-in
        /// pages' authentication.LOCALE.json).
        #[arg(
            long,
            value_enum,
            value_name = "LOCALE",
            default_value_t = Locale::En,
            conflicts_with = "api"
        )]
        locale: Locale,
    },
    /// Generate application artifacts.
    Generate {
        #[command(subcommand)]
        generator: GenerateCommand,
    },
    /// Remove what a generator created (like `rails destroy`): give the
    /// arguments the generator was run with. Only files that still match
    /// what the generator writes are removed; migrations are kept unless
    /// --migrations.
    Destroy {
        #[command(subcommand)]
        generator: DestroyCommand,
    },
    /// Type-check the current application.
    Check,
    /// Run the current application's tests (`cargo test`). ARGS replace the
    /// default `--all-targets --all-features`, like `forge test ARGS`
    /// replaces `./...` in Forge-Go.
    Test {
        /// Arguments for `cargo test`, for example `--lib orders -- --nocapture`.
        #[arg(
            allow_hyphen_values = true,
            trailing_var_arg = true,
            value_name = "ARGS"
        )]
        args: Vec<String>,
    },
    /// Format the current application's Rust code.
    Format,
    /// Run Clippy with warnings treated as errors.
    Lint,
    /// Build the optimized release binary from the locked dependency graph.
    Build {
        /// The binary to build, when the package has several.
        name: Option<String>,
    },
    /// Print the Forge CLI version.
    Version,
    /// Check the Rust toolchain, the FORGE_* configuration and that the
    /// configured database server answers. Exits 1 when something is wrong.
    Doctor {
        /// Print a versioned, secret-free JSON report.
        #[arg(long)]
        json: bool,
    },
    /// Run the application; rebuild it and restart it gracefully when files
    /// change. The old process keeps serving until a build succeeds.
    Dev {
        /// The binary to run, when the package has several.
        name: Option<String>,
        /// Build and run once: no file watching or restarts.
        #[arg(long)]
        no_reload: bool,
        /// How often the source tree is scanned for changes (50ms to 1m).
        #[arg(
            long,
            value_name = "D",
            value_parser = parse_poll_interval,
            default_value = DEFAULT_POLL_INTERVAL_TEXT
        )]
        poll_interval: Duration,
        /// Quiet time after a change before rebuilding (0 to 1m).
        #[arg(
            long,
            value_name = "D",
            value_parser = parse_debounce,
            default_value = DEFAULT_DEBOUNCE_TEXT
        )]
        debounce: Duration,
    },
    /// Apply pending migrations by running the application's `migrate`
    /// command (the same as `forge db migrate`); `status` lists them.
    Migrate {
        /// `status` lists every migration instead of applying them.
        #[arg(value_enum)]
        view: Option<MigrateView>,
    },
    /// Revert the latest application migration(s) by running the
    /// application's `rollback` command; Forge's own migrations are never
    /// reverted.
    Rollback {
        /// How many migrations to revert.
        #[arg(long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..=1000))]
        steps: Option<u32>,
    },
    /// Create the first tenant and its owner by running the application's
    /// `setup` command. The password is prompted for (echo off), piped with
    /// `--password-stdin` or read from FORGE_SETUP_PASSWORD; never an argument.
    Setup {
        /// Tenant slug, the organization's sign-in name.
        #[arg(long, value_name = "SLUG")]
        tenant: String,
        /// Owner's email address.
        #[arg(long, value_name = "EMAIL")]
        email: String,
        /// Organization display name (default: the slug).
        #[arg(long, value_name = "NAME")]
        organization: Option<String>,
        /// Read the password as one line from standard input.
        #[arg(long)]
        password_stdin: bool,
    },
    /// Load the application's seed data by running its `seed` command.
    Seed,
    /// Print the application's OpenAPI 3.1 document by running its `openapi`
    /// command (ADR 0039): it composes the application without listening,
    /// so it needs the configuration `serve` needs.
    Openapi {
        /// Write the document to FILE instead of standard output.
        #[arg(long, value_name = "FILE", conflicts_with = "check")]
        output: Option<std::path::PathBuf>,
        /// Exit 1 when FILE differs from the document (for CI).
        #[arg(long, value_name = "FILE")]
        check: Option<std::path::PathBuf>,
    },
    /// Print every route of the application as a table (METHOD, PATH, ACTION,
    /// SOURCE) by running its `routes` command: HTML and JSON
    /// routes alike, with the source line that registered each. It composes
    /// the application without listening, so it needs the configuration
    /// `serve` needs.
    Routes,
    /// Encrypted per-environment credentials in
    /// config/credentials/ENV.json.enc (like `rails credentials:edit`, ADR
    /// 0041). The key is FORGE_MASTER_KEY or config/credentials/ENV.key.
    Credentials {
        #[command(subcommand)]
        command: CredentialsCommand,
    },
    /// Translations of config/locales (ADR 0037).
    I18n {
        #[command(subcommand)]
        command: I18nCommand,
    },
    /// Database lifecycle, like Rails' `db:*` tasks. Every command runs the
    /// application's own subcommand, except `console`, which runs the
    /// database's own client, and the privilege restore of `reset`, which
    /// runs `psql`: only those read the database URL.
    Db {
        #[command(subcommand)]
        command: DbCommand,
    },
    /// Build, push and deploy the application image to hosts over SSH
    /// (like Kamal): health-gated zero-downtime switch through forge-proxy,
    /// migrations before the switch, all-or-nothing revert, deploy lock and
    /// retained versions for rollback (ADR 0042).
    Deploy(DeployArgs),
    /// Stream docker logs of the current role container from every matching
    /// host, each line prefixed with host|; Ctrl-C stops the remote streams.
    Logs(LogsArgs),
    /// Run a command next to the deployed application, or open a shell.
    App {
        #[command(subcommand)]
        command: AppCommand,
    },
}

/// The flags every `forge deploy` command accepts, before or after the
/// subcommand.
#[derive(Clone, Debug, Args)]
pub(crate) struct DeployCommon {
    /// Deploy configuration.
    #[arg(
        short = 'c',
        long,
        value_name = "PATH",
        default_value = "config/deploy.yml",
        global = true
    )]
    pub(crate) config: std::path::PathBuf,
    /// Secrets file; must be mode 0600.
    #[arg(
        long,
        value_name = "PATH",
        default_value = ".forge/secrets",
        global = true
    )]
    pub(crate) secrets: std::path::PathBuf,
    /// Print the remote and local commands with secrets redacted; read-only
    /// queries (docker ps, lock status) still run.
    #[arg(long, global = true)]
    pub(crate) dry_run: bool,
    /// Print a schema-versioned report on stdout (also on failure); progress
    /// goes to stderr.
    #[arg(long, global = true)]
    pub(crate) json: bool,
}

/// `forge deploy`.
#[derive(Debug, Args)]
pub(crate) struct DeployArgs {
    #[command(subcommand)]
    pub(crate) command: Option<DeployCommand>,
    #[command(flatten)]
    pub(crate) common: DeployCommon,
    /// Image tag to deploy; default is the abbreviated git commit of a clean
    /// tree.
    #[arg(long = "version", value_name = "TAG")]
    pub(crate) tag: Option<String>,
    /// Deploy a tag already in the registry instead of building and pushing.
    #[arg(long)]
    pub(crate) skip_build: bool,
}

#[derive(Debug, Subcommand)]
pub(crate) enum DeployCommand {
    /// Write a commented config/deploy.yml and .forge/secrets (0600); add
    /// .forge/secrets to .gitignore and .forge to .dockerignore. Never
    /// overwrites.
    Init {
        /// Service name (default: from Cargo.toml).
        #[arg(long, value_name = "NAME")]
        service: Option<String>,
    },
    /// Check Docker, create ~/.forge/SERVICE (0700) and the forge network,
    /// log in to the registry, start forge-proxy and accessories.
    /// Idempotent.
    Setup,
    /// Restart the retained containers of VERSION on every host through the
    /// same health-gated switch. Migrations are not reverted.
    Rollback {
        /// A version still retained on every host.
        version: String,
    },
    /// Remove stopped containers beyond retain per role, and images no
    /// remaining container uses. Never touches running containers.
    Prune,
    /// Containers and versions per host and role, proxy routing and
    /// maintenance, lock holder, last deploys.
    Details,
    /// Inspect or hold the deploy lock (for maintenance windows).
    Lock {
        #[arg(value_enum)]
        action: LockActionArg,
        /// acquire: reason recorded with the lock.
        #[arg(long, value_name = "TEXT", default_value = "manual lock")]
        message: String,
        /// release: release a lock held by someone else.
        #[arg(long)]
        force: bool,
    },
    /// Serve forge-proxy's static 503 page (with Retry-After) instead of the
    /// application.
    Maintenance {
        #[arg(value_enum)]
        state: Toggle,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(crate) enum LockActionArg {
    Status,
    Acquire,
    Release,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(crate) enum Toggle {
    On,
    Off,
}

/// The flags of `forge logs` and `forge app`.
#[derive(Clone, Debug, Args)]
pub(crate) struct RemoteCommon {
    /// Deploy configuration.
    #[arg(
        short = 'c',
        long,
        value_name = "PATH",
        default_value = "config/deploy.yml"
    )]
    pub(crate) config: std::path::PathBuf,
    /// Secrets file; must be mode 0600.
    #[arg(long, value_name = "PATH", default_value = ".forge/secrets")]
    pub(crate) secrets: std::path::PathBuf,
    /// Print the remote command with secrets redacted instead of running it.
    #[arg(long)]
    pub(crate) dry_run: bool,
}

/// `forge logs`.
#[derive(Debug, Args)]
pub(crate) struct LogsArgs {
    #[command(flatten)]
    pub(crate) common: RemoteCommon,
    /// Keep streaming new lines.
    #[arg(short = 'f', long)]
    pub(crate) follow: bool,
    /// Role (default: the proxied role, else web).
    #[arg(long, value_name = "ROLE")]
    pub(crate) role: Option<String>,
    /// Only this host.
    #[arg(long, value_name = "HOST")]
    pub(crate) host: Option<String>,
    /// Only lines since 1h, 30m or a timestamp.
    #[arg(long, value_name = "DURATION")]
    pub(crate) since: Option<String>,
    /// Only lines matching this RE2 expression (filtered locally).
    #[arg(long, value_name = "REGEX")]
    pub(crate) grep: Option<String>,
    /// Lines per host to start with (default 100, 10 with --follow).
    #[arg(
        long,
        value_name = "N",
        default_value_t = 0,
        allow_negative_numbers = true
    )]
    pub(crate) lines: i64,
}

#[derive(Debug, Subcommand)]
pub(crate) enum AppCommand {
    /// Run CMD on one host (the first of the role unless --host): in a
    /// one-off container of the image the role runs, with its env file, so
    /// CMD goes to the image entrypoint (`forge app exec migrate status`);
    /// --reuse runs inside the running container.
    Exec {
        #[command(flatten)]
        common: RemoteCommon,
        /// Role (default: the proxied role, else web).
        #[arg(long, value_name = "ROLE")]
        role: Option<String>,
        /// Host (default: the first host of the role).
        #[arg(long, value_name = "HOST")]
        host: Option<String>,
        /// Allocate a terminal (ssh -t, docker -it).
        #[arg(short = 'i', long)]
        interactive: bool,
        /// Run inside the running container instead of a one-off container.
        #[arg(long)]
        reuse: bool,
        /// The command and its arguments (flags must precede it).
        #[arg(
            required = true,
            trailing_var_arg = true,
            allow_hyphen_values = true,
            value_name = "CMD"
        )]
        command: Vec<String>,
    },
    /// Interactive shell over ssh -t. Distroless images have no shell:
    /// --image busybox:1.37 starts a sidecar sharing the container's network
    /// and process namespaces, without the application's environment.
    Shell {
        #[command(flatten)]
        common: RemoteCommon,
        /// Role (default: the proxied role, else web).
        #[arg(long, value_name = "ROLE")]
        role: Option<String>,
        /// Host (default: the first host of the role).
        #[arg(long, value_name = "HOST")]
        host: Option<String>,
        /// Debug image with a shell, run as a sidecar.
        #[arg(long, value_name = "IMAGE")]
        image: Option<String>,
    },
}

const DEFAULT_POLL_INTERVAL_TEXT: &str = "500ms";
const DEFAULT_DEBOUNCE_TEXT: &str = "300ms";

#[derive(Debug, Subcommand)]
pub(crate) enum I18nCommand {
    /// Report the keys and plural forms each locale lacks (compared with the
    /// other locales and the literal keys of `t("KEY")` in templates and
    /// Rust code) and the keys nothing uses. Exits 1 when a locale lacks a
    /// key or a plural form. Reads files only.
    Check {
        /// Print a schema-versioned JSON report.
        #[arg(long)]
        json: bool,
    },
}

/// The environment flag every `forge credentials` command takes.
#[derive(Clone, Debug, Args)]
pub(crate) struct CredentialsEnvironment {
    /// Environment whose credentials to use (default: $FORGE_ENV or
    /// development).
    #[arg(short = 'e', long, value_name = "ENV")]
    pub(crate) environment: Option<String>,
}

#[derive(Debug, Subcommand)]
pub(crate) enum CredentialsCommand {
    /// Create a random key in config/credentials/ENV.key (mode 0600) and the
    /// encrypted ENV.json.enc, and keep *.key out of git and Docker. Never
    /// overwrites a key or an existing file and never prints the key.
    Init(CredentialsEnvironment),
    /// Decrypt into a private temporary file, open $VISUAL or $EDITOR (no
    /// shell), validate the JSON and re-encrypt atomically. The plaintext is
    /// wiped on every way out.
    Edit(CredentialsEnvironment),
    /// Print the document's structure with every value redacted.
    Show {
        #[command(flatten)]
        environment: CredentialsEnvironment,
        /// Print the values (interactive terminal only).
        #[arg(long)]
        reveal: bool,
    },
    /// Re-encrypt the file with a new random key.
    RotateKey {
        #[command(flatten)]
        environment: CredentialsEnvironment,
        /// Keep the previous keys readable in the new key list (migration
        /// window).
        #[arg(long)]
        keep_old: bool,
        /// Print the new key list to standard output.
        #[arg(long)]
        print_key: bool,
        /// Store the new keys in the environment's key file even when the key
        /// came from FORGE_MASTER_KEY.
        #[arg(long)]
        write_key_file: bool,
    },
}

/// What `forge migrate` shows instead of migrating.
#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(crate) enum MigrateView {
    /// List every migration as applied, pending, modified, disabled or missing.
    Status,
}

/// The guard flags of destructive commands.
#[derive(Clone, Copy, Debug, Args)]
pub(crate) struct DestructiveArgs {
    /// Allow this when FORGE_ENV is not development or test, or the database
    /// server is not on this machine, after confirmation.
    #[arg(long)]
    pub(crate) force_production: bool,
    /// Confirm without a prompt; needs FORGE_ALLOW_DESTRUCTIVE=true in the
    /// process environment.
    #[arg(long, requires = "force_production")]
    pub(crate) yes: bool,
}

#[derive(Debug, Subcommand)]
pub(crate) enum DbCommand {
    /// Create the database (the migration role needs CREATEDB on PostgreSQL).
    Create,
    /// Drop the database and everything in it. Refused in production and for
    /// servers that are not on this machine without --force-production.
    Drop(DestructiveArgs),
    /// Drop, then create, migrate and seed: a clean development database.
    Reset(DestructiveArgs),
    /// Create, migrate and seed.
    Setup,
    /// Set up the database when it is missing, otherwise migrate it; safe to
    /// repeat.
    Prepare,
    /// Apply pending migrations (the same as `forge migrate`).
    Migrate,
    /// Revert the latest application migration(s). Guarded like `drop`.
    Rollback {
        /// How many migrations to revert.
        #[arg(long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..=1000))]
        steps: Option<u32>,
        #[command(flatten)]
        destructive: DestructiveArgs,
    },
    /// List applied and pending migrations (the same as `forge migrate status`).
    Status,
    /// Load the seed data (the same as `forge seed`).
    Seed,
    /// Open the database's own client (psql, mysql or sqlite3) on
    /// FORGE_DATABASE_URL's database, with the credentials of
    /// FORGE_MIGRATION_DATABASE_URL when it is set. The password goes to the
    /// client through the environment or a private option file, never an
    /// argument. Arguments after -- go to the client.
    Console {
        /// Arguments for the client, after `--` (for example `-c 'select 1'`).
        #[arg(
            last = true,
            value_name = "CLIENT-ARGUMENTS",
            allow_hyphen_values = true
        )]
        arguments: Vec<String>,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum GenerateCommand {
    /// Add sign-in, sign-out and password-reset pages and the `setup`
    /// command to a full-stack PostgreSQL application (like `rails generate
    /// authentication`; `forge new` already includes them). Writes only new
    /// files and prints the wiring to add by hand.
    Authentication {
        /// List the files that would be created and the wiring, without
        /// writing anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Create a reversible SQL migration in migrations/.
    Migration {
        /// Human-readable migration name.
        name: String,
    },
    /// Create a model through the inner rings: migration, entity,
    /// repository port, use cases, repository adapter and its tests (like
    /// `rails generate model`). No HTTP layer; writes only new files and
    /// prints the wiring to add by hand.
    Model {
        #[command(flatten)]
        resource: ResourceArgs,
    },
    /// Create read-only GET pages (or JSON endpoints with --api), their
    /// routes, views and tests, without persistence (like `rails generate
    /// controller`). Routes ask the access policy for NAME:read unless
    /// --public.
    Controller {
        #[command(flatten)]
        controller: ControllerArgs,
    },
    /// Create a mailer: one typed method per email, askama subject, text and
    /// HTML templates, previews and tests (like `rails generate mailer`).
    Mailer {
        #[command(flatten)]
        mailer: MailerArgs,
    },
    /// Create a typed job for the persistent PostgreSQL queue (--queue): the
    /// payload, its handler, registration, enqueue function and tests (like
    /// `rails generate job`).
    Job {
        #[command(flatten)]
        job: JobArgs,
    },
    /// Create an HTML CRUD resource through every ring: migration, entity,
    /// port, use cases, repository, handlers, views and tests (like
    /// `rails generate scaffold`). Writes only new files and prints the
    /// wiring to add by hand.
    #[command(group(
        ArgGroup::new("scope")
            .required(true)
            .args(["tenant_scoped", "global"])
    ))]
    Scaffold {
        /// Singular resource name, snake_case or PascalCase (`line_item`).
        name: String,
        /// Fields as name:type, type one of string, text, integer, boolean,
        /// timestamp, uuid; append `?` for an optional field.
        #[arg(required = true, value_name = "FIELD:TYPE")]
        fields: Vec<String>,
        /// Rows belong to a tenant: routes under /tenants/{tenant_id}/,
        /// tenant predicates and (PostgreSQL) row-level security.
        #[arg(long)]
        tenant_scoped: bool,
        /// Rows are shared by every tenant; routes stay closed until the
        /// application chooses who may use them.
        #[arg(long)]
        global: bool,
        /// Plural name when the English rules get it wrong.
        #[arg(long, value_name = "PLURAL")]
        plural: Option<String>,
        /// Live updates: the pages publish every create, update and delete
        /// as Turbo Streams, and open index pages apply them over
        /// Server-Sent Events (needs FORGE_STREAMS_KEY in production).
        #[arg(long)]
        broadcast: bool,
        /// List the files that would be created and the wiring, without
        /// writing anything.
        #[arg(long)]
        dry_run: bool,
    },
}

/// The arguments of `forge generate model` (and `forge destroy model`).
#[derive(Debug, Args)]
pub(crate) struct ResourceArgs {
    /// Singular resource name, snake_case or PascalCase (`line_item`).
    pub(crate) name: String,
    /// Fields as name:type, type one of string, text, integer, boolean,
    /// timestamp, uuid; append `?` for an optional field.
    #[arg(required = true, value_name = "FIELD:TYPE")]
    pub(crate) fields: Vec<String>,
    /// Rows belong to a tenant: tenant predicates and (PostgreSQL)
    /// row-level security.
    #[arg(long, required_unless_present = "global", conflicts_with = "global")]
    pub(crate) tenant_scoped: bool,
    /// Rows are shared by every tenant.
    #[arg(long)]
    pub(crate) global: bool,
    /// Plural name when the English rules get it wrong.
    #[arg(long, value_name = "PLURAL")]
    pub(crate) plural: Option<String>,
    /// List the files without writing (or removing) anything.
    #[arg(long)]
    pub(crate) dry_run: bool,
}

/// The arguments of `forge generate controller` (and `forge destroy
/// controller`).
#[derive(Debug, Args)]
pub(crate) struct ControllerArgs {
    /// Controller name, snake_case and usually plural (`pages`).
    pub(crate) name: String,
    /// Read-only actions, each a GET route: `index` is /NAME, any other is
    /// /NAME/ACTION. create, update and destroy belong to the scaffold.
    #[arg(value_name = "ACTION")]
    pub(crate) actions: Vec<String>,
    /// JSON endpoints under /v1/NAME instead of HTML pages.
    #[arg(long)]
    pub(crate) api: bool,
    /// Mount the routes without a guard: anyone may call them.
    #[arg(long)]
    pub(crate) public: bool,
    /// List the files without writing (or removing) anything.
    #[arg(long)]
    pub(crate) dry_run: bool,
}

/// The arguments of `forge generate mailer` (and `forge destroy mailer`).
#[derive(Debug, Args)]
pub(crate) struct MailerArgs {
    /// Mailer name, snake_case (`user` becomes `UserMailer`).
    pub(crate) name: String,
    /// One email per METHOD, optionally followed by the string fields of its
    /// template data: `welcome:name,url`.
    #[arg(value_name = "METHOD[:FIELD,...]")]
    pub(crate) methods: Vec<String>,
    /// List the files without writing (or removing) anything.
    #[arg(long)]
    pub(crate) dry_run: bool,
}

/// The arguments of `forge generate job` (and `forge destroy job`).
#[derive(Debug, Args)]
pub(crate) struct JobArgs {
    /// Job name, snake_case (`send_welcome`).
    pub(crate) name: String,
    /// Payload fields as name:type (the model field types; `?` for
    /// optional).
    #[arg(value_name = "FIELD:TYPE")]
    pub(crate) fields: Vec<String>,
    /// A job of the persistent PostgreSQL queue (`forge::jobs`). Required:
    /// Forge-Rust schedules are queue jobs too (docs/jobs.md).
    #[arg(long)]
    pub(crate) queue: bool,
    /// List the files without writing (or removing) anything.
    #[arg(long)]
    pub(crate) dry_run: bool,
}

/// The flags `forge destroy` adds to the generator's arguments.
#[derive(Clone, Copy, Debug, Args)]
pub(crate) struct DestroyArgs {
    /// Remove files that differ from what the generator writes, or that a
    /// bigger generation still needs.
    #[arg(long)]
    pub(crate) force: bool,
    /// Remove the migrations too (revert them with `forge rollback` first).
    #[arg(long)]
    pub(crate) migrations: bool,
}

#[derive(Debug, Subcommand)]
pub(crate) enum DestroyCommand {
    /// Remove what `forge generate model` created.
    Model {
        #[command(flatten)]
        resource: ResourceArgs,
        #[command(flatten)]
        flags: DestroyArgs,
    },
    /// Remove what `forge generate scaffold` created (the shared access and
    /// streams modules stay).
    Scaffold {
        #[command(flatten)]
        resource: ResourceArgs,
        /// The scaffold was generated with --broadcast.
        #[arg(long)]
        broadcast: bool,
        #[command(flatten)]
        flags: DestroyArgs,
    },
    /// Remove what `forge generate controller` created.
    Controller {
        #[command(flatten)]
        controller: ControllerArgs,
        #[command(flatten)]
        flags: DestroyArgs,
    },
    /// Remove what `forge generate mailer` created.
    Mailer {
        #[command(flatten)]
        mailer: MailerArgs,
        #[command(flatten)]
        flags: DestroyArgs,
    },
    /// Remove what `forge generate job --queue` created.
    Job {
        #[command(flatten)]
        job: JobArgs,
        #[command(flatten)]
        flags: DestroyArgs,
    },
    /// Not supported: a migration that ran is part of the database's
    /// history (explains what to do instead).
    Migration {
        /// The migration name.
        name: String,
    },
}
