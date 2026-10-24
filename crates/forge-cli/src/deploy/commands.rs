//! The remote commands of one configuration, kept apart from the
//! orchestration so their exact text is reviewable and golden-tested.
//!
//! Every command is a POSIX `sh` script built from fixed control flow and
//! validated configuration values, each passed through [`quote`] as one
//! shell word. Secrets never appear in a script: they travel on stdin.
//! Remote paths are relative to the SSH user's home directory, where
//! commands start.

use std::collections::BTreeMap;

use super::config::{Config, publish_spec};
use super::errors::{LOCK_GONE_EXIT_STATUS, LOCK_HELD_EXIT_STATUS, LOCK_NOT_OURS_EXIT_STATUS};
use super::secrets::Secrets;

pub(crate) const NETWORK_NAME: &str = "forge";
pub(crate) const PROXY_CONTAINER: &str = "forge-proxy";
const PROXY_VOLUME: &str = "forge-proxy-data";
const PROXY_BINARY: &str = "/usr/local/bin/forge-proxy";
const STATE_ROOT: &str = ".forge";
pub(crate) const LABEL_SERVICE: &str = "forge.service";
pub(crate) const LABEL_ROLE: &str = "forge.role";
pub(crate) const LABEL_VERSION: &str = "forge.version";
const LABEL_ACCESSORY: &str = "forge.accessory";
/// Printed by [`Commands::acquire_lock`] on stderr after it took over a lock
/// that never got its record.
pub(crate) const STALE_LOCK_NOTICE: &str =
    "took over a stale lock without a record (an earlier acquire was interrupted)";

/// The application setting that lists the proxies whose `X-Forwarded-For`
/// the application believes (ADR 0032).
pub(crate) const TRUSTED_PROXIES_ENV: &str = forge_config::TRUSTED_PROXIES_KEY;

/// One remote shell command. `stdin` is written to its standard input and
/// never printed; `stdin_note` describes it for `--dry-run`. Read-only
/// commands only inspect state, so `--dry-run` still runs them and the
/// plan names real containers.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct RemoteCommand {
    pub(crate) script: String,
    pub(crate) stdin: Option<Vec<u8>>,
    pub(crate) stdin_note: String,
    pub(crate) read_only: bool,
}

impl RemoteCommand {
    fn new(script: String) -> Self {
        Self {
            script,
            ..Self::default()
        }
    }

    fn read_only(script: String) -> Self {
        Self {
            script,
            read_only: true,
            ..Self::default()
        }
    }
}

/// `text` as one POSIX shell word: unchanged when it is made only of safe
/// characters, otherwise single-quoted with `'` written as `'\''`.
pub(crate) fn quote(text: &str) -> String {
    let safe = !text.is_empty()
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(&b));
    if safe {
        text.to_owned()
    } else {
        format!("'{}'", text.replace('\'', "'\\''"))
    }
}

/// Quotes and joins `argv` into a command line.
pub(crate) fn join<S: AsRef<str>>(argv: &[S]) -> String {
    argv.iter()
        .map(|arg| quote(arg.as_ref()))
        .collect::<Vec<_>>()
        .join(" ")
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}

/// Builds the remote commands of one configuration.
#[derive(Clone, Debug)]
pub(crate) struct Commands {
    cfg: Config,
}

impl Commands {
    pub(crate) fn new(cfg: Config) -> Self {
        Self { cfg }
    }

    fn state_dir(&self) -> String {
        format!("{STATE_ROOT}/{}", self.cfg.service)
    }

    fn env_dir(&self) -> String {
        format!("{}/env", self.state_dir())
    }

    fn lock_dir(&self) -> String {
        format!("{}/lock", self.state_dir())
    }

    fn lock_info(&self) -> String {
        format!("{}/info", self.lock_dir())
    }

    fn log_file(&self) -> String {
        format!("{}/deploys.log", self.state_dir())
    }

    pub(crate) fn env_file(&self, role: &str) -> String {
        format!("{}/{role}.env", self.env_dir())
    }

    fn accessory_env_file(&self, name: &str) -> String {
        format!("{}/accessory-{name}.env", self.env_dir())
    }

