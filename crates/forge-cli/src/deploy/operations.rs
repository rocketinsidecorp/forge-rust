//! `forge deploy` operations: setup, deploy, rollback, prune, maintenance
//! and details, with Forge-Go's algorithm (ADR 0042).

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use forge_proxy::clock::Timestamp;
use serde::Serialize;
use serde_json::{Value, json};

use super::config::VERSION;
use super::deployer::{Container, Deployer, lock};
use super::errors::{
    DeployError, EXIT_FAILED, EXIT_INCONSISTENT, EXIT_MIGRATION_FAILED, EXIT_UNHEALTHY, LockInfo,
};
use super::runner::Cancel;

/// The summary of an operation for `--json`.
#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct Report {
    pub(crate) schema_version: i64,
    pub(crate) action: String,
    pub(crate) service: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub(crate) version: String,
    pub(crate) dry_run: bool,
    pub(crate) steps: Vec<RoleReport>,
    pub(crate) pruned: Vec<String>,
    pub(crate) warnings: Vec<String>,
    pub(crate) duration_ms: i64,
}

/// The containers of one role on one host.
#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct RoleReport {
    pub(crate) host: String,
    pub(crate) role: String,
    pub(crate) container: String,
    pub(crate) stopped: Vec<String>,
}

/// What an operation returns: the report (absent when it never started)
/// and its outcome. The report is printed with `--json` also on failure.
pub(crate) type Outcome = (Option<Report>, Result<(), DeployError>);

/// One role container being brought up on one host.
#[derive(Debug)]
pub(crate) struct Boot {
    pub(crate) host: String,
    pub(crate) role: String,
    pub(crate) name: String,
    pub(crate) version: String,
    /// Rollback: the container exists and is started, not created.
    pub(crate) existing: bool,
    pub(crate) previous: Vec<Container>,
    pub(crate) started: AtomicBool,
    pub(crate) switched: AtomicBool,
}

type Plan = BTreeMap<String, Vec<Boot>>;

/// The running containers of `role`, except `except`.
fn running(containers: &[Container], role: &str, except: &str) -> Vec<Container> {
    containers
        .iter()
        .filter(|c| c.role == role && c.running() && c.name != except)
        .cloned()
        .collect()
}

fn versions_of(containers: &[Container], role: &str) -> Vec<String> {
    let mut versions: Vec<String> = Vec::new();
    for container in containers.iter().filter(|c| c.role == role) {
        if !versions.contains(&container.version) {
            versions.push(container.version.clone());
        }
    }
    if versions.is_empty() {
        versions.push("none".to_owned());
    }
    versions
}

/// Updates a local inventory after a successful switch, so pruning works
/// from the post-deploy state without another query.
pub(crate) fn apply_boot(
    mut containers: Vec<Container>,
    boot: &Boot,
    now: Timestamp,
) -> Vec<Container> {
    for index in 0..containers.len() {
        if boot
            .previous
            .iter()
            .any(|previous| previous.name == containers[index].name)
        {
            "exited".clone_into(&mut containers[index].state);
        }
        if containers[index].name == boot.name {
            "running".clone_into(&mut containers[index].state);
            return containers;
        }
    }
    // The new container is the newest whatever the clocks say: the host's
    // clock, which stamped the others, may be ahead of the local one.
    let mut created = now;
    for container in &containers {
        if container.created.total_nanos() >= created.total_nanos() {
            created = container.created.plus(std::time::Duration::from_secs(1));
        }
    }
    containers.insert(
        0,
        Container {
            name: boot.name.clone(),
            role: boot.role.clone(),
            version: boot.version.clone(),
            state: "running".to_owned(),
            image: String::new(),
            created,
        },
    );
    containers
}

fn prefixed(host: &str, names: &[String]) -> Vec<String> {
    names.iter().map(|name| format!("{host}:{name}")).collect()
}

/// The state of one host for `forge deploy details`.
#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct HostDetails {
    pub(crate) host: String,
    pub(crate) roles: Vec<String>,
    pub(crate) containers: Vec<Container>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) proxy: Option<Vec<Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) lock: Option<LockInfo>,
    pub(crate) recent_deploys: Vec<Value>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub(crate) error: String,
}

impl Deployer {
    fn new_report(&self, action: &str, version: &str) -> Report {
        Report {
            schema_version: 1,
            action: action.to_owned(),
            service: self.config.service.clone(),
            version: version.to_owned(),
            dry_run: self.dry_run,
            ..Report::default()
        }
    }

