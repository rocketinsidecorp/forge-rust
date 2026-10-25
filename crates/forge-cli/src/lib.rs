//! Command-line tooling for creating and maintaining Forge applications.

mod application;
mod authentication;
mod cargo;
mod cli;
mod controller;
mod credentials;
mod credentials_edit;
#[cfg(test)]
mod credentials_tests;
mod dbclient;
mod deploy;
mod destroy;
mod dev;
mod doctor;
mod error;
#[cfg(test)]
mod error_tests;
mod generate;
#[cfg(test)]
mod generator_tests;
mod generators;
mod i18n;
mod job;
mod lifecycle;
mod mailer;
mod process;
mod project;
mod scaffold;

use std::env;
use std::io::{self, IsTerminal as _};

use uuid::Uuid;

pub use error::CliError;

use crate::application::{Application as _, CargoApplication};
use crate::cargo::CargoCommand;
use crate::cli::{
    AppCommand, Cli, Command, ControllerArgs, CredentialsCommand, DbCommand, DeployArgs,
    DeployCommand, DestroyArgs, DestroyCommand, DestructiveArgs, GenerateCommand, I18nCommand,
    JobArgs, LockActionArg, LogsArgs, MailerArgs, MigrateView, RemoteCommon, ResourceArgs, Toggle,
};
use crate::controller::ControllerOptions;
use crate::destroy::{DestroyFlags, DestroyPlan};
use crate::generate::{ForgeSource, NewApplication};
use crate::job::JobOptions;
use crate::lifecycle::{Db, Destructive, Lifecycle};
use crate::mailer::MailerOptions;
use crate::process::Signals;
use crate::project::Project;
use crate::scaffold::{Layers, ScaffoldOptions, Scope};

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
                    layers: Layers::Scaffold,
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
            GenerateCommand::Model { resource } => {
                let options = resource_options(resource, Layers::Model, false);
                let version = generate::migration_version(current_dir)?;
                let outcome = scaffold::generate(current_dir, &options, &version)?;
                let verb = if options.dry_run {
                    "would create"
                } else {
                    "create"
                };
                for path in &outcome.created {
                    writeln!(output, "  {verb:<12} {path}").map_err(CliError::Output)?;
                }
                write!(output, "{}", outcome.wiring).map_err(CliError::Output)
            }
            GenerateCommand::Controller { controller } => {
                let options = controller_options(controller);
                let outcome = controller::generate(current_dir, &options)?;
                generators::report(output, options.dry_run, &outcome.files)?;
                if outcome.reused_access {
                    writeln!(output, "  {:<12} {}", "reuse", scaffold::ACCESS_PATH)
                        .map_err(CliError::Output)?;
                }
                write!(output, "{}", outcome.wiring).map_err(CliError::Output)
            }
            GenerateCommand::Mailer { mailer } => {
                let options = mailer_options(mailer);
                let outcome = mailer::generate(current_dir, &options)?;
                generators::report(output, options.dry_run, &outcome.files)?;
                write!(output, "{}", outcome.wiring).map_err(CliError::Output)
            }
            GenerateCommand::Job { job } => {
                let options = job_options(job);
                let outcome = job::generate(current_dir, &options)?;
                generators::report(output, options.dry_run, &outcome.files)?;
                write!(output, "{}", outcome.wiring).map_err(CliError::Output)
            }
        },
        Command::Destroy { generator } => run_destroy(current_dir, generator, output),
        Command::Check => CargoCommand::Check.run(current_dir),
        Command::Test { args } => CargoCommand::Test(args).run(current_dir),
        Command::Format => CargoCommand::Format.run(current_dir),
        Command::Lint => CargoCommand::Lint.run(current_dir),
        Command::Build { name } => CargoCommand::Build(name).run(current_dir),
        Command::Version => {
            writeln!(output, "forge {}", env!("CARGO_PKG_VERSION")).map_err(CliError::Output)
        }
        Command::I18n {
            command: I18nCommand::Check { json },
        } => i18n::check(current_dir, json, output),
        Command::Credentials { command } => run_credentials(current_dir, command, output),
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
        Command::Openapi { output, check } => {
            // The application runs in the project root: a relative FILE
            // means the one next to where forge was run.
            let mut arguments = Vec::new();
            for (flag, file) in [("--output", output), ("--check", check)] {
                if let Some(file) = file {
                    arguments.push(flag.to_owned());
                    arguments.push(current_dir.join(file).to_string_lossy().into_owned());
                }
            }
            lifecycle(current_dir, |app| app.openapi(&arguments))
        }
        Command::Deploy(args) => run_deploy(current_dir, args),
        Command::Logs(args) => run_logs(current_dir, args),
        Command::App { command } => run_app(current_dir, command),
        Command::Routes => lifecycle(current_dir, |app| app.routes()),
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
                DbCommand::Console { arguments } => Db::Console(arguments),
            };
            lifecycle(current_dir, |app| app.db(&command))
        }
    }
}