    /// `IMAGE:VERSION`.
    pub(crate) fn image_ref(&self, version: &str) -> String {
        format!("{}:{version}", self.cfg.image)
    }

    pub(crate) fn container_name(&self, role: &str, version: &str, suffix: &str) -> String {
        format!("{}-{role}-{version}-{suffix}", self.cfg.service)
    }

    fn accessory_name(&self, name: &str) -> String {
        format!("{}-{name}", self.cfg.service)
    }

    fn stop_seconds(&self) -> String {
        self.cfg.stop_timeout.whole_seconds().to_string()
    }

    pub(crate) fn docker_version(&self) -> RemoteCommand {
        RemoteCommand::read_only("docker version --format '{{.Server.Version}}'".to_owned())
    }

    pub(crate) fn prepare_state_dir(&self) -> RemoteCommand {
        RemoteCommand::new(format!(
            "{} && {}",
            join(&["mkdir", "-p", &self.env_dir()]),
            join(&[
                "chmod",
                "700",
                STATE_ROOT,
                &self.state_dir(),
                &self.env_dir()
            ])
        ))
    }

    pub(crate) fn ensure_network(&self) -> RemoteCommand {
        RemoteCommand::new(format!(
            "{} >/dev/null 2>&1 || {}",
            join(&["docker", "network", "inspect", NETWORK_NAME]),
            join(&["docker", "network", "create", NETWORK_NAME])
        ))
    }

    /// `docker login` reading the password from stdin.
    pub(crate) fn login_argv(&self) -> Vec<String> {
        let mut argv = strings(&["docker", "login"]);
        if !self.cfg.registry.server.is_empty() {
            argv.push(self.cfg.registry.server.clone());
        }
        argv.extend([
            "--username".to_owned(),
            self.cfg.registry.username.clone(),
            "--password-stdin".to_owned(),
        ]);
        argv
    }

    pub(crate) fn registry_login(&self, password: &[u8]) -> RemoteCommand {
        RemoteCommand {
            script: join(&self.login_argv()),
            stdin: Some(password.to_vec()),
            stdin_note: format!("registry password ({})", self.cfg.registry.password),
            read_only: false,
        }
    }

    pub(crate) fn pull(&self, version: &str) -> RemoteCommand {
        RemoteCommand::new(join(&["docker", "pull", &self.image_ref(version)]))
    }

    /// Writes `content` to `path` atomically with 0600 permissions; the
    /// content never appears in the command line or the process list.
    fn upload(&self, path: &str, content: Vec<u8>, note: String) -> RemoteCommand {
        let directory = &path[..path.rfind('/').unwrap_or(0)];
        let temporary = format!("{path}.tmp");
        RemoteCommand {
            script: format!(
                "umask 077 && {} && {} > {} && {}",
                join(&["mkdir", "-p", directory]),
                join(&["cat"]),
                quote(&temporary),
                join(&["mv", &temporary, path])
            ),
            stdin: Some(content),
            stdin_note: note,
            read_only: false,
        }
    }

    /// The global and role environment.
    pub(crate) fn role_env(&self, role: &str) -> (BTreeMap<String, String>, Vec<String>) {
        let mut clear = self.cfg.env.clear.clone();
        let mut secret = self.cfg.env.secret.clone();
        if let Some(role) = self.cfg.servers.get(role) {
            clear.extend(role.env.clear.clone());
            for name in &role.env.secret {
                if !secret.contains(name) {
                    secret.push(name.clone());
                }
            }
        }
        secret.sort();
        (clear, secret)
    }

    pub(crate) fn upload_role_env(&self, role: &str, secrets: &Secrets) -> RemoteCommand {
        let (clear, secret) = self.role_env(role);
        let note = format!(
            "env file: {} clear, {} secret ({}), values not shown",
            clear.len(),
            secret.len(),
            secret.join(", ")
        );
        self.upload(
            &self.env_file(role),
            env_content(&clear, &secret, secrets),
            note,
        )
    }

    fn role_volumes(&self, role: &str) -> Vec<String> {
        let mut volumes = self.cfg.volumes.clone();
        if let Some(role) = self.cfg.servers.get(role) {
            volumes.extend(role.volumes.iter().cloned());
        }
        volumes
    }

