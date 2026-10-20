//! Command-line tooling for creating and maintaining Forge applications.

mod application;
mod authentication;
mod cargo;
mod cli;
mod dev;
mod doctor;
mod error;
mod generate;
mod lifecycle;
mod process;
mod project;
mod scaffold;

use std::env;
use std::io::{self, IsTerminal as _};

use uuid::Uuid;

pub use error::CliError;

use crate::application::{Application as _, CargoApplication};
use crate::cargo::CargoCommand;
use crate::cli::{Cli, Command, DbCommand, DestructiveArgs, GenerateCommand, MigrateView};
use crate::generate::{ForgeSource, NewApplication};
use crate::lifecycle::{Db, Destructive, Lifecycle};
use crate::process::Signals;
use crate::project::Project;
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
            locale,
        } => {
            let options = NewApplication {
                name,
                docker: !skip_docker,
                ci: !skip_ci,
                database,
                views: !api,
                forge: forge_path.map_or(ForgeSource::GitTag, ForgeSource::Path),
                locale,
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
                broadcast,
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
                    broadcast,
                    dry_run,
                };
                let version = generate::migration_version(current_dir)?;
                let outcome = scaffold::generate(current_dir, &options, &version)?;
                let verb = if dry_run { "would create" } else { "create" };
                for path in &outcome.created {
                    writeln!(output, "  {verb:<12} {path}").map_err(CliError::Output)?;
                }
                if outcome.reused_access {
                    writeln!(output, "  {:<12} src/adapters/http/access.rs", "reuse")
                        .map_err(CliError::Output)?;
                }
                if outcome.reused_streams {
                    writeln!(output, "  {:<12} src/adapters/http/streams.rs", "reuse")
                        .map_err(CliError::Output)?;
                }
                write!(output, "{}", outcome.wiring).map_err(CliError::Output)
            }
        },
        Command::Check => CargoCommand::Check.run(current_dir),
        Command::Test { args } => CargoCommand::Test(args).run(current_dir),
        Command::Format => CargoCommand::Format.run(current_dir),
        Command::Lint => CargoCommand::Lint.run(current_dir),
        Command::Build { name } => CargoCommand::Build(name).run(current_dir),
        Command::Version => {
            writeln!(output, "forge {}", env!("CARGO_PKG_VERSION")).map_err(CliError::Output)
        }
        Command::Doctor { json } => doctor::run(
            json,
            env::vars_os().collect(),
            current_dir,
            &doctor::System,
            output,
        ),
        Command::Dev {
            name,
            no_reload,
            poll_interval,
            debounce,
        } => run_dev(current_dir, name, no_reload, poll_interval, debounce),
        Command::Migrate { view } => lifecycle(current_dir, |app| {
            app.migrate(view == Some(MigrateView::Status))
        }),
        Command::Rollback { steps } => lifecycle(current_dir, |app| app.rollback(steps)),
        Command::Setup {
            tenant,
            email,
            organization,
            password_stdin,
        } => {
            let mut arguments = vec!["--tenant".to_owned(), tenant, "--email".to_owned(), email];
            if let Some(organization) = organization {
                arguments.extend(["--organization".to_owned(), organization]);
            }
            if password_stdin {
                arguments.push("--password-stdin".to_owned());
            }
            lifecycle(current_dir, |app| app.setup(&arguments))
        }
        Command::Seed => lifecycle(current_dir, |app| app.seed()),
        Command::Db { command } => {
            let command = match command {
                DbCommand::Create => Db::Create,
                DbCommand::Drop(flags) => Db::Drop(guard(flags)),
                DbCommand::Reset(flags) => Db::Reset(guard(flags)),
                DbCommand::Setup => Db::Setup,
                DbCommand::Prepare => Db::Prepare,
                DbCommand::Migrate => Db::Migrate,
                DbCommand::Rollback { steps, destructive } => Db::Rollback {
                    steps,
                    destructive: guard(destructive),
                },
                DbCommand::Status => Db::Status,
                DbCommand::Seed => Db::Seed,
            };
            lifecycle(current_dir, |app| app.db(&command))
        }
    }
}

fn guard(flags: DestructiveArgs) -> Destructive {
    Destructive {
        force_production: flags.force_production,
        yes: flags.yes,
    }
}

/// Runs a lifecycle command against the application in `current_dir`, with the
/// real terminal, environment and signal handling.
fn lifecycle(
    current_dir: &std::path::Path,
    command: impl FnOnce(&mut Lifecycle<'_, CargoApplication>) -> Result<(), CliError>,
) -> Result<(), CliError> {
    let project = Project::discover(current_dir)?;
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let mut messages = io::stderr();
    // A value that is not UTF-8 is read lossily, so it can never pass for an
    // unprotected FORGE_ENV.
    let environment =
        |key: &str| env::var_os(key).map(|value| value.to_string_lossy().into_owned());
    let mut lifecycle = Lifecycle {
        app: CargoApplication::new(project.clone()),
        name: &project.name,
        environment: &environment,
        input: &mut input,
        messages: &mut messages,
        interactive: io::stdin().is_terminal(),
    };
    command(&mut lifecycle)
}

/// `forge dev`.
fn run_dev(
    current_dir: &std::path::Path,
    name: Option<String>,
    no_reload: bool,
    poll_interval: std::time::Duration,
    debounce: std::time::Duration,
) -> Result<(), CliError> {
    let mut project = Project::discover(current_dir)?;
    if let Some(name) = name {
        if !valid_binary_name(&name) {
            return Err(CliError::Usage(format!("invalid binary name {name:?}")));
        }
        project.name = name;
    }
    if no_reload {
        let name = project.name.clone();
        let mut app = CargoApplication::new(project);
        return match app.run(&[])? {
            Some(0) => Ok(()),
            code => Err(CliError::AppCommandFailed {
                command: name,
                code,
            }),
        };
    }
    let signals = Signals::install()?;
    let log = |line: &str| {
        use std::io::Write as _;
        // Progress goes to stderr, next to the application's own logs.
        let _ = writeln!(io::stderr(), "forge dev: {line}");
    };
    log(&format!(
        "watching {}; the application restarts when files change (Ctrl-C stops)",
        project.root.display()
    ));
    let mut host = dev::CargoHost {
        root: project.root.clone(),
        bin: project.name.clone(),
        shutdown: signals.shutdown(),
    };
    dev::supervise(
        &mut host,
        || dev::scan(&project.root),
        poll_interval,
        debounce,
        &signals.shutdown(),
        log,
    )
}

/// A Cargo binary name: letters, digits, `-` and `_`, not starting with `-`.
fn valid_binary_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
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
