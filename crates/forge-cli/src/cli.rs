use clap::{Parser, Subcommand};

use clap::ArgGroup;

use crate::generate::Database;

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
    },
    /// Generate application artifacts.
    Generate {
        #[command(subcommand)]
        generator: GenerateCommand,
    },
    /// Type-check the current application.
    Check,
    /// Run the current application's tests.
    Test,
    /// Format the current application's Rust code.
    Format,
    /// Run Clippy with warnings treated as errors.
    Lint,
    /// Build the optimized release binary from the locked dependency graph.
    Build,
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
        /// List the files that would be created and the wiring, without
        /// writing anything.
        #[arg(long)]
        dry_run: bool,
    },
}
