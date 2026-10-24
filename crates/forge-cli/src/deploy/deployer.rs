//! The orchestration shared by every operation: progress logging with
//! secrets redacted, bounded parallelism over hosts, the deploy lock,
//! hooks, the local build and the container inventory.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use forge_proxy::clock::Timestamp;
use serde::Serialize;

use super::commands::{
    Commands, LABEL_ROLE, LABEL_SERVICE, LABEL_VERSION, RemoteCommand, STALE_LOCK_NOTICE,
};
use super::config::{Config, VERSION};
use super::errors::{
    DeployError, EXIT_HOOK_FAILED, EXIT_LOCK_HELD, LOCK_GONE_EXIT_STATUS, LOCK_HELD_EXIT_STATUS,
    LOCK_NOT_OURS_EXIT_STATUS, LockInfo,
};
use super::runner::{Cancel, DryLocal, DryRunner, LocalRunner, Output, RunError, Runner, Sink};
use super::secrets::{Redactor, Secrets};

/// Where progress lines go.
pub(crate) type LogWriter = Arc<Mutex<Box<dyn Write + Send>>>;

/// What a [`Deployer`] is built from.
pub(crate) struct Parts {
    pub(crate) config: Config,
    pub(crate) secrets: Secrets,
    pub(crate) runner: Box<dyn Runner>,
    pub(crate) local: Box<dyn LocalRunner>,
    /// The project directory (hooks, build context).
    pub(crate) root: PathBuf,
    pub(crate) log: LogWriter,
    /// Who deploys (`user@machine`); defaults to the local user.
    pub(crate) performer: Option<String>,
    pub(crate) now: Option<Box<dyn Fn() -> Timestamp + Send + Sync>>,
    /// Makes container names unique; tests make it deterministic.
    pub(crate) suffix: Option<Box<dyn Fn() -> String + Send + Sync>>,
    /// Print mutating commands instead of running them; read-only queries
    /// still run so the plan names real containers.
    pub(crate) dry_run: bool,
}

/// Runs deploy operations for one configuration.
pub(crate) struct Deployer {
    pub(crate) config: Config,
    pub(crate) secrets: Secrets,
    pub(crate) runner: Box<dyn Runner>,
    pub(crate) local: Box<dyn LocalRunner>,
    pub(crate) root: PathBuf,
    pub(crate) log: LogWriter,
    pub(crate) performer: String,
    now: Box<dyn Fn() -> Timestamp + Send + Sync>,
    suffix: Box<dyn Fn() -> String + Send + Sync>,
    pub(crate) dry_run: bool,
    pub(crate) cmds: Commands,
    pub(crate) redactor: Redactor,
}