    pub(crate) fn migrate(&self, version: &str, suffix: &str) -> RemoteCommand {
        let role = &self.cfg.migrate.role;
        let mut argv = vec![
            "docker".to_owned(),
            "run".to_owned(),
            "--rm".to_owned(),
            "--name".to_owned(),
            format!("{}-migrate-{version}-{suffix}", self.cfg.service),
            "--network".to_owned(),
            NETWORK_NAME.to_owned(),
            "--env-file".to_owned(),
            self.env_file(role),
        ];
        argv.extend(volume_args(&self.role_volumes(role)));
        argv.push(self.image_ref(version));
        argv.extend(self.cfg.migrate.cmd.iter().cloned());
        RemoteCommand::new(join(&argv))
    }

    pub(crate) fn run_role(&self, role: &str, version: &str, name: &str) -> RemoteCommand {
        let head = vec![
            "docker".to_owned(),
            "run".to_owned(),
            "--detach".to_owned(),
            "--name".to_owned(),
            name.to_owned(),
            "--network".to_owned(),
            NETWORK_NAME.to_owned(),
            "--restart".to_owned(),
            "unless-stopped".to_owned(),
            "--stop-timeout".to_owned(),
            self.stop_seconds(),
            "--env-file".to_owned(),
            self.env_file(role),
        ];
        let mut tail = vec![
            "--label".to_owned(),
            format!("{LABEL_SERVICE}={}", self.cfg.service),
            "--label".to_owned(),
            format!("{LABEL_ROLE}={role}"),
            "--label".to_owned(),
            format!("{LABEL_VERSION}={version}"),
        ];
        tail.extend(volume_args(&self.role_volumes(role)));
        tail.push(self.image_ref(version));
        if let Some(role) = self.cfg.servers.get(role) {
            tail.extend(role.cmd.iter().cloned());
        }
        if !self.trusts_proxy_network(role) {
            let mut argv = head;
            argv.extend(tail);
            return RemoteCommand::new(join(&argv));
        }
        RemoteCommand::new(format!(
            "{}; {} --env \"{TRUSTED_PROXIES_ENV}=$forge_proxies\" {}",
            proxy_network_script(),
            join(&head),
            join(&tail)
        ))
    }

    /// Whether the proxied role's containers are told to trust the forge
    /// network as their proxies: always, unless the configuration sets
    /// `FORGE_HTTP_TRUSTED_PROXIES` itself (an empty value trusts nobody).
    pub(crate) fn trusts_proxy_network(&self, role: &str) -> bool {
        if !self.cfg.proxied(role) {
            return false;
        }
        let (clear, secret) = self.role_env(role);
        !clear.contains_key(TRUSTED_PROXIES_ENV)
            && !secret.iter().any(|name| name == TRUSTED_PROXIES_ENV)
    }

    pub(crate) fn start(&self, name: &str) -> RemoteCommand {
        RemoteCommand::new(join(&["docker", "start", name]))
    }

    /// SIGTERM, then up to `stop_timeout` before SIGKILL, which lets the
    /// application's graceful shutdown finish.
    pub(crate) fn stop(&self, names: &[String]) -> RemoteCommand {
        let mut argv = vec![
            "docker".to_owned(),
            "stop".to_owned(),
            "-t".to_owned(),
            self.stop_seconds(),
        ];
        argv.extend(names.iter().cloned());
        RemoteCommand::new(join(&argv))
    }

    pub(crate) fn remove(&self, names: &[String]) -> RemoteCommand {
        let mut argv = vec!["docker".to_owned(), "rm".to_owned()];
        argv.extend(names.iter().cloned());
        RemoteCommand::new(join(&argv))
    }

    pub(crate) fn remove_image(&self, reference: &str) -> RemoteCommand {
        RemoteCommand::new(join(&["docker", "image", "rm", reference]))
    }

    pub(crate) fn inventory(&self) -> RemoteCommand {
        RemoteCommand::read_only(join(&[
            "docker",
            "ps",
            "--all",
            "--no-trunc",
            "--filter",
            &format!("label={LABEL_SERVICE}={}", self.cfg.service),
            "--format",
            "{{json .}}",
        ]))
    }