/// `forge credentials ...` against the process: its environment, stderr,
/// terminal and the user's editor.
fn run_credentials(
    root: &std::path::Path,
    command: CredentialsCommand,
    output: &mut dyn io::Write,
) -> Result<(), CliError> {
    let (action, environment) = match command {
        CredentialsCommand::Init(flags) => (credentials::Action::Init, flags.environment),
        CredentialsCommand::Edit(flags) => (credentials::Action::Edit, flags.environment),
        CredentialsCommand::Show {
            environment,
            reveal,
        } => (
            credentials::Action::Show { reveal },
            environment.environment,
        ),
        CredentialsCommand::RotateKey {
            environment,
            keep_old,
            print_key,
            write_key_file,
        } => (
            credentials::Action::RotateKey {
                keep_old,
                print_key,
                write_key_file,
            },
            environment.environment,
        ),
    };
    // Signals are caught for the whole edit, so its cleanup always runs.
    let interrupts = if action == credentials::Action::Edit {
        credentials_edit::Interrupts::install()?
    } else {
        credentials_edit::Interrupts::none()
    };
    let lookup = |name: &str| env::var(name).ok();
    let mut stderr = io::stderr();
    let mut context = credentials::Context {
        root: root.to_path_buf(),
        lookup: &lookup,
        stdout: output,
        stderr: &mut stderr,
        stdout_is_terminal: io::stdout().is_terminal(),
        editor: Box::new(credentials_edit::run_editor_process),
        interrupts: &interrupts,
        after_read: None,
        after_write: None,
    };
    context.run(&action, environment)
}

/// Installs the signal handlers deploy commands stop on.
fn deploy_signals() -> Result<(Signals, deploy::runner::Cancel), CliError> {
    let signals = Signals::install()?;
    let cancel = deploy::runner::Cancel::signal(signals.shutdown());
    Ok((signals, cancel))
}

/// `forge deploy [SUBCOMMAND]`.
fn run_deploy(current_dir: &std::path::Path, args: DeployArgs) -> Result<(), CliError> {
    use deploy::cli::{DeployAction, LockAction};

    let common = deploy::cli::Common {
        config: args.common.config,
        secrets: args.common.secrets,
        dry_run: args.common.dry_run,
        json: args.common.json,
    };
    if args.command.is_some() && (args.tag.is_some() || args.skip_build) {
        return Err(CliError::Usage(
            "--version and --skip-build apply to `forge deploy` without a subcommand".to_owned(),
        ));
    }
    let action = match args.command {
        None => DeployAction::Deploy {
            version: args.tag,
            skip_build: args.skip_build,
        },
        Some(DeployCommand::Init { service }) => DeployAction::Init { service },
        Some(DeployCommand::Setup) => DeployAction::Setup,
        Some(DeployCommand::Rollback { version }) => DeployAction::Rollback { version },
        Some(DeployCommand::Prune) => DeployAction::Prune,
        Some(DeployCommand::Details) => DeployAction::Details,
        Some(DeployCommand::Lock {
            action,
            message,
            force,
        }) => DeployAction::Lock {
            action: match action {
                LockActionArg::Status => LockAction::Status,
                LockActionArg::Acquire => LockAction::Acquire,
                LockActionArg::Release => LockAction::Release,
            },
            message,
            force,
        },
        Some(DeployCommand::Maintenance { state }) => DeployAction::Maintenance {
            on: state == Toggle::On,
        },
    };
    let (_signals, cancel) = deploy_signals()?;
    deploy::cli::run_deploy(
        action,
        &common,
        current_dir,
        &deploy::cli::Io::process(),
        &deploy::cli::Factory::system(),
        &cancel,
    )
    .map_err(CliError::from)
}

fn remote_common(common: RemoteCommon) -> deploy::cli::Common {
    deploy::cli::Common {
        config: common.config,
        secrets: common.secrets,
        dry_run: common.dry_run,
        json: false,
    }
}

/// `forge logs`.
fn run_logs(current_dir: &std::path::Path, args: LogsArgs) -> Result<(), CliError> {
    let options = deploy::remote::LogsOptions {
        role: args.role.unwrap_or_default(),
        host: args.host.unwrap_or_default(),
        follow: args.follow,
        since: args.since.unwrap_or_default(),
        lines: args.lines,
        grep: args.grep.unwrap_or_default(),
    };
    let (_signals, cancel) = deploy_signals()?;
    deploy::cli::run_logs(
        options,
        &remote_common(args.common),
        current_dir,
        &deploy::cli::Io::process(),
        &deploy::cli::Factory::system(),
        &cancel,
    )
    .map_err(CliError::from)
}