/// Three random bytes in hex.
fn random_suffix() -> String {
    let mut bytes = [0_u8; 3];
    if getrandom::fill(&mut bytes).is_err() {
        // A clock-derived fallback still keeps names unique enough to retry.
        let nanos = Timestamp::now().nanos;
        bytes = [(nanos >> 16) as u8, (nanos >> 8) as u8, nanos as u8];
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `user@hostname` of the local user.
pub(crate) fn performer() -> String {
    let name = ["USER", "LOGNAME", "USERNAME"]
        .iter()
        .find_map(|key| std::env::var(key).ok().filter(|value| !value.is_empty()))
        .unwrap_or_else(|| "unknown".to_owned());
    let host = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .or_else(|_| std::fs::read_to_string("/etc/hostname"))
        .map(|text| text.trim().to_owned())
        .ok()
        .filter(|text| !text.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "unknown".to_owned());
    format!("{name}@{host}")
}

impl Deployer {
    pub(crate) fn new(parts: Parts) -> Self {
        let redactor = parts.secrets.redactor();
        let (runner, local): (Box<dyn Runner>, Box<dyn LocalRunner>) = if parts.dry_run {
            (
                Box::new(DryRunner {
                    inner: Some(parts.runner),
                    out: Arc::clone(&parts.log),
                    redactor: redactor.clone(),
                }),
                Box::new(DryLocal {
                    inner: parts.local,
                    out: Arc::clone(&parts.log),
                    redactor: redactor.clone(),
                }),
            )
        } else {
            (parts.runner, parts.local)
        };
        Self {
            cmds: Commands::new(parts.config.clone()),
            config: parts.config,
            secrets: parts.secrets,
            runner,
            local,
            root: parts.root,
            log: parts.log,
            performer: parts.performer.unwrap_or_else(performer),
            now: parts.now.unwrap_or_else(|| Box::new(Timestamp::now)),
            suffix: parts.suffix.unwrap_or_else(|| Box::new(random_suffix)),
            dry_run: parts.dry_run,
            redactor,
        }
    }

    /// Checks that every secret the configuration names has a value.
    pub(crate) fn prepare(&self) -> Result<(), DeployError> {
        self.secrets
            .require(&self.config.secret_names())
            .map_err(DeployError::new)
    }

    pub(crate) fn now(&self) -> Timestamp {
        (self.now)()
    }

    pub(crate) fn suffix(&self) -> String {
        (self.suffix)()
    }

    /// One progress line, secrets redacted (`#` marks it in a dry run).
    pub(crate) fn logf(&self, host: &str, line: &str) {
        let line = self.redactor.redact(line);
        let mut out = self
            .log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.dry_run {
            let _ = writeln!(out, "{host:<15} # {line}");
        } else {
            let _ = writeln!(out, "{host:<15} {line}");
        }
        let _ = out.flush();
    }

    /// Remote stderr and ssh diagnostics end up in `error: …` output, so
    /// every error from the runner is redacted.
    pub(crate) fn redact_error(&self, error: RunError) -> DeployError {
        match error {
            RunError::Remote(mut remote) => {
                remote.stderr = self.redactor.redact(&remote.stderr);
                DeployError::from(remote)
            }
            RunError::Other(message) => DeployError::new(self.redactor.redact(&message)),
        }
    }

    pub(crate) fn run(
        &self,
        cancel: &Cancel,
        host: &str,
        command: &RemoteCommand,
    ) -> Result<Output, DeployError> {
        self.runner
            .run(cancel, host, command)
            .map_err(|error| self.redact_error(error))
    }

    pub(crate) fn stream(
        &self,
        cancel: &Cancel,
        host: &str,
        command: &RemoteCommand,
        out: &dyn Sink,
    ) -> Result<(), DeployError> {
        self.runner
            .stream(cancel, host, command, out)
            .map_err(|error| self.redact_error(error))
    }

    pub(crate) fn interactive(
        &self,
        cancel: &Cancel,
        host: &str,
        script: &str,
    ) -> Result<(), DeployError> {
        self.runner
            .interactive(cancel, host, script)
            .map_err(|error| self.redact_error(error))
    }

    /// Runs `work` for every host with at most `parallelism` at once
    /// (sequentially in a dry run, so the plan prints in a stable order)
    /// and joins the errors in host order. The worker threads are scoped to
    /// this call.
    pub(crate) fn each_host<F>(
        &self,
        cancel: &Cancel,
        hosts: &[String],
        work: F,
    ) -> Result<(), DeployError>
    where
        F: Fn(&Cancel, &str) -> Result<(), DeployError> + Sync,
    {
        let limit = usize::try_from(self.config.parallelism).unwrap_or(1);
        if self.dry_run || limit < 2 || hosts.len() < 2 {
            let errors: Vec<DeployError> = hosts
                .iter()
                .filter_map(|host| work(cancel, host).err())
                .collect();
            return DeployError::join(errors).map_or(Ok(()), Err);
        }
        let results: Vec<Mutex<Option<DeployError>>> =
            hosts.iter().map(|_| Mutex::new(None)).collect();
        let next = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..limit.min(hosts.len()) {
                scope.spawn(|| {
                    loop {
                        let index = next.fetch_add(1, Ordering::SeqCst);
                        let Some(host) = hosts.get(index) else { break };
                        if let Err(error) = work(cancel, host) {
                            *results[index]
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(error);
                        }
                    }
                });
            }
        });
        let errors: Vec<DeployError> = results
            .into_iter()
            .filter_map(|slot| {
                slot.into_inner()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
            })
            .collect();
        DeployError::join(errors).map_or(Ok(()), Err)
    }

    /// Takes the deploy lock on every host. The returned guard releases it
    /// on drop, also after a failure or a signal, but only where the lock
    /// still carries this acquire's record.
    pub(crate) fn acquire(
        &self,
        cancel: &Cancel,
        hosts: &[String],
        message: &str,
    ) -> Result<LockGuard<'_>, DeployError> {
        let info = LockInfo {
            holder: self.performer.clone(),
            message: message.to_owned(),
            acquired_at: self.now().utc(),
        };
        let mut record =
            serde_json::to_vec(&info).map_err(|error| DeployError::new(error.to_string()))?;
        record.push(b'\n');
        let acquired = Mutex::new(Vec::new());
        let held = Mutex::new(BTreeMap::new());
        let command = self.cmds.acquire_lock(record.clone());
        let result = self.each_host(cancel, hosts, |cancel, host| {
            match self.runner.run(cancel, host, &command) {
                Ok(output) => {
                    lock(&acquired).push(host.to_owned());
                    if output.stderr.contains(STALE_LOCK_NOTICE) {
                        self.logf("warning", &format!("{host}: {STALE_LOCK_NOTICE}"));
                    }
                    Ok(())
                }
                Err(RunError::Remote(remote)) if remote.exit_code == LOCK_HELD_EXIT_STATUS => {
                    lock(&held).insert(host.to_owned(), parse_lock_info(&remote.stdout));
                    Ok(())
                }
                Err(error) => Err(self.redact_error(error)),
            }
        });
        let guard = LockGuard {
            deployer: self,
            hosts: acquired
                .into_inner()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            record,
        };
        let held = held
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !held.is_empty() {
            drop(guard);
            return Err(DeployError::lock_held(&held));
        }
        if let Err(error) = result {
            drop(guard);
            return Err(error);
        }
        for host in hosts {
            self.logf(host, &format!("lock acquired ({message})"));
        }
        Ok(guard)
    }

    /// Takes the deploy lock on every host until `forge deploy lock release`.
    pub(crate) fn lock_acquire(&self, cancel: &Cancel, message: &str) -> Result<(), DeployError> {
        self.prepare()?;
        self.acquire(cancel, &self.config.all_hosts(), message)?
            .keep();
        Ok(())
    }

    /// The raw record per host; hosts without a lock are absent, and a lock
    /// without a record is `{}`.
    fn lock_records(&self, cancel: &Cancel) -> Result<BTreeMap<String, String>, DeployError> {
        let records = Mutex::new(BTreeMap::new());
        let command = self.cmds.lock_status();
        self.each_host(cancel, &self.config.all_hosts(), |cancel, host| {
            let output = self.run(cancel, host, &command)?;
            let record = output.stdout.trim();
            if !record.is_empty() {
                lock(&records).insert(host.to_owned(), record.to_owned());
            }
            Ok(())
        })?;
        Ok(records
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner))
    }

    /// The lock record per host; hosts without a lock are absent. A lock
    /// left without a record reports holder `unknown`.
    pub(crate) fn lock_status(
        &self,
        cancel: &Cancel,
    ) -> Result<BTreeMap<String, LockInfo>, DeployError> {
        self.prepare()?;
        Ok(self
            .lock_records(cancel)?
            .into_iter()
            .map(|(host, record)| (host, parse_lock_info(&record)))
            .collect())
    }

    /// Removes the deploy lock. Without `force` it removes only locks held
    /// by this performer, and only while the record is unchanged; it refuses
    /// one held by someone else (or without a record).
    pub(crate) fn lock_release(&self, cancel: &Cancel, force: bool) -> Result<(), DeployError> {
        self.prepare()?;
        if force {
            let command = self.cmds.force_release_lock();
            return self.each_host(cancel, &self.config.all_hosts(), |cancel, host| {
                self.run(cancel, host, &command)?;
                self.logf(host, "lock released");
                Ok(())
            });
        }
        let records = self.lock_records(cancel)?;
        for (host, record) in &records {
            let info = parse_lock_info(record);
            if record == "{}" || info.holder != self.performer {
                return Err(DeployError::new(format!(
                    "{host}: lock is held by {} ({}); pass --force to release it anyway",
                    info.holder, info.message
                ))
                .with_code(EXIT_LOCK_HELD));
            }
        }
        let hosts: Vec<String> = records.keys().cloned().collect();
        self.each_host(cancel, &hosts, |cancel, host| {
            let record = records.get(host).cloned().unwrap_or_default() + "\n";
            match self.run(cancel, host, &self.cmds.release_lock(record.into_bytes())) {
                Ok(_) => {
                    self.logf(host, "lock released");
                    Ok(())
                }
                Err(error) if error.remote_exit == Some(LOCK_GONE_EXIT_STATUS) => {
                    self.logf(host, "lock was already released");
                    Ok(())
                }
                Err(error) if error.remote_exit == Some(LOCK_NOT_OURS_EXIT_STATUS) => {
                    Err(DeployError::new(format!(
                        "{host}: the lock changed while releasing it; left in place (pass --force to release it anyway)"
                    ))
                    .with_code(EXIT_LOCK_HELD))
                }
                Err(error) => Err(error),
            }
        })
    }

    /// Runs `.forge/hooks/NAME` when it exists. A non-zero exit aborts.
    pub(crate) fn hook(
        &self,
        cancel: &Cancel,
        name: &str,
        env: &BTreeMap<String, String>,
    ) -> Result<(), DeployError> {
        let path = self.root.join(&self.config.hooks).join(name);
        let metadata = match std::fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(
                    DeployError::new(format!("hook {name}: {error}")).with_code(EXIT_HOOK_FAILED)
                );
            }
        };
        #[cfg(unix)]
        let executable = {
            use std::os::unix::fs::PermissionsExt as _;
            metadata.permissions().mode() & 0o100 != 0
        };
        #[cfg(not(unix))]
        let executable = true;
        if metadata.is_dir() || !executable {
            return Err(DeployError::new(format!(
                "hook {name} exists but is not executable (chmod +x {})",
                path.display()
            ))
            .with_code(EXIT_HOOK_FAILED));
        }
        let vars: Vec<(String, String)> = env.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        self.logf("local", &format!("hook {name}"));
        self.local
            .run(
                cancel,
                &self.root,
                &[path.display().to_string()],
                None,
                &vars,
            )
            .map_err(|error| {
                DeployError::new(format!(
                    "hook {name} failed: {}",
                    self.redactor.redact(&error)
                ))
                .with_code(EXIT_HOOK_FAILED)
            })
    }

    pub(crate) fn hook_env(
        &self,
        command: &str,
        version: &str,
        started: Option<Timestamp>,
    ) -> BTreeMap<String, String> {
        let mut env = BTreeMap::from([
            ("FORGE_COMMAND".to_owned(), command.to_owned()),
            ("FORGE_SERVICE".to_owned(), self.config.service.clone()),
            ("FORGE_VERSION".to_owned(), version.to_owned()),
            ("FORGE_IMAGE".to_owned(), self.cmds.image_ref(version)),
            ("FORGE_HOSTS".to_owned(), self.config.app_hosts().join(",")),
            ("FORGE_ROLES".to_owned(), self.config.roles().join(",")),
            ("FORGE_PERFORMER".to_owned(), self.performer.clone()),
        ]);
        if let Some(started) = started {
            env.insert(
                "FORGE_RUNTIME".to_owned(),
                (self.now().millis_since(started) / 1000).to_string(),
            );
        }
        env
    }

    /// Logs in locally, builds and pushes `IMAGE:version`.
    pub(crate) fn build(&self, cancel: &Cancel, version: &str) -> Result<(), DeployError> {
        let config = &self.config;
        if !config.registry.username.is_empty() {
            let password = self
                .secrets
                .get(&config.registry.password)
                .unwrap_or_default();
            self.local
                .run(
                    cancel,
                    &self.root,
                    &self.cmds.login_argv(),
                    Some(password.as_bytes()),
                    &[],
                )
                .map_err(|error| {
                    DeployError::new(format!("registry login: {}", self.redactor.redact(&error)))
                })?;
        }
        let image = self.cmds.image_ref(version);
        let mut argv = vec![
            "docker".to_owned(),
            "build".to_owned(),
            "--tag".to_owned(),
            image.clone(),
            "--file".to_owned(),
            config.builder.dockerfile.clone(),
            "--label".to_owned(),
            format!("{LABEL_SERVICE}={}", config.service),
            "--label".to_owned(),
            format!("{LABEL_VERSION}={version}"),
        ];
        if !config.builder.platform.is_empty() {
            argv.extend(["--platform".to_owned(), config.builder.platform.clone()]);
        }
        for (key, value) in &config.builder.args {
            argv.extend(["--build-arg".to_owned(), format!("{key}={value}")]);
        }
        argv.push(config.builder.context.clone());
        self.logf("local", &format!("building {image}"));
        self.local
            .run(cancel, &self.root, &argv, None, &[])
            .map_err(|error| {
                DeployError::new(format!("build: {}", self.redactor.redact(&error)))
            })?;
        self.local
            .run(
                cancel,
                &self.root,
                &["docker".to_owned(), "push".to_owned(), image],
                None,
                &[],
            )
            .map_err(|error| DeployError::new(format!("push: {}", self.redactor.redact(&error))))
    }

    pub(crate) fn inventory(
        &self,
        cancel: &Cancel,
        host: &str,
    ) -> Result<Vec<Container>, DeployError> {
        let output = self.run(cancel, host, &self.cmds.inventory())?;
        parse_inventory(&output.stdout)
    }

    pub(crate) fn inventories(
        &self,
        cancel: &Cancel,
        hosts: &[String],
    ) -> Result<BTreeMap<String, Vec<Container>>, DeployError> {
        let all = Mutex::new(BTreeMap::new());
        let result = self.each_host(cancel, hosts, |cancel, host| {
            let containers = self.inventory(cancel, host);
            let (containers, outcome) = match containers {
                Ok(containers) => (containers, Ok(())),
                Err(error) => (Vec::new(), Err(error)),
            };
            lock(&all).insert(host.to_owned(), containers);
            outcome
        });
        let all = all
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        result.map(|()| all)
    }
}

