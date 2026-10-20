//! `forge deploy`, `forge logs` and `forge app`: argument handling, the
//! deployer wiring and the printed output (Forge-Go's formats).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use forge_proxy::GoDuration;
use serde::Serialize;

use super::config::Config;
use super::deployer::{Deployer, LogWriter, Parts, lock, resolve_version};
use super::errors::DeployError;
use super::operations::{Outcome, Report};
use super::remote::{ExecOptions, LogsOptions};
use super::runner::{Cancel, ExecLocal, LocalRunner, Runner, SshRunner};
use super::secrets::Secrets;

/// Where command output and progress go.
#[derive(Clone)]
pub(crate) struct Io {
    pub(crate) stdout: LogWriter,
    pub(crate) stderr: LogWriter,
}

impl Io {
    /// The process's stdout and stderr.
    pub(crate) fn process() -> Self {
        Self {
            stdout: Arc::new(Mutex::new(Box::new(std::io::stdout()))),
            stderr: Arc::new(Mutex::new(Box::new(std::io::stderr()))),
        }
    }

    fn print(&self, text: &str) {
        let mut out = lock(&self.stdout);
        let _ = out.write_all(text.as_bytes());
        let _ = out.flush();
    }
}

/// The flags every deploy command shares.
#[derive(Clone, Debug)]
pub(crate) struct Common {
    pub(crate) config: PathBuf,
    pub(crate) secrets: PathBuf,
    pub(crate) dry_run: bool,
    pub(crate) json: bool,
}

/// Makes the remote runner for a configuration.
type RunnerFactory = Box<dyn Fn(&Config) -> Box<dyn Runner>>;

/// How runners are made; tests substitute fakes.
pub(crate) struct Factory {
    pub(crate) runner: RunnerFactory,
    pub(crate) local: Box<dyn Fn() -> Box<dyn LocalRunner>>,
}

impl Factory {
    /// The system ssh client and local processes.
    pub(crate) fn system() -> Self {
        Self {
            runner: Box::new(|config: &Config| {
                Box::new(SshRunner::new(&config.ssh)) as Box<dyn Runner>
            }),
            local: Box::new(|| Box::new(ExecLocal) as Box<dyn LocalRunner>),
        }
    }
}

fn deployer(
    common: &Common,
    root: &Path,
    io: &Io,
    factory: &Factory,
) -> Result<Deployer, DeployError> {
    // Messages name the paths as given, like Forge-Go (relative to the
    // project), while the files are read under `root`.
    let shown = |error: String, full: &Path, given: &Path| {
        DeployError::new(error.replace(&full.display().to_string(), &given.display().to_string()))
    };
    let config_path = root.join(&common.config);
    let config =
        Config::load(&config_path).map_err(|error| shown(error, &config_path, &common.config))?;
    let secrets_path = root.join(&common.secrets);
    let secrets = Secrets::load(&secrets_path, &|name| std::env::var(name).ok())
        .map_err(|error| shown(error, &secrets_path, &common.secrets))?;
    // A dry-run plan is the command's output, unless stdout carries JSON.
    let log = if common.dry_run && !common.json {
        Arc::clone(&io.stdout)
    } else {
        Arc::clone(&io.stderr)
    };
    Ok(Deployer::new(Parts {
        runner: (factory.runner)(&config),
        local: (factory.local)(),
        config,
        secrets,
        root: root.to_owned(),
        log,
        performer: None,
        now: None,
        suffix: None,
        dry_run: common.dry_run,
    }))
}

impl Drop for Deployer {
    fn drop(&mut self) {
        self.runner.close();
    }
}

/// What `forge deploy` was asked to do.
#[derive(Clone, Debug)]
pub(crate) enum DeployAction {
    Deploy {
        version: Option<String>,
        skip_build: bool,
    },
    Init {
        service: Option<String>,
    },
    Setup,
    Rollback {
        version: String,
    },
    Prune,
    Details,
    Lock {
        action: LockAction,
        message: String,
        force: bool,
    },
    Maintenance {
        on: bool,
    },
}

