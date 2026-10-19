//! Command-line tooling for creating and maintaining Forge applications.

mod authentication;
mod cargo;
mod cli;
mod error;
mod generate;
mod scaffold;

use std::env;
use std::io;

use uuid::Uuid;

pub use error::CliError;

use crate::cargo::CargoCommand;
use crate::cli::{Cli, Command, GenerateCommand};
use crate::generate::{ForgeSource, NewApplication};
use crate::scaffold::{ScaffoldOptions, Scope};

/// Parses process arguments and executes the selected Forge command.
pub fn run() -> Result<(), CliError> {
    let cli = Cli::parse_process();
    let current_dir = env::current_dir().map_err(CliError::CurrentDirectory)?;
    execute(cli.command, &current_dir, &mut io::stdout())
}

fn execute(
    command: Command,
    current_dir: &std::path::Path,
    output: &mut dyn io::Write,
) -> Result<(), CliError> {
    match command {
        Command::Id => writeln!(output, "{}", Uuid::now_v7()).map_err(CliError::Output),
        Command::New {
            name,
            skip_docker,
            skip_ci,
            database,
            api,
            skip_lockfile,
            forge_path,
        } => {
            let options = NewApplication {
                name,
                docker: !skip_docker,
                ci: !skip_ci,
                database,
                views: !api,
                forge: forge_path.map_or(ForgeSource::GitTag, ForgeSource::Path),
            };
            let destination = generate::create_application(current_dir, &options)?;
            writeln!(
                output,
                "Created Forge application `{}` at {}",
                options.name,
                destination.display()
            )
            .map_err(CliError::Output)?;
            if let Some(notice) = options.database.unavailable_notice() {
                writeln!(output, "{notice}").map_err(CliError::Output)?;
            }

            if !skip_lockfile && let Err(error) = CargoCommand::GenerateLockfile.run(&destination) {
                writeln!(
                    output,
                    "warning: {error}\n\
                         warning: the application was created, but `docker build` and \
                         `--locked` builds need Cargo.lock; run `cargo generate-lockfile` \
                         in {} once the `forge` dependency can be resolved",
                    destination.display()
                )
                .map_err(CliError::Output)?;
            }
            Ok(())
        }
        Command::Generate { generator } => match generator {
            GenerateCommand::Authentication { dry_run } => {
                let outcome = authentication::generate(current_dir, dry_run)?;
                let verb = if dry_run { "would create" } else { "create" };
                for path in &outcome.created {
                    writeln!(output, "  {verb:<12} {path}").map_err(CliError::Output)?;
                }
                write!(output, "{}", outcome.wiring).map_err(CliError::Output)
            }
            GenerateCommand::Migration { name } => {
                let (up, down) = generate::create_migration(current_dir, &name)?;
                writeln!(
                    output,
                    "Created migration files:\n  {}\n  {}",
                    up.display(),
                    down.display()
                )
                .map_err(CliError::Output)
            }
            GenerateCommand::Scaffold {
                name,
                fields,
                tenant_scoped,
                global: _,
                plural,
                dry_run,
            } => {
                let options = ScaffoldOptions {
                    name,
                    fields,
                    scope: if tenant_scoped {
                        Scope::Tenant
                    } else {
                        Scope::Global
                    },
                    plural,
                    dry_run,
                };
                let version = generate::migration_version()?;
                let outcome = scaffold::generate(current_dir, &options, &version)?;
                let verb = if dry_run { "would create" } else { "create" };
                for path in &outcome.created {
                    writeln!(output, "  {verb:<12} {path}").map_err(CliError::Output)?;
                }
                if outcome.reused_access {
                    writeln!(output, "  {:<12} src/adapters/http/access.rs", "reuse")
                        .map_err(CliError::Output)?;
                }
                write!(output, "{}", outcome.wiring).map_err(CliError::Output)
            }
        },
        Command::Check => CargoCommand::Check.run(current_dir),
        Command::Test => CargoCommand::Test.run(current_dir),
        Command::Format => CargoCommand::Format.run(current_dir),
        Command::Lint => CargoCommand::Lint.run(current_dir),
        Command::Build => CargoCommand::Build.run(current_dir),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_command_emits_a_version_seven_uuid() {
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let mut output = Vec::new();

        execute(Command::Id, directory.path(), &mut output).expect("id generation should succeed");

        let rendered = String::from_utf8(output).expect("UUID output should be UTF-8");
        let id = Uuid::parse_str(rendered.trim()).expect("output should be a UUID");
        assert_eq!(id.get_version_num(), 7);
    }
}