    pub(crate) fn images(&self) -> RemoteCommand {
        RemoteCommand::read_only(join(&[
            "docker",
            "image",
            "ls",
            "--format",
            "{{json .}}",
            &self.cfg.image,
        ]))
    }

    /// Asks forge-proxy to health-gate and switch to `name`. The proxy exits
    /// 4 when the target never became healthy.
    pub(crate) fn proxy_deploy(&self, name: &str) -> RemoteCommand {
        let proxy = &self.cfg.proxy;
        let mut argv = vec![
            "docker".to_owned(),
            "exec".to_owned(),
            PROXY_CONTAINER.to_owned(),
            PROXY_BINARY.to_owned(),
            "deploy".to_owned(),
            "-service".to_owned(),
            self.cfg.service.clone(),
            "-target".to_owned(),
            format!("{name}:{}", proxy.app_port),
        ];
        if !proxy.hosts.is_empty() {
            argv.extend(["-hosts".to_owned(), proxy.hosts.join(",")]);
        }
        if proxy.ssl {
            argv.push("-tls".to_owned());
        }
        let health = &proxy.healthcheck;
        argv.extend([
            "-health-path".to_owned(),
            health.path.clone(),
            "-health-interval".to_owned(),
            health.interval.to_string(),
            "-health-timeout".to_owned(),
            health.timeout.to_string(),
            "-healthy-count".to_owned(),
            health.healthy_count.to_string(),
            "-deploy-timeout".to_owned(),
            health.deploy_timeout.to_string(),
            "-drain-timeout".to_owned(),
            proxy.drain_timeout.to_string(),
            "-response-timeout".to_owned(),
            proxy.response_timeout.to_string(),
            "-max-body-bytes".to_owned(),
            proxy.max_body_bytes.to_string(),
        ]);
        RemoteCommand::new(join(&argv))
    }

    pub(crate) fn proxy_status(&self) -> RemoteCommand {
        RemoteCommand::read_only(join(&[
            "docker",
            "exec",
            PROXY_CONTAINER,
            PROXY_BINARY,
            "status",
        ]))
    }

    pub(crate) fn proxy_maintenance(&self, on: bool) -> RemoteCommand {
        RemoteCommand::new(join(&[
            "docker",
            "exec",
            PROXY_CONTAINER,
            PROXY_BINARY,
            "maintenance",
            "-service",
            &self.cfg.service,
            if on { "on" } else { "off" },
        ]))
    }

    /// Starts forge-proxy unless it already exists. Ports 80 and 443 are
    /// published on all addresses; the control socket is reachable only
    /// through `docker exec`.
    pub(crate) fn boot_proxy(&self) -> RemoteCommand {
        let mut argv = strings(&[
            "docker",
            "run",
            "--detach",
            "--name",
            PROXY_CONTAINER,
            "--network",
            NETWORK_NAME,
            "--restart",
            "unless-stopped",
            "--publish",
            "80:8080",
            "--publish",
            "443:8443",
            "--volume",
        ]);
        argv.push(format!("{PROXY_VOLUME}:/var/lib/forge-proxy"));
        argv.extend(["--label".to_owned(), "forge.proxy=true".to_owned()]);
        argv.push(self.cfg.proxy.image.clone());
        argv.push("run".to_owned());
        if !self.cfg.proxy.acme_email.is_empty() {
            argv.extend(["-acme-email".to_owned(), self.cfg.proxy.acme_email.clone()]);
        }
        RemoteCommand::new(format!(
            "{} >/dev/null 2>&1 && {} >/dev/null || {}",
            join(&["docker", "container", "inspect", PROXY_CONTAINER]),
            join(&["docker", "start", PROXY_CONTAINER]),
            join(&argv)
        ))
    }

    pub(crate) fn upload_accessory_env(&self, name: &str, secrets: &Secrets) -> RemoteCommand {
        let accessory = self.cfg.accessories.get(name).cloned().unwrap_or_default();
        let mut secret = accessory.env.secret.clone();
        secret.sort();
        let note = format!(
            "env file: {} clear, {} secret ({}), values not shown",
            accessory.env.clear.len(),
            secret.len(),
            secret.join(", ")
        );
        self.upload(
            &self.accessory_env_file(name),
            env_content(&accessory.env.clear, &secret, secrets),
            note,
        )
    }