    fn login(&self, cancel: &Cancel, host: &str) -> Result<(), DeployError> {
        if self.config.registry.username.is_empty() {
            return Ok(());
        }
        let password = self
            .secrets
            .get(&self.config.registry.password)
            .unwrap_or_default();
        self.run(cancel, host, &self.cmds.registry_login(password.as_bytes()))
            .map(|_| ())
            .map_err(|error| error.context(format!("{host}: registry login")))
    }

    /// Verifies Docker on every host, creates the network and state
    /// directory, logs in to the registry and boots forge-proxy and the
    /// accessories. Idempotent.
    pub(crate) fn setup(&self, cancel: &Cancel) -> Outcome {
        if let Err(error) = self.prepare() {
            return (None, Err(error));
        }
        let mut report = self.new_report("setup", "");
        let started = self.now();
        let hosts = self.config.all_hosts();
        let _lock = match self.acquire(cancel, &hosts, "setup") {
            Ok(guard) => guard,
            Err(error) => return (Some(report), Err(error)),
        };
        let image = &self.config.proxy.image;
        if !image.is_empty() && !image.contains("@sha256:") {
            report.warnings.push(format!(
                "proxy.image {image} is not pinned by digest; pin it (image@sha256:...) so every host runs the reviewed build"
            ));
        }
        let proxy_role = self
            .config
            .roles()
            .into_iter()
            .rfind(|role| self.config.proxied(role));
        let result = self.each_host(cancel, &hosts, |cancel, host| {
            let output = self
                .run(cancel, host, &self.cmds.docker_version())
                .map_err(|error| error.context(format!("{host}: docker is not available")))?;
            let version = output.stdout.trim();
            if !version.is_empty() {
                self.logf(host, &format!("docker {version}"));
            }
            for command in [self.cmds.prepare_state_dir(), self.cmds.ensure_network()] {
                self.run(cancel, host, &command)?;
            }
            self.login(cancel, host)?;
            if let Some(role) = &proxy_role
                && self.config.servers[role].hosts.iter().any(|h| h == host)
            {
                self.run(cancel, host, &self.cmds.boot_proxy())
                    .map_err(|error| error.context(format!("{host}: boot forge-proxy")))?;
                self.logf(host, "forge-proxy running");
            }
            for (name, accessory) in &self.config.accessories {
                if accessory.host != host {
                    continue;
                }
                self.run(
                    cancel,
                    host,
                    &self.cmds.upload_accessory_env(name, &self.secrets),
                )?;
                self.run(cancel, host, &self.cmds.boot_accessory(name))
                    .map_err(|error| error.context(format!("{host}: boot accessory {name}")))?;
                self.logf(host, &format!("accessory {name} running"));
            }
            Ok(())
        });
        report.duration_ms = self.now().millis_since(started);
        (Some(report), result)
    }

