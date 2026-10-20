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
    /// application's own subcommand; the CLI never reads the database URL.
    Db {
        #[command(subcommand)]
        command: DbCommand,
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