    pub(crate) fn boot_accessory(&self, name: &str) -> RemoteCommand {
        let accessory = self.cfg.accessories.get(name).cloned().unwrap_or_default();
        let container = self.accessory_name(name);
        let mut argv = vec![
            "docker".to_owned(),
            "run".to_owned(),
            "--detach".to_owned(),
            "--name".to_owned(),
            container.clone(),
            "--network".to_owned(),
            NETWORK_NAME.to_owned(),
            "--restart".to_owned(),
            "unless-stopped".to_owned(),
            "--env-file".to_owned(),
            self.accessory_env_file(name),
            "--label".to_owned(),
            format!("{LABEL_ACCESSORY}={}/{name}", self.cfg.service),
        ];
        if !accessory.port.is_empty()
            && let Ok(spec) = publish_spec(&accessory.port)
        {
            argv.extend(["--publish".to_owned(), spec]);
        }
        argv.extend(volume_args(&accessory.volumes));
        argv.push(accessory.image.clone());
        argv.extend(accessory.cmd.iter().cloned());
        RemoteCommand::new(format!(
            "{} >/dev/null 2>&1 && {} >/dev/null || {}",
            join(&["docker", "container", "inspect", &container]),
            join(&["docker", "start", &container]),
            join(&argv)
        ))
    }

    /// Polls a container without a proxy until Docker reports it healthy
    /// (when the image has a HEALTHCHECK) or running for three checks.
    pub(crate) fn wait_ready(&self, name: &str) -> RemoteCommand {
        let seconds = self.cfg.boot_timeout.whole_seconds();
        let format =
            "{{.State.Status}} {{if .State.Health}}{{.State.Health.Status}}{{else}}none{{end}}";
        RemoteCommand::new(format!(
            "end=$(($(date +%s)+{seconds})); ok=0; while :; do s=$({}) || exit 1; case \"$s\" in \"running healthy\") exit 0;; \"running none\") ok=$((ok+1)); [ \"$ok\" -ge 3 ] && exit 0;; \"running starting\") ;; *) echo \"container {name} is $s\" >&2; exit 1;; esac; [ \"$(date +%s)\" -ge \"$end\" ] && {{ echo \"container {name} not ready after {seconds}s\" >&2; exit 1; }}; sleep 1; done",
            join(&["docker", "inspect", "--format", format, name])
        ))
    }

    /// Stores the record in a temporary file and claims the lock by
    /// hard-linking it to `lock/info`: `ln` fails when `info` exists, so
    /// exactly one claimant wins and the record is never visible half
    /// written. Only a successful `ln` wins; nothing else in the script can
    /// turn a loss into a win or a win into a loss, and the takeover notice
    /// is printed after the win is decided. The directory comes from an
    /// atomic `mkdir`. A directory with no `info` file at all (the acquire
    /// was killed between `mkdir` and `ln`) that is older than a minute is
    /// taken over in place with the same `ln`, so nothing is ever removed and
    /// two recoverers cannot both win; the script says so on stderr
    /// ([`STALE_LOCK_NOTICE`]). An empty `info` file (left by earlier
    /// versions) is not taken over: `--force` clears it.
    ///
    /// The temporary file is created under noclobber (`set -C`), so a name
    /// left by an earlier process with the same PID, possibly still linked
    /// to `lock/info`, is never truncated: the redirection fails and the
    /// acquire exits 1. When `ln` fails and `info` does not exist (hard
    /// links unsupported on the file system), its own error is printed on
    /// stderr before the acquire reports the lock as held.
    ///
    /// The text is Forge-Go's `acquireLock`, byte for byte.
    pub(crate) fn acquire_lock(&self, record: Vec<u8>) -> RemoteCommand {
        let lock = quote(&self.lock_dir());
        let info = quote(&self.lock_info());
        let tmp = format!("{}$$", quote(&format!("{}/lock.tmp.", self.state_dir())));
        let script = [
            format!(
                "set -C; {} && {} > {tmp} || {{ rm -f {tmp}; exit 1; }}; ",
                join(&["mkdir", "-p", &self.state_dir()]),
                join(&["cat"])
            ),
            format!(
                "claim() {{ if err=$(ln {tmp} {info} 2>&1); then return 0; fi; [ -e {info} ] || echo \"$err\" >&2; return 1; }}; "
            ),
            format!(
                "won=; if {} 2>/dev/null; then claim && won=1; ",
                join(&["mkdir", &self.lock_dir()])
            ),
            format!(
                "elif [ ! -e {info} ] && [ -n \"$(find {lock} -maxdepth 0 -mmin +1 2>/dev/null)\" ]; then "
            ),
            format!(
                "if claim; then won=1; {{ echo {} >&2; true; }}; fi; fi; ",
                quote(STALE_LOCK_NOTICE)
            ),
            format!("rm -f {tmp}; [ -n \"$won\" ] && exit 0; "),
            format!(
                "{} 2>/dev/null; exit {LOCK_HELD_EXIT_STATUS}",
                join(&["cat", &self.lock_info()])
            ),
        ]
        .concat();
        RemoteCommand {
            script,
            stdin: Some(record),
            stdin_note: "lock record (holder, time, message)".to_owned(),
            read_only: false,
        }
    }

