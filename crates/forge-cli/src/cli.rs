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
    /// Database lifecycle, like Rails' `db:*` tasks. Every command runs the
    /// application's own subcommand; the CLI never reads the database URL.
    Db {
        #[command(subcommand)]
        command: DbCommand,
    },
}

const DEFAULT_POLL_INTERVAL_TEXT: &str = "500ms";
const DEFAULT_DEBOUNCE_TEXT: &str = "300ms";

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