    /// Builds (unless skipped), distributes and migrates `version`, then
    /// switches every host to it. All or nothing: previous containers keep
    /// running until every host passed its health gate; if any host fails,
    /// switched hosts are routed back and the new containers removed.
    pub(crate) fn deploy(&self, cancel: &Cancel, version: &str, skip_build: bool) -> Outcome {
        if let Err(error) = self.prepare() {
            return (None, Err(error));
        }
        if !VERSION.matches(version) {
            return (
                None,
                Err(DeployError::new(format!(
                    "version {version:?} must match {}",
                    VERSION.source()
                ))),
            );
        }
        let mut report = self.new_report("deploy", version);
        let started = self.now();
        let hosts = self.config.app_hosts();
        let _lock = match self.acquire(cancel, &hosts, &format!("deploy {version}")) {
            Ok(guard) => guard,
            Err(error) => return (Some(report), Err(error)),
        };
        let result = (|| {
            let env = self.hook_env("deploy", version, None);
            if !skip_build {
                self.hook(cancel, "pre-build", &env)?;
                self.build(cancel, version)?;
            }
            self.hook(cancel, "pre-deploy", &env)?;
            let image = self.cmds.image_ref(version);
            self.each_host(cancel, &hosts, |cancel, host| {
                self.login(cancel, host)?;
                self.run(cancel, host, &self.cmds.pull(version))
                    .map_err(|error| error.context(format!("{host}: pull {image}")))?;
                for role in self.config.roles_on(host) {
                    self.run(
                        cancel,
                        host,
                        &self.cmds.upload_role_env(&role, &self.secrets),
                    )?;
                }
                self.logf(host, &format!("pulled {image}"));
                Ok(())
            })?;
            if self.config.migrate.enabled == Some(true) {
                let host = self.config.servers[&self.config.migrate.role].hosts[0].clone();
                self.logf(&host, &format!("migrating with {image}"));
                self.run(cancel, &host, &self.cmds.migrate(version, &self.suffix()))
                    .map_err(|error| {
                        error
                            .context("migration failed; nothing was switched")
                            .with_code(EXIT_MIGRATION_FAILED)
                    })?;
            }
            let inventories = self.inventories(cancel, &hosts)?;
            let mut plan: Plan = BTreeMap::new();
            for host in &hosts {
                for role in self.config.roles_on(host) {
                    let boot = Boot {
                        host: host.clone(),
                        name: self.cmds.container_name(&role, version, &self.suffix()),
                        previous: running(
                            inventories.get(host).map_or(&[][..], Vec::as_slice),
                            &role,
                            "",
                        ),
                        role,
                        version: version.to_owned(),
                        existing: false,
                        started: AtomicBool::new(false),
                        switched: AtomicBool::new(false),
                    };
                    plan.entry(host.clone()).or_default().push(boot);
                }
            }
            self.switch_all(cancel, &hosts, &plan)?;
            self.finish(cancel, &mut report, &hosts, &plan, &inventories);
            self.hook(
                cancel,
                "post-deploy",
                &self.hook_env("deploy", version, Some(started)),
            )?;
            Ok(())
        })();
        if result.is_ok() {
            report.duration_ms = self.now().millis_since(started);
        }
        (Some(report), result)
    }

    /// Boots every planned container; reverts all hosts if any host fails.
    fn switch_all(
        &self,
        cancel: &Cancel,
        hosts: &[String],
        plan: &Plan,
    ) -> Result<(), DeployError> {
        let Err(error) = self.each_host(cancel, hosts, |cancel, host| {
            for boot in plan.get(host).map_or(&[][..], Vec::as_slice) {
                self.boot_one(cancel, boot)?;
            }
            Ok(())
        }) else {
            return Ok(());
        };
        let code = if error.health {
            EXIT_UNHEALTHY
        } else {
            EXIT_FAILED
        };
        if cancel.is_cancelled() {
            return Err(error
                .context("interrupted while switching; hosts may run different versions, run forge deploy details")
                .with_code(EXIT_INCONSISTENT));
        }
        self.logf(
            "revert",
            &format!("switch failed, reverting every host: {error}"),
        );
        let revert = self.each_host(&Cancel::never(), hosts, |cancel, host| {
            self.revert(cancel, plan.get(host).map_or(&[][..], Vec::as_slice))
        });
        if let Err(revert_error) = revert {
            return Err(error
                .suffix(format!(
                    "reverting also failed, hosts may run different versions (run forge deploy details): {revert_error}"
                ))
                .with_code(EXIT_INCONSISTENT));
        }
        Err(error
            .suffix("every host serves its previous version")
            .with_code(code))
    }

    fn boot_one(&self, cancel: &Cancel, boot: &Boot) -> Result<(), DeployError> {
        let (command, verb) = if boot.existing {
            (self.cmds.start(&boot.name), "restarted")
        } else {
            (
                self.cmds.run_role(&boot.role, &boot.version, &boot.name),
                "started",
            )
        };
        self.run(cancel, &boot.host, &command)
            .map_err(|error| error.context(format!("{}: start {}", boot.host, boot.name)))?;
        boot.started.store(true, Ordering::SeqCst);
        self.logf(&boot.host, &format!("{verb} {}", boot.name));
        if self.config.proxied(&boot.role) {
            if let Err(error) = self.run(cancel, &boot.host, &self.cmds.proxy_deploy(&boot.name)) {
                if error.remote_exit == Some(4) {
                    return Err(error
                        .context(format!(
                            "{}: {} failed its health check",
                            boot.host, boot.name
                        ))
                        .health());
                }
                return Err(error.context(format!("{}: proxy switch to {}", boot.host, boot.name)));
            }
            boot.switched.store(true, Ordering::SeqCst);
            self.logf(&boot.host, &format!("traffic switched to {}", boot.name));
            return Ok(());
        }
        self.run(cancel, &boot.host, &self.cmds.wait_ready(&boot.name))
            .map_err(|error| {
                error
                    .context(format!("{}: {} did not become ready", boot.host, boot.name))
                    .health()
            })?;
        self.logf(&boot.host, &format!("{} ready", boot.name));
        Ok(())
    }