    /// Prints the record, or `{}` for a lock without one.
    pub(crate) fn lock_status(&self) -> RemoteCommand {
        RemoteCommand::read_only(format!(
            "if [ -d {} ]; then if [ -s {} ]; then {}; else echo '{{}}'; fi; fi",
            quote(&self.lock_dir()),
            quote(&self.lock_info()),
            join(&["cat", &self.lock_info()])
        ))
    }

    /// Removes the lock only while its record is the one on stdin, so a lock
    /// somebody else took after a `--force` release is left alone (exit
    /// [`LOCK_NOT_OURS_EXIT_STATUS`]); a lock that is already gone exits
    /// [`LOCK_GONE_EXIT_STATUS`]. The compare and the `rm` are not atomic;
    /// the window needs a force release and a new acquire inside it.
    pub(crate) fn release_lock(&self, record: Vec<u8>) -> RemoteCommand {
        let lock = quote(&self.lock_dir());
        let info = quote(&self.lock_info());
        RemoteCommand {
            script: format!(
                "record=$(cat) && if [ ! -d {lock} ]; then echo 'lock is already released' >&2; exit {LOCK_GONE_EXIT_STATUS}; elif [ -n \"$record\" ] && [ \"$(cat {info} 2>/dev/null)\" = \"$record\" ]; then rm -rf {lock}; else echo 'lock is held by someone else; left in place' >&2; exit {LOCK_NOT_OURS_EXIT_STATUS}; fi"
            ),
            stdin: Some(record),
            stdin_note: "own lock record (release only if unchanged)".to_owned(),
            read_only: false,
        }
    }

    /// Removes the lock whoever holds it (`lock release --force`).
    pub(crate) fn force_release_lock(&self) -> RemoteCommand {
        RemoteCommand::new(join(&["rm", "-rf", &self.lock_dir()]))
    }

    pub(crate) fn append_log(&self, entry: Vec<u8>) -> RemoteCommand {
        RemoteCommand {
            script: format!(
                "umask 077 && {} && {} >> {}",
                join(&["mkdir", "-p", &self.state_dir()]),
                join(&["cat"]),
                quote(&self.log_file())
            ),
            stdin: Some(entry),
            stdin_note: "deploy log entry".to_owned(),
            read_only: false,
        }
    }

    pub(crate) fn read_log(&self, lines: usize) -> RemoteCommand {
        RemoteCommand::read_only(format!(
            "{} 2>/dev/null || true",
            join(&["tail", "-n", &lines.to_string(), &self.log_file()])
        ))
    }

    /// Selects the newest running container of `role` in `$c`.
    fn latest(&self, role: &str) -> String {
        format!(
            "c=$({}); [ -n \"$c\" ] || {{ echo \"no running {role} container\" >&2; exit 1; }}; ",
            join(&[
                "docker",
                "ps",
                "--latest",
                "--quiet",
                "--filter",
                &format!("label={LABEL_SERVICE}={}", self.cfg.service),
                "--filter",
                &format!("label={LABEL_ROLE}={role}"),
                "--filter",
                "status=running",
            ])
        )
    }