/// `forge app exec|shell`.
fn run_app(current_dir: &std::path::Path, command: AppCommand) -> Result<(), CliError> {
    use deploy::cli::AppAction;

    let (common, action) = match command {
        AppCommand::Exec {
            common,
            role,
            host,
            interactive,
            reuse,
            command,
        } => (
            common,
            AppAction::Exec(deploy::remote::ExecOptions {
                role: role.unwrap_or_default(),
                host: host.unwrap_or_default(),
                interactive,
                reuse,
                argv: command,
            }),
        ),
        AppCommand::Shell {
            common,
            role,
            host,
            image,
        } => (
            common,
            AppAction::Shell {
                role: role.unwrap_or_default(),
                host: host.unwrap_or_default(),
                image: image.unwrap_or_default(),
            },
        ),
    };
    let (_signals, cancel) = deploy_signals()?;
    deploy::cli::run_app(
        action,
        &remote_common(common),
        current_dir,
        &deploy::cli::Io::process(),
        &deploy::cli::Factory::system(),
        &cancel,
    )
    .map_err(CliError::from)
}

fn resource_options(resource: ResourceArgs, layers: Layers, broadcast: bool) -> ScaffoldOptions {
    ScaffoldOptions {
        name: resource.name,
        fields: resource.fields,
        scope: if resource.tenant_scoped {
            Scope::Tenant
        } else {
            Scope::Global
        },
        plural: resource.plural,
        broadcast,
        dry_run: resource.dry_run,
        layers,
    }
}

fn controller_options(arguments: ControllerArgs) -> ControllerOptions {
    ControllerOptions {
        name: arguments.name,
        actions: arguments.actions,
        api: arguments.api,
        public: arguments.public,
        dry_run: arguments.dry_run,
    }
}

fn mailer_options(arguments: MailerArgs) -> MailerOptions {
    MailerOptions {
        name: arguments.name,
        methods: arguments.methods,
        dry_run: arguments.dry_run,
    }
}

fn job_options(arguments: JobArgs) -> JobOptions {
    JobOptions {
        name: arguments.name,
        fields: arguments.fields,
        queue: arguments.queue,
        dry_run: arguments.dry_run,
    }
}

/// `forge destroy GENERATOR ARGUMENTS...`: plans what the generator would
/// write from the same arguments, then removes what is unchanged.
fn run_destroy(
    root: &std::path::Path,
    generator: DestroyCommand,
    output: &mut dyn io::Write,
) -> Result<(), CliError> {
    let destroy_flags = |dry_run: bool, flags: DestroyArgs| DestroyFlags {
        dry_run,
        force: flags.force,
        migrations: flags.migrations,
    };
    let (plan, flags) = match generator {
        DestroyCommand::Model { resource, flags } => {
            let options = resource_options(resource, Layers::Model, false);
            let (files, unwire, owned_elsewhere) = scaffold::plan_destroy(root, &options)?;
            (
                DestroyPlan {
                    files,
                    unwire,
                    owned_elsewhere,
                },
                destroy_flags(options.dry_run, flags),
            )
        }
        DestroyCommand::Scaffold {
            resource,
            broadcast,
            flags,
        } => {
            let options = resource_options(resource, Layers::Scaffold, broadcast);
            let (files, unwire, owned_elsewhere) = scaffold::plan_destroy(root, &options)?;
            (
                DestroyPlan {
                    files,
                    unwire,
                    owned_elsewhere,
                },
                destroy_flags(options.dry_run, flags),
            )
        }
        DestroyCommand::Controller { controller, flags } => {
            let options = controller_options(controller);
            let (files, unwire) = controller::plan_destroy(root, &options)?;
            (
                DestroyPlan {
                    files,
                    unwire,
                    owned_elsewhere: None,
                },
                destroy_flags(options.dry_run, flags),
            )
        }
        DestroyCommand::Mailer { mailer, flags } => {
            let options = mailer_options(mailer);
            let (files, unwire) = mailer::plan_destroy(root, &options)?;
            (
                DestroyPlan {
                    files,
                    unwire,
                    owned_elsewhere: None,
                },
                destroy_flags(options.dry_run, flags),
            )
        }
        DestroyCommand::Job { job, flags } => {
            let options = job_options(job);
            let (files, unwire) = job::plan_destroy(root, &options)?;
            (
                DestroyPlan {
                    files,
                    unwire,
                    owned_elsewhere: None,
                },
                destroy_flags(options.dry_run, flags),
            )
        }
        DestroyCommand::Migration { .. } => {
            return Err(CliError::Destroy(
                "forge destroy does not remove migrations: one that ran is part of the \
                 database's history. Revert it with `forge rollback` and delete \
                 migrations/<version>_NAME.{up,down}.sql yourself"
                    .to_owned(),
            ));
        }
    };
    destroy::apply(root, &plan, flags, output)
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
    let mut client = dbclient::System;
    let mut lifecycle = Lifecycle {
        app: CargoApplication::new(project.clone()),
        client: &mut client,
        root: &project.root,
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