    /// Undoes `boot_one` in reverse order. A role without a previous
    /// container keeps the new one serving: there is nothing to go back to.
    fn revert(&self, cancel: &Cancel, boots: &[Boot]) -> Result<(), DeployError> {
        let mut errors = Vec::new();
        for boot in boots.iter().rev() {
            if !boot.started.load(Ordering::SeqCst) {
                continue;
            }
            if self.config.proxied(&boot.role) && boot.switched.load(Ordering::SeqCst) {
                let Some(previous) = boot.previous.first() else {
                    self.logf(
                        &boot.host,
                        &format!(
                            "keeping {}: no previous {} container to return to",
                            boot.name, boot.role
                        ),
                    );
                    continue;
                };
                if let Err(error) =
                    self.run(cancel, &boot.host, &self.cmds.proxy_deploy(&previous.name))
                {
                    errors.push(
                        error.context(format!("{}: switch back to {}", boot.host, previous.name)),
                    );
                    continue;
                }
                self.logf(
                    &boot.host,
                    &format!("traffic switched back to {}", previous.name),
                );
            }
            if let Err(error) = self.run(
                cancel,
                &boot.host,
                &self.cmds.stop(std::slice::from_ref(&boot.name)),
            ) {
                errors.push(error);
                continue;
            }
            if !boot.existing
                && let Err(error) = self.run(
                    cancel,
                    &boot.host,
                    &self.cmds.remove(std::slice::from_ref(&boot.name)),
                )
            {
                errors.push(error);
                continue;
            }
            self.logf(&boot.host, &format!("removed {}", boot.name));
        }
        DeployError::join(errors).map_or(Ok(()), Err)
    }

    /// Stops previous containers, prunes beyond `retain` and records the
    /// deploy on each host. Failures here are warnings: the new version
    /// serves.
    fn finish(
        &self,
        cancel: &Cancel,
        report: &mut Report,
        hosts: &[String],
        plan: &Plan,
        inventories: &BTreeMap<String, Vec<Container>>,
    ) {
        let shared = Mutex::new((Vec::new(), Vec::new(), Vec::new()));
        let _ = self.each_host(cancel, hosts, |cancel, host| {
            let mut containers = inventories.get(host).cloned().unwrap_or_default();
            let mut roles = BTreeMap::new();
            for boot in plan.get(host).map_or(&[][..], Vec::as_slice) {
                roles.insert(boot.role.clone(), boot.name.clone());
                let stopped: Vec<String> = boot.previous.iter().map(|c| c.name.clone()).collect();
                if !stopped.is_empty() {
                    match self.run(cancel, host, &self.cmds.stop(&stopped)) {
                        Err(error) => lock(&shared).1.push(format!(
                            "{host}: stopping {} failed: {error}",
                            stopped.join(", ")
                        )),
                        Ok(_) => self.logf(host, &format!("stopped {}", stopped.join(", "))),
                    }
                }
                containers = apply_boot(containers, boot, self.now());
                lock(&shared).0.push(RoleReport {
                    host: host.to_owned(),
                    role: boot.role.clone(),
                    container: boot.name.clone(),
                    stopped,
                });
            }
            let (pruned, mut warnings) = self.prune_host(cancel, host, containers);
            let entry = json!({
                "time": self.now().utc().rfc3339_nano(),
                "action": report.action,
                "service": self.config.service,
                "version": report.version,
                "performer": self.performer,
                "host": host,
                "containers": roles,
            });
            let mut line = entry.to_string().into_bytes();
            line.push(b'\n');
            if let Err(error) = self.run(cancel, host, &self.cmds.append_log(line)) {
                warnings.push(format!("{host}: recording the deploy failed: {error}"));
            }
            let mut shared = lock(&shared);
            shared.2.extend(pruned);
            shared.1.extend(warnings);
            Ok(())
        });
        let (mut steps, warnings, mut pruned) = shared
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        steps.sort_by(|a, b| {
            (a.host.as_str(), a.role.as_str()).cmp(&(b.host.as_str(), b.role.as_str()))
        });
        report.steps.extend(steps);
        report.warnings.extend(warnings);
        report.pruned.append(&mut pruned);
        report.pruned.sort();
    }