/// Releases the deploy lock on the hosts that took it, when dropped, where
/// the lock still carries `record`.
pub(crate) struct LockGuard<'a> {
    deployer: &'a Deployer,
    hosts: Vec<String>,
    record: Vec<u8>,
}

impl Drop for LockGuard<'_> {
    fn drop(&mut self) {
        if self.hosts.is_empty() {
            return;
        }
        let deployer = self.deployer;
        let command = deployer.cmds.release_lock(self.record.clone());
        // Released even after a signal, within 30 seconds.
        let cancel = Cancel::detached(Duration::from_secs(30));
        if let Err(error) = deployer.each_host(&cancel, &self.hosts, |cancel, host| {
            match deployer.run(cancel, host, &command) {
                Ok(_) => Ok(()),
                Err(error) if error.remote_exit == Some(LOCK_GONE_EXIT_STATUS) => {
                    deployer.logf(
                        "warning",
                        &format!("{host}: the deploy lock was already released (by --force?)"),
                    );
                    Ok(())
                }
                Err(error) if error.remote_exit == Some(LOCK_NOT_OURS_EXIT_STATUS) => {
                    // Somebody force-released and took the lock: not ours to remove.
                    deployer.logf(
                        "warning",
                        &format!(
                            "{host}: the deploy lock is now held by someone else and was left in place"
                        ),
                    );
                    Ok(())
                }
                Err(error) => Err(error),
            }
        }) {
            deployer.logf(
                "warning",
                &format!("releasing the deploy lock failed: {error}"),
            );
        }
    }
}

