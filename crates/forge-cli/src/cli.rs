use clap::{Parser, Subcommand};

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
    /// Create a new application with hexagonal boundaries, Docker and CI.
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
    /// Create a reversible SQL migration in migrations/.
    Migration {
        /// Human-readable migration name.
        name: String,
    },
}