/// `forge deploy lock` actions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LockAction {
    Status,
    Acquire,
    Release,
}

fn usage(text: &str) -> DeployError {
    DeployError::new(format!("usage: {text}"))
}

/// `forge deploy [SUBCOMMAND]`.
pub(crate) fn run_deploy(
    action: DeployAction,
    common: &Common,
    root: &Path,
    io: &Io,
    factory: &Factory,
    cancel: &Cancel,
) -> Result<(), DeployError> {
    match action {
        DeployAction::Init { service } => {
            if common.dry_run || common.json {
                return Err(usage("forge deploy init [--service NAME]"));
            }
            let service = service.unwrap_or_else(|| super::init::default_service_name(root));
            let (changed, result) = super::init::init(root, &service);
            for path in &changed {
                io.print(&format!("wrote {path}\n"));
            }
            result.map_err(DeployError::new)?;
            io.print(
                "next: edit config/deploy.yml and .forge/secrets, then run forge deploy setup\n",
            );
            Ok(())
        }
        DeployAction::Deploy {
            version,
            skip_build,
        } => {
            let deployer = deployer(common, root, io, factory)?;
            let resolved = resolve_version(
                cancel,
                deployer.local.as_ref(),
                root,
                version.as_deref().unwrap_or_default(),
            )?;
            let outcome = deployer.deploy(cancel, &resolved, skip_build);
            finish_report(common, io, outcome, &format!("deployed {resolved}"))
        }
        DeployAction::Setup => {
            let deployer = deployer(common, root, io, factory)?;
            finish_report(common, io, deployer.setup(cancel), "setup complete")
        }
        DeployAction::Prune => {
            let deployer = deployer(common, root, io, factory)?;
            finish_report(common, io, deployer.prune(cancel), "prune complete")
        }
        DeployAction::Rollback { version } => {
            let deployer = deployer(common, root, io, factory)?;
            let outcome = deployer.rollback(cancel, &version);
            finish_report(common, io, outcome, &format!("rolled back to {version}"))
        }
        DeployAction::Details => {
            if common.dry_run {
                return Err(usage("forge deploy details [--json]"));
            }
            let deployer = deployer(common, root, io, factory)?;
            details(&deployer, common, io, cancel)
        }
        DeployAction::Lock {
            action,
            message,
            force,
        } => {
            let deployer = deployer(common, root, io, factory)?;
            match action {
                LockAction::Status => {
                    let locks = deployer.lock_status(cancel)?;
                    if common.json {
                        let report = serde_json::json!({
                            "schema_version": 1,
                            "locked": !locks.is_empty(),
                            "locks": locks,
                        });
                        io.print(&format!("{report}\n"));
                        return Ok(());
                    }
                    if locks.is_empty() {
                        io.print("not locked\n");
                    }
                    for (host, info) in &locks {
                        io.print(&format!(
                            "{host}: held by {} since {} ({})\n",
                            info.holder,
                            info.acquired_at.rfc3339(),
                            info.message
                        ));
                    }
                    Ok(())
                }
                LockAction::Acquire => {
                    deployer.lock_acquire(cancel, &message)?;
                    io.print("lock acquired\n");
                    Ok(())
                }
                LockAction::Release => {
                    deployer.lock_release(cancel, force)?;
                    io.print("lock released\n");
                    Ok(())
                }
            }
        }
        DeployAction::Maintenance { on } => {
            if common.json {
                return Err(usage("forge deploy maintenance on|off [--dry-run]"));
            }
            let deployer = deployer(common, root, io, factory)?;
            deployer.maintenance(cancel, on)
        }
    }
}

#[derive(Serialize)]
struct JsonReport<'a> {
    #[serde(flatten)]
    report: &'a Report,
    ok: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    error: String,
    exit_code: u8,
}