impl LockGuard<'_> {
    /// Keeps the lock after the command ends (`forge deploy lock acquire`).
    pub(crate) fn keep(mut self) {
        self.hosts.clear();
    }
}

/// Reads the stdout of the lock commands. An empty record or `{}` is a lock
/// whose acquire was interrupted before it wrote the record.
pub(crate) fn parse_lock_info(stdout: &str) -> LockInfo {
    let text = stdout.trim();
    if text.is_empty() || text == "{}" {
        return LockInfo {
            holder: "unknown".to_owned(),
            message: "lock without a record: an acquire was interrupted; a deploy takes it over once it is a minute old if its info file is missing, otherwise use forge deploy lock release --force".to_owned(),
            acquired_at: Timestamp::go_zero(),
        };
    }
    serde_json::from_str::<LockInfo>(text).unwrap_or_else(|_| LockInfo {
        holder: "unknown".to_owned(),
        message: "unreadable lock record".to_owned(),
        acquired_at: Timestamp::go_zero(),
    })
}

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Returns the explicit version when set, else the abbreviated commit of
/// HEAD. A working tree with uncommitted changes needs an explicit
/// version: the image would not match any commit.
pub(crate) fn resolve_version(
    cancel: &Cancel,
    local: &dyn LocalRunner,
    root: &Path,
    explicit: &str,
) -> Result<String, DeployError> {
    if !explicit.is_empty() {
        if !VERSION.matches(explicit) {
            return Err(DeployError::new(format!(
                "version {explicit:?} must match {}",
                VERSION.source()
            )));
        }
        return Ok(explicit.to_owned());
    }
    let argv = |args: &[&str]| args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
    let head = local
        .output(
            cancel,
            root,
            &argv(&["git", "rev-parse", "--short=12", "--verify", "HEAD"]),
        )
        .map_err(|error| {
            DeployError::new(format!(
                "determine version from git (or pass --version): {error}"
            ))
        })?;
    let status = local
        .output(
            cancel,
            root,
            &argv(&["git", "status", "--porcelain", "--untracked-files=no"]),
        )
        .map_err(|error| DeployError::new(format!("check working tree: {error}")))?;
    if !status.trim().is_empty() {
        return Err(DeployError::new(
            "the working tree has uncommitted changes; commit them or pass --version",
        ));
    }
    Ok(head.trim().to_owned())
}