    /// Removes stopped containers beyond `retain` per role and the images no
    /// remaining container uses. Never touches running containers.
    pub(crate) fn prune_host(
        &self,
        cancel: &Cancel,
        host: &str,
        containers: Vec<Container>,
    ) -> (Vec<String>, Vec<String>) {
        let mut remove: Vec<String> = Vec::new();
        let mut warnings = Vec::new();
        let mut keep_versions = std::collections::HashSet::new();
        let mut by_role: BTreeMap<String, Vec<Container>> = BTreeMap::new();
        for container in containers {
            by_role
                .entry(container.role.clone())
                .or_default()
                .push(container);
        }
        let retain = usize::try_from(self.config.retain).unwrap_or(0);
        for list in by_role.values_mut() {
            list.sort_by_key(|c| std::cmp::Reverse(c.created.total_nanos()));
            for (index, container) in list.iter().enumerate() {
                if index < retain || container.running() {
                    keep_versions.insert(container.version.clone());
                    continue;
                }
                remove.push(container.name.clone());
            }
        }
        if !remove.is_empty() {
            if let Err(error) = self.run(cancel, host, &self.cmds.remove(&remove)) {
                return (
                    Vec::new(),
                    vec![format!("{host}: pruning containers failed: {error}")],
                );
            }
            self.logf(host, &format!("pruned {}", remove.join(", ")));
        }
        let output = match self.run(cancel, host, &self.cmds.images()) {
            Ok(output) => output,
            Err(error) => {
                warnings.push(format!("{host}: listing images failed: {error}"));
                return (prefixed(host, &remove), warnings);
            }
        };
        for line in output.stdout.lines() {
            let Ok(image) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            let repository = image["Repository"].as_str().unwrap_or_default();
            let tag = image["Tag"].as_str().unwrap_or_default();
            if tag.is_empty()
                || tag == "<none>"
                || keep_versions.contains(tag)
                || repository != self.config.image
            {
                continue;
            }
            let reference = format!("{repository}:{tag}");
            if let Err(error) = self.run(cancel, host, &self.cmds.remove_image(&reference)) {
                warnings.push(format!(
                    "{host}: removing image {reference} failed: {error}"
                ));
                continue;
            }
            remove.push(reference);
        }
        (prefixed(host, &remove), warnings)
    }

    /// Switches every host back to the retained containers of `version`,
    /// which restart with the environment they were created with.
    /// Migrations are not reverted.
    pub(crate) fn rollback(&self, cancel: &Cancel, version: &str) -> Outcome {
        if let Err(error) = self.prepare() {
            return (None, Err(error));
        }
        if !VERSION.matches(version) {
            return (
                None,
                Err(DeployError::new(format!(
                    "version {version:?} must match {}",
                    VERSION.source()
                ))),
            );
        }
        let mut report = self.new_report("rollback", version);
        let started = self.now();
        let hosts = self.config.app_hosts();
        let _lock = match self.acquire(cancel, &hosts, &format!("rollback to {version}")) {
            Ok(guard) => guard,
            Err(error) => return (Some(report), Err(error)),
        };
        let result = (|| {
            let inventories = self.inventories(cancel, &hosts)?;
            let mut plan: Plan = BTreeMap::new();
            let mut missing = Vec::new();
            for host in &hosts {
                let inventory = inventories.get(host).map_or(&[][..], Vec::as_slice);
                for role in self.config.roles_on(host) {
                    let Some(target) = inventory
                        .iter()
                        .find(|c| c.role == role && c.version == version)
                    else {
                        missing.push(format!(
                            "{host}/{role} (available: {})",
                            versions_of(inventory, &role).join(", ")
                        ));
                        continue;
                    };
                    let others = running(inventory, &role, &target.name);
                    if target.running() && others.is_empty() {
                        self.logf(host, &format!("{role} already runs {version}"));
                        continue;
                    }
                    plan.entry(host.clone()).or_default().push(Boot {
                        host: host.clone(),
                        role,
                        name: target.name.clone(),
                        version: version.to_owned(),
                        existing: true,
                        previous: others,
                        started: AtomicBool::new(false),
                        switched: AtomicBool::new(false),
                    });
                }
            }
            if !missing.is_empty() {
                return Err(DeployError::new(format!(
                    "version {version} is not retained on {}; nothing was changed",
                    missing.join("; ")
                )));
            }
            self.hook(
                cancel,
                "pre-deploy",
                &self.hook_env("rollback", version, None),
            )?;
            self.switch_all(cancel, &hosts, &plan)?;
            self.finish(cancel, &mut report, &hosts, &plan, &inventories);
            self.hook(
                cancel,
                "post-deploy",
                &self.hook_env("rollback", version, Some(started)),
            )?;
            Ok(())
        })();
        if result.is_ok() {
            report.duration_ms = self.now().millis_since(started);
        }
        (Some(report), result)
    }