/// Prints the report (always with `--json`, also on failure) and passes
/// the outcome through.
pub(crate) fn finish_report(
    common: &Common,
    io: &Io,
    (report, result): Outcome,
    summary: &str,
) -> Result<(), DeployError> {
    if common.json
        && let Some(report) = &report
    {
        let output = JsonReport {
            report,
            ok: result.is_ok(),
            error: result
                .as_ref()
                .err()
                .map(ToString::to_string)
                .unwrap_or_default(),
            exit_code: result.as_ref().err().map_or(0, DeployError::exit_code),
        };
        let encoded =
            serde_json::to_string(&output).map_err(|error| DeployError::new(error.to_string()))?;
        io.print(&format!("{encoded}\n"));
        return result;
    }
    result?;
    if let Some(report) = report {
        for warning in &report.warnings {
            io.print(&format!("warning: {warning}\n"));
        }
        let summary = if common.dry_run {
            "dry run: nothing was changed"
        } else {
            summary
        };
        io.print(&format!(
            "{summary} ({})\n",
            GoDuration::from_millis(report.duration_ms)
        ));
    }
    Ok(())
}

fn details(
    deployer: &Deployer,
    common: &Common,
    io: &Io,
    cancel: &Cancel,
) -> Result<(), DeployError> {
    let (hosts, result) = deployer.details(cancel);
    if common.json {
        let report = serde_json::json!({
            "schema_version": 1,
            "service": deployer.config.service,
            "hosts": hosts,
        });
        io.print(&format!("{report}\n"));
        return result;
    }
    let mut text = String::new();
    for host in &hosts {
        text.push_str(&format!("{} ({})\n", host.host, host.roles.join(", ")));
        if !host.error.is_empty() {
            text.push_str(&format!("  error: {}\n", host.error));
        }
        for container in &host.containers {
            text.push_str(&format!(
                "  {:<8} {:<10} {:<20} {}\n",
                container.role, container.state, container.version, container.name
            ));
        }
        for service in host.proxy.iter().flatten() {
            let hosts: Vec<&str> = service["hosts"]
                .as_array()
                .map(|list| list.iter().filter_map(|h| h.as_str()).collect())
                .unwrap_or_default();
            text.push_str(&format!(
                "  proxy    {} -> {} hosts={} tls={} maintenance={}\n",
                service["service"].as_str().unwrap_or_default(),
                service["target"].as_str().unwrap_or_default(),
                hosts.join(","),
                service["tls"].as_bool().unwrap_or(false),
                service["maintenance"].as_bool().unwrap_or(false)
            ));
        }
        if let Some(lock) = &host.lock {
            text.push_str(&format!(
                "  lock     held by {} since {} ({})\n",
                lock.holder,
                lock.acquired_at.rfc3339(),
                lock.message
            ));
        }
        for entry in &host.recent_deploys {
            text.push_str(&format!("  history  {entry}\n"));
        }
    }
    io.print(&text);
    result
}

/// `forge logs`.
pub(crate) fn run_logs(
    options: LogsOptions,
    common: &Common,
    root: &Path,
    io: &Io,
    factory: &Factory,
    cancel: &Cancel,
) -> Result<(), DeployError> {
    let deployer = deployer(common, root, io, factory)?;
    deployer.logs(cancel, Arc::clone(&io.stdout), options)
}

/// What `forge app` was asked to do.
#[derive(Clone, Debug)]
pub(crate) enum AppAction {
    Exec(ExecOptions),
    Shell {
        role: String,
        host: String,
        image: String,
    },
}

/// `forge app exec|shell`.
pub(crate) fn run_app(
    action: AppAction,
    common: &Common,
    root: &Path,
    io: &Io,
    factory: &Factory,
    cancel: &Cancel,
) -> Result<(), DeployError> {
    let deployer = deployer(common, root, io, factory)?;
    match action {
        AppAction::Exec(options) => deployer.exec(cancel, Arc::clone(&io.stdout), &options),
        AppAction::Shell { role, host, image } => deployer.shell(cancel, &role, &host, &image),
    }
}