/// One application container found on a host.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct Container {
    pub(crate) name: String,
    pub(crate) role: String,
    pub(crate) version: String,
    pub(crate) state: String,
    pub(crate) image: String,
    #[serde(with = "super::errors::rfc3339")]
    pub(crate) created: Timestamp,
}

impl Container {
    /// Running or restarting.
    pub(crate) fn running(&self) -> bool {
        self.state == "running" || self.state == "restarting"
    }
}

/// Reads `docker ps --format '{{json .}}'` output, newest first.
pub(crate) fn parse_inventory(output: &str) -> Result<Vec<Container>, DeployError> {
    let mut containers = Vec::new();
    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let value: serde_json::Value = serde_json::from_str(line)
            .map_err(|error| DeployError::new(format!("unexpected docker ps output: {error}")))?;
        if !value.is_object() {
            return Err(DeployError::new(
                "unexpected docker ps output: not an object",
            ));
        }
        let field = |name: &str| value[name].as_str().unwrap_or_default().to_owned();
        let labels: BTreeMap<String, String> = field("Labels")
            .split(',')
            .filter_map(|pair| {
                pair.split_once('=')
                    .map(|(k, v)| (k.to_owned(), v.to_owned()))
            })
            .collect();
        containers.push(Container {
            name: field("Names"),
            role: labels.get(LABEL_ROLE).cloned().unwrap_or_default(),
            version: labels.get(LABEL_VERSION).cloned().unwrap_or_default(),
            state: field("State"),
            image: field("Image"),
            created: Timestamp::parse_docker(&field("CreatedAt"))
                .unwrap_or_else(Timestamp::go_zero),
        });
    }
    containers.sort_by_key(|c| std::cmp::Reverse(c.created.total_nanos()));
    Ok(containers)
}