    /// Removes stopped containers beyond `retain` and unused images.
    pub(crate) fn prune(&self, cancel: &Cancel) -> Outcome {
        if let Err(error) = self.prepare() {
            return (None, Err(error));
        }
        let mut report = self.new_report("prune", "");
        let started = self.now();
        let hosts = self.config.app_hosts();
        let _lock = match self.acquire(cancel, &hosts, "prune") {
            Ok(guard) => guard,
            Err(error) => return (Some(report), Err(error)),
        };
        let shared = Mutex::new((Vec::new(), Vec::new()));
        let result = self.each_host(cancel, &hosts, |cancel, host| {
            let containers = self.inventory(cancel, host)?;
            let (pruned, warnings) = self.prune_host(cancel, host, containers);
            let mut shared = lock(&shared);
            shared.0.extend(pruned);
            shared.1.extend(warnings);
            Ok(())
        });
        let (mut pruned, warnings) = shared
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pruned.sort();
        report.pruned = pruned;
        report.warnings = warnings;
        report.duration_ms = self.now().millis_since(started);
        (Some(report), result)
    }

    /// Turns the proxy maintenance page on or off on every web host.
    pub(crate) fn maintenance(&self, cancel: &Cancel, on: bool) -> Result<(), DeployError> {
        self.prepare()?;
        let role = self.config.primary_role();
        if !self.config.proxied(&role) {
            return Err(DeployError::new("no role uses the proxy"));
        }
        let command = self.cmds.proxy_maintenance(on);
        self.each_host(cancel, &self.config.servers[&role].hosts, |cancel, host| {
            self.run(cancel, host, &command)?;
            self.logf(host, &format!("maintenance {on}"));
            Ok(())
        })
    }

    /// Containers, proxy routing, lock and recent deploys of every host.
    pub(crate) fn details(&self, cancel: &Cancel) -> (Vec<HostDetails>, Result<(), DeployError>) {
        if let Err(error) = self.prepare() {
            return (Vec::new(), Err(error));
        }
        let hosts = self.config.app_hosts();
        let details: Vec<Mutex<HostDetails>> = hosts
            .iter()
            .map(|host| {
                Mutex::new(HostDetails {
                    host: host.clone(),
                    roles: self.config.roles_on(host),
                    ..HostDetails::default()
                })
            })
            .collect();
        let result = self.each_host(cancel, &hosts, |cancel, host| {
            let index = hosts.iter().position(|h| h == host).unwrap_or(0);
            let mut detail = HostDetails {
                host: host.to_owned(),
                roles: self.config.roles_on(host),
                ..HostDetails::default()
            };
            let outcome = (|| {
                match self.inventory(cancel, host) {
                    Ok(containers) => detail.containers = containers,
                    Err(error) => {
                        detail.error = error.to_string();
                        return Err(error);
                    }
                }
                if detail.roles.iter().any(|role| self.config.proxied(role)) {
                    match self.run(cancel, host, &self.cmds.proxy_status()) {
                        Err(error) => detail.error = format!("forge-proxy: {error}"),
                        Ok(output) => {
                            if let Ok(status) = serde_json::from_str::<Value>(&output.stdout)
                                && let Some(services) = status["services"].as_array()
                            {
                                detail.proxy = Some(services.clone());
                            }
                        }
                    }
                }
                if let Ok(output) = self.run(cancel, host, &self.cmds.lock_status())
                    && !output.stdout.trim().is_empty()
                    && let Ok(info) = serde_json::from_str::<LockInfo>(output.stdout.trim())
                {
                    detail.lock = Some(info);
                }
                if let Ok(output) = self.run(cancel, host, &self.cmds.read_log(5)) {
                    for line in output.stdout.lines() {
                        if let Ok(entry) = serde_json::from_str::<Value>(line.trim()) {
                            detail.recent_deploys.push(entry);
                        }
                    }
                }
                Ok(())
            })();
            *lock(&details[index]) = detail;
            outcome
        });
        (
            details
                .into_iter()
                .map(|slot| {
                    slot.into_inner()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                })
                .collect(),
            result,
        )
    }
}