    /// Streams `docker logs` of the current role container. The watchdog
    /// stops `docker logs` when the local side closes stdin (Ctrl-C), even
    /// though no terminal is allocated.
    pub(crate) fn logs(&self, role: &str, follow: bool, since: &str, lines: i64) -> RemoteCommand {
        let mut argv = strings(&["docker", "logs", "--timestamps"]);
        if follow {
            argv.push("--follow".to_owned());
        }
        if !since.is_empty() {
            argv.extend(["--since".to_owned(), since.to_owned()]);
        }
        if lines > 0 {
            argv.extend(["--tail".to_owned(), lines.to_string()]);
        }
        RemoteCommand::read_only(format!(
            "{}exec 3<&0; {} \"$c\" 2>&1 & p=$!; {{ read -r _ <&3; kill \"$p\" 2>/dev/null; }} & w=$!; wait \"$p\"; s=$?; kill \"$w\" 2>/dev/null; exit \"$s\"",
            self.latest(role),
            join(&argv)
        ))
    }

    /// Runs `argv` in a new container of the current image, with the role's
    /// env file and volumes, removed afterwards.
    pub(crate) fn exec_one_off(&self, role: &str, argv: &[String], interactive: bool) -> String {
        let mut run = strings(&["docker", "run", "--rm"]);
        if interactive {
            run.extend(["--interactive".to_owned(), "--tty".to_owned()]);
        }
        run.extend([
            "--network".to_owned(),
            NETWORK_NAME.to_owned(),
            "--env-file".to_owned(),
            self.env_file(role),
        ]);
        run.extend(volume_args(&self.role_volumes(role)));
        format!(
            "{}i=$(docker inspect --format '{{{{.Config.Image}}}}' \"$c\") && {} \"$i\" {}",
            self.latest(role),
            join(&run),
            join(argv)
        )
    }

    /// Runs `argv` inside the running role container.
    pub(crate) fn exec_reuse(&self, role: &str, argv: &[String], interactive: bool) -> String {
        let mut run = strings(&["docker", "exec"]);
        if interactive {
            run.extend(["--interactive".to_owned(), "--tty".to_owned()]);
        }
        format!("{}{} \"$c\" {}", self.latest(role), join(&run), join(argv))
    }

    /// Starts `image` as a sidecar sharing the network and process
    /// namespaces of the running container, without its environment.
    pub(crate) fn debug_shell(&self, role: &str, image: &str) -> String {
        format!(
            "{}{} --network \"container:$c\" --pid \"container:$c\" {}",
            self.latest(role),
            join(&["docker", "run", "--rm", "--interactive", "--tty"]),
            join(&[image, "sh"])
        )
    }
}

/// The `docker --env-file` content. Values are single-line (checked at
/// load) and written verbatim.
fn env_content(
    clear: &BTreeMap<String, String>,
    secret_names: &[String],
    secrets: &Secrets,
) -> Vec<u8> {
    let mut content = String::new();
    for (key, value) in clear {
        content.push_str(&format!("{key}={value}\n"));
    }
    for name in secret_names {
        content.push_str(&format!(
            "{name}={}\n",
            secrets.get(name).unwrap_or_default()
        ));
    }
    content.into_bytes()
}

fn volume_args(volumes: &[String]) -> Vec<String> {
    volumes
        .iter()
        .flat_map(|volume| ["--volume".to_owned(), volume.clone()])
        .collect()
}

/// Sets `$forge_proxies` to the subnets of the forge network, comma
/// separated, and fails when there are none: forge-proxy reaches the
/// containers from an address in them, and replaces any `X-Forwarded-For`
/// a client sent with the client's address.
fn proxy_network_script() -> String {
    format!(
        "forge_proxies=$({}) || exit 1; [ -n \"$forge_proxies\" ] || {{ echo \"docker network {NETWORK_NAME} has no subnet; set {TRUSTED_PROXIES_ENV} in config/deploy.yml\" >&2; exit 1; }}",
        join(&[
            "docker",
            "network",
            "inspect",
            "--format",
            "{{range $i, $c := .IPAM.Config}}{{if $i}},{{end}}{{$c.Subnet}}{{end}}",
            NETWORK_NAME,
        ])
    )
}
