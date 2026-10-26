//! `forge deploy` over real SSH: the system `ssh` client against private
//! `sshd` instances (one per simulated host) started here. Each session
//! runs with a docker stand-in (tests/support/fake_docker.rs) first on
//! PATH and its own home and state directory, so the whole orchestration —
//! quoting, stdin secrets, ControlMaster multiplexing, exit codes and
//! stream cancellation — runs over real SSH. No Docker daemon is involved.
//!
//! Skipped (with a notice) without sshd, ssh, ssh-keygen or the stand-in;
//! `FORGE_TEST_REQUIRE_SSHD=true` turns the skip into a failure. The
//! stand-in is `cargo build -p forge-cli --example fake_docker` (`cargo test`
//! leaves examples under hashed names only); `FORGE_TEST_FAKE_DOCKER` names
//! it when the tests run from another target directory (cargo llvm-cov).

use std::collections::BTreeMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::deployer::{Deployer, Parts};
use super::errors::{EXIT_LOCK_HELD, EXIT_MIGRATION_FAILED, EXIT_UNHEALTHY};
use super::remote::{ExecOptions, LogsOptions};
use super::runner::{Cancel, LocalRunner, SshRunner};
use super::secrets::{REDACTED, Secrets};
use super::tests::write_executable;
use crate::process::Shutdown;

const REGISTRY_PASSWORD: &str = "reg-pass-1234567";
const DATABASE_URL: &str = "postgres://shop:db-secret-9876543@db/shop";
const POSTGRES_PASSWORD: &str = "pg-secret-5555555";

const CONFIG: &str = r#"
service: shop
image: registry.test/acme/shop
registry:
  server: registry.test
  username: acme
  password: REGISTRY_PASSWORD
servers:
  web:
    hosts: [alpha, beta]
  jobs:
    hosts: [beta]
    cmd: [jobs]
env:
  clear:
    FORGE_ENV: production
  secret: [FORGE_DATABASE_URL]
proxy:
  image: registry.test/forge-proxy@sha256:0000000000000000000000000000000000000000000000000000000000000000
  hosts: [shop.example.com]
  app_port: 3000
accessories:
  db:
    image: postgres:18-alpine
    host: beta
    port: "5432"
    env:
      secret: [POSTGRES_PASSWORD]
retain: 2
stop_timeout: 5s
"#;

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .chain(["/usr/sbin", "/usr/local/sbin"].map(PathBuf::from))
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// Skips (or, when required, fails) the test.
fn unavailable(reason: &str) -> Option<Cluster> {
    assert!(
        std::env::var("FORGE_TEST_REQUIRE_SSHD").as_deref() != Ok("true"),
        "FORGE_TEST_REQUIRE_SSHD=true but {reason}"
    );
    eprintln!("skipped: {reason}");
    None
}

struct Daemon {
    child: Child,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Cluster {
    dir: PathBuf,
    ssh_config: PathBuf,
    hosts: BTreeMap<String, (PathBuf, PathBuf)>,
    _daemons: Vec<Daemon>,
}

impl Drop for Cluster {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn run(program: &Path, args: &[&str]) {
    let output = Command::new(program).args(args).output().expect("spawn");
    assert!(
        output.status.success(),
        "{} {args:?}: {}",
        program.display(),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr())
        .map(|address| address.port())
        .expect("free port")
}

fn start_sshd(
    sshd: &Path,
    dir: &Path,
    host_key: &Path,
    authorized: &Path,
    wrapper: &Path,
) -> (Daemon, u16) {
    let mut last = String::new();
    for _ in 0..5 {
        let port = free_port();
        let config = dir.join("sshd_config");
        std::fs::write(
            &config,
            format!(
                "Port {port}\nListenAddress 127.0.0.1\nHostKey {}\nAuthorizedKeysFile {}\nStrictModes no\nUsePAM no\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nPermitRootLogin prohibit-password\nPidFile none\nForceCommand {}\n",
                host_key.display(),
                authorized.display(),
                wrapper.display()
            ),
        )
        .expect("sshd_config");
        let mut child = Command::new(sshd)
            .args(["-D", "-e", "-f"])
            .arg(&config)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start sshd");
        let log = Arc::new(Mutex::new(String::new()));
        let mut stderr = child.stderr.take().expect("stderr");
        let sink = Arc::clone(&log);
        std::thread::spawn(move || {
            let mut buffer = [0_u8; 1024];
            while let Ok(n) = stderr.read(&mut buffer) {
                if n == 0 {
                    break;
                }
                sink.lock()
                    .expect("lock")
                    .push_str(&String::from_utf8_lossy(&buffer[..n]));
            }
        });
        let listening = format!("Server listening on 127.0.0.1 port {port}");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if log.lock().expect("lock").contains(&listening) {
                return (Daemon { child }, port);
            }
            if child.try_wait().ok().flatten().is_some() || Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                break;
            }
            // real-clock: polls a child process (an OS resource a fake clock cannot advance) until a deadline
            std::thread::sleep(Duration::from_millis(20));
        }
        last = log.lock().expect("lock").clone();
    }
    panic!("sshd did not start: {last}");
}

fn start_cluster(names: &[&str]) -> Option<Cluster> {
    let (Some(sshd), Some(_ssh), Some(keygen)) = (which("sshd"), which("ssh"), which("ssh-keygen"))
    else {
        return unavailable("sshd, ssh or ssh-keygen is not installed");
    };
    if rustix::process::geteuid().is_root() && !Path::new("/run/sshd").is_dir() {
        return unavailable("sshd running as root needs /run/sshd (mkdir -m 0755 /run/sshd)");
    }
    let fake_docker = match std::env::var_os("FORGE_TEST_FAKE_DOCKER") {
        Some(path) => Some(PathBuf::from(path)),
        None => std::env::current_exe().ok().and_then(|exe| {
            exe.parent()
                .and_then(Path::parent)
                .map(|dir| dir.join("examples/fake_docker"))
        }),
    }
    .filter(|path| path.is_file());
    let Some(fake_docker) = fake_docker else {
        return unavailable(
            "the fake_docker stand-in is not built \
             (cargo build -p forge-cli --example fake_docker, or set FORGE_TEST_FAKE_DOCKER)",
        );
    };
    // Short: ControlPath sockets are limited to about 100 bytes.
    let mut random = [0_u8; 4];
    getrandom::fill(&mut random).expect("random");
    let dir = PathBuf::from(format!(
        "/tmp/fdi{}",
        random
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    ));
    std::fs::create_dir_all(dir.join("bin")).expect("dir");
    std::os::unix::fs::symlink(&fake_docker, dir.join("bin/docker")).expect("symlink");
    for key in ["hostkey", "userkey"] {
        run(
            &keygen,
            &[
                "-q",
                "-t",
                "ed25519",
                "-N",
                "",
                "-f",
                dir.join(key).to_str().expect("utf-8"),
            ],
        );
    }
    std::fs::copy(dir.join("userkey.pub"), dir.join("authorized_keys")).expect("authorized_keys");
    let host_public = std::fs::read_to_string(dir.join("hostkey.pub")).expect("host key");
    let user = std::env::var("USER").unwrap_or_else(|_| "root".to_owned());
    let mut ssh_config = String::new();
    let mut known_hosts = String::new();
    let mut hosts = BTreeMap::new();
    let mut daemons = Vec::new();
    for name in names {
        let home = dir.join(name).join("home");
        let state = dir.join(name).join("docker");
        std::fs::create_dir_all(&home).expect("home");
        std::fs::create_dir_all(&state).expect("state");
        let wrapper = dir.join(name).join("session.sh");
        write_executable(
            &wrapper,
            &format!(
                "#!/bin/sh\nexport PATH={}:/usr/bin:/bin HOME={} FAKE_DOCKER_STATE={}\ncd \"$HOME\" || exit 1\nexec /bin/sh -c \"$SSH_ORIGINAL_COMMAND\"\n",
                dir.join("bin").display(),
                home.display(),
                state.display()
            ),
        );
        let (daemon, port) = start_sshd(
            &sshd,
            &dir.join(name),
            &dir.join("hostkey"),
            &dir.join("authorized_keys"),
            &wrapper,
        );
        daemons.push(daemon);
        known_hosts.push_str(&format!("[127.0.0.1]:{port} {host_public}"));
        ssh_config.push_str(&format!(
            "Host {name}\n  HostName 127.0.0.1\n  Port {port}\n  User {user}\n  IdentityFile {}\n  IdentitiesOnly yes\n  UserKnownHostsFile {}\n  StrictHostKeyChecking yes\n  LogLevel ERROR\n",
            dir.join("userkey").display(),
            dir.join("known_hosts").display()
        ));
        hosts.insert((*name).to_owned(), (home, state));
    }
    std::fs::write(dir.join("known_hosts"), known_hosts).expect("known_hosts");
    let config_path = dir.join("ssh_config");
    std::fs::write(&config_path, ssh_config).expect("ssh_config");
    Some(Cluster {
        dir,
        ssh_config: config_path,
        hosts,
        _daemons: daemons,
    })
}

#[derive(Clone, Default)]
struct Buffer(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Buffer {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("lock").extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Buffer {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().expect("lock")).into_owned()
    }
    fn writer(&self) -> super::deployer::LogWriter {
        Arc::new(Mutex::new(Box::new(self.clone())))
    }
}

/// Local commands are not part of these tests.
struct NoLocal;

impl LocalRunner for NoLocal {
    fn output(&self, _: &Cancel, _: &Path, argv: &[String]) -> Result<String, String> {
        Err(format!("unexpected local command {argv:?}"))
    }
    fn run(
        &self,
        _: &Cancel,
        _: &Path,
        argv: &[String],
        _: Option<&[u8]>,
        _: &[(String, String)],
    ) -> Result<(), String> {
        Err(format!("unexpected local command {argv:?}"))
    }
}

impl Cluster {
    fn deployer(&self, performer: &str, dry_run: bool) -> (Deployer, Buffer) {
        self.deployer_for(performer, dry_run, CONFIG, &[])
    }

    /// [`Self::deployer`] for another configuration; `extra` secrets are
    /// added to the fixture's.
    fn deployer_for(
        &self,
        performer: &str,
        dry_run: bool,
        config: &str,
        extra: &[(&str, &str)],
    ) -> (Deployer, Buffer) {
        let config = super::config::Config::parse(config.as_bytes()).expect("config");
        let mut runner = SshRunner::new(&config.ssh);
        runner.config_file = Some(self.ssh_config.clone());
        let log = Buffer::default();
        let deployer = Deployer::new(Parts {
            config,
            secrets: Secrets::from_values(
                [
                    ("REGISTRY_PASSWORD", REGISTRY_PASSWORD),
                    ("FORGE_DATABASE_URL", DATABASE_URL),
                    ("POSTGRES_PASSWORD", POSTGRES_PASSWORD),
                ]
                .into_iter()
                .chain(extra.iter().copied()),
            ),
            runner: Box::new(runner),
            local: Box::new(NoLocal),
            root: self.dir.clone(),
            log: log.writer(),
            performer: Some(performer.to_owned()),
            now: None,
            suffix: None,
            dry_run,
        });
        (deployer, log)
    }

    fn state(&self, host: &str) -> serde_json::Value {
        let path = self.hosts[host].1.join("state.json");
        serde_json::from_slice(&std::fs::read(path).expect("state")).expect("json")
    }

    fn home(&self, host: &str) -> &Path {
        &self.hosts[host].0
    }
}

/// role → version → state.
fn versions(state: &serde_json::Value, role: &str) -> BTreeMap<String, String> {
    state["containers"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| c["labels"]["forge.role"] == role)
        .map(|c| {
            (
                c["labels"]["forge.version"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
                c["state"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect()
}

fn route_version(state: &serde_json::Value) -> String {
    let target = state["routes"]["shop"].as_str().unwrap_or_default();
    state["containers"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|c| target.starts_with(&format!("{}:", c["name"].as_str().unwrap_or("?"))))
        .map(|c| {
            c["labels"]["forge.version"]
                .as_str()
                .unwrap_or_default()
                .to_owned()
        })
        .unwrap_or_else(|| panic!("route {target:?} points at no container"))
}

#[test]
#[allow(clippy::too_many_lines)]
fn deploys_over_real_ssh() {
    let Some(cluster) = start_cluster(&["alpha", "beta"]) else {
        return;
    };
    let cancel = Cancel::never();
    let (deployer, log) = cluster.deployer("alice@laptop", false);

    // setup
    let (_, result) = deployer.setup(&cancel);
    result.unwrap_or_else(|error| panic!("setup: {error}\n{}", log.text()));
    let (alpha, beta) = (cluster.state("alpha"), cluster.state("beta"));
    let has_proxy = |state: &serde_json::Value| {
        state["containers"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|c| c["name"] == "forge-proxy" && c["state"] == "running")
    };
    assert!(has_proxy(&alpha) && has_proxy(&beta), "{alpha} {beta}");
    assert_eq!(alpha["networks"].as_array().map(Vec::len), Some(1));
    let db = beta["containers"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|c| c["name"] == "shop-db")
        .expect("accessory");
    assert_eq!(db["publish"][0], "127.0.0.1:5432:5432");
    assert!(
        beta["env_seen"]["shop-db"]
            .as_str()
            .unwrap_or_default()
            .contains(&format!("POSTGRES_PASSWORD={POSTGRES_PASSWORD}"))
    );
    assert_eq!(
        alpha["logins"][0],
        format!("acme password={REGISTRY_PASSWORD}"),
        "registry password not delivered on stdin"
    );
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(cluster.home("alpha").join(".forge/shop"))
            .expect("state dir")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
    }
    deployer.setup(&cancel).1.expect("setup is idempotent");

    // deploy twice
    for version in ["v1", "v2"] {
        let (report, result) = deployer.deploy(&cancel, version, true);
        result.unwrap_or_else(|error| panic!("deploy {version}: {error}\n{}", log.text()));
        assert_eq!(report.expect("report").steps.len(), 3);
    }
    let (alpha, beta) = (cluster.state("alpha"), cluster.state("beta"));
    for state in [&alpha, &beta] {
        assert_eq!(route_version(state), "v2");
        let web = versions(state, "web");
        assert_eq!(
            (web["v1"].as_str(), web["v2"].as_str()),
            ("exited", "running")
        );
    }
    let jobs = versions(&beta, "jobs");
    assert_eq!(
        (jobs["v1"].as_str(), jobs["v2"].as_str()),
        ("exited", "running")
    );
    for container in beta["containers"].as_array().into_iter().flatten() {
        match container["labels"]["forge.role"].as_str() {
            Some("jobs") => {
                assert_eq!(container["args"], serde_json::json!(["jobs"]));
                assert!(
                    container["env"].get("FORGE_HTTP_TRUSTED_PROXIES").is_none(),
                    "jobs trusts proxies"
                );
            }
            Some("web") => assert_eq!(
                container["env"]["FORGE_HTTP_TRUSTED_PROXIES"],
                "172.18.0.0/16"
            ),
            _ => {}
        }
    }
    let one_offs = alpha["one_offs"].as_array().map(Vec::len);
    assert_eq!(one_offs, Some(2), "migrations ran {:?}", alpha["one_offs"]);
    assert_eq!(alpha["one_offs"][1][1], "migrate");
    assert!(
        beta["one_offs"].as_array().is_none_or(Vec::is_empty),
        "migration ran on beta"
    );
    let env_file = cluster.home("beta").join(".forge/shop/env/web.env");
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            std::fs::metadata(&env_file)
                .expect("env")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    assert_eq!(
        std::fs::read_to_string(&env_file).expect("env"),
        format!("FORGE_ENV=production\nFORGE_DATABASE_URL={DATABASE_URL}\n")
    );
    let history = std::fs::read_to_string(cluster.home("alpha").join(".forge/shop/deploys.log"))
        .expect("history");
    assert_eq!(history.lines().count(), 2);
    assert!(
        history.contains("\"performer\":\"alice@laptop\""),
        "{history}"
    );
    assert!(
        !cluster.home("alpha").join(".forge/shop/lock").exists(),
        "lock not released"
    );

    // migration failure aborts before any switch
    let before = cluster.state("alpha")["containers"]
        .as_array()
        .map(Vec::len);
    let error = deployer
        .deploy(&cancel, "v3-badmigrate", true)
        .1
        .expect_err("bad migration");
    assert_eq!(error.exit_code(), EXIT_MIGRATION_FAILED, "{error}");
    assert!(
        error.message.contains("migration 0002_add_index failed"),
        "{error}"
    );
    for host in ["alpha", "beta"] {
        let state = cluster.state(host);
        assert!(!versions(&state, "web").contains_key("v3-badmigrate"));
        assert_eq!(route_version(&state), "v2");
    }
    assert_eq!(
        cluster.state("alpha")["containers"]
            .as_array()
            .map(Vec::len),
        before
    );
    assert!(!cluster.home("beta").join(".forge/shop/lock").exists());

    // a health failure on one host reverts every host
    let marker = cluster.hosts["beta"].1.join("unhealthy");
    std::fs::write(&marker, b"").expect("marker");
    let error = deployer
        .deploy(&cancel, "v4", true)
        .1
        .expect_err("unhealthy");
    std::fs::remove_file(&marker).expect("marker");
    assert_eq!(error.exit_code(), EXIT_UNHEALTHY, "{error}\n{}", log.text());
    assert!(error.message.contains("previous version"), "{error}");
    for host in ["alpha", "beta"] {
        let state = cluster.state(host);
        assert_eq!(route_version(&state), "v2", "{host} after revert");
        for role in ["web", "jobs"] {
            let versions = versions(&state, role);
            assert!(
                !versions.contains_key("v4"),
                "{host} kept a v4 {role}: {versions:?}"
            );
            if role == "web" || host == "beta" {
                assert_eq!(versions["v2"], "running", "{host} {role}");
            }
        }
    }
    assert!(
        log.text().contains("traffic switched back to shop-web-v2-"),
        "{}",
        log.text()
    );

    // lock contention
    let (other, _) = cluster.deployer("bob@ci", false);
    other
        .lock_acquire(&cancel, "maintenance window")
        .expect("bob locks");
    let error = deployer.deploy(&cancel, "v5", true).1.expect_err("locked");
    assert_eq!(error.exit_code(), EXIT_LOCK_HELD, "{error}");
    assert!(error.message.contains("bob@ci"), "{error}");
    assert!(!versions(&cluster.state("alpha"), "web").contains_key("v5"));
    let locks = deployer.lock_status(&cancel).expect("status");
    assert_eq!(locks.len(), 2);
    assert_eq!(locks["alpha"].message, "maintenance window");
    assert_eq!(
        deployer
            .lock_release(&cancel, false)
            .expect_err("not mine")
            .exit_code(),
        EXIT_LOCK_HELD
    );
    deployer.lock_release(&cancel, true).expect("forced");
    assert!(deployer.lock_status(&cancel).expect("status").is_empty());

    // rollback
    deployer
        .rollback(&cancel, "v1")
        .1
        .unwrap_or_else(|error| panic!("rollback: {error}\n{}", log.text()));
    for host in ["alpha", "beta"] {
        let state = cluster.state(host);
        assert_eq!(route_version(&state), "v1");
        let web = versions(&state, "web");
        assert_eq!(
            (web["v1"].as_str(), web["v2"].as_str()),
            ("running", "exited")
        );
    }
    let jobs = versions(&cluster.state("beta"), "jobs");
    assert_eq!(
        (jobs["v1"].as_str(), jobs["v2"].as_str()),
        ("running", "exited")
    );
    let error = deployer
        .rollback(&cancel, "v9")
        .1
        .expect_err("unknown version");
    assert!(error.message.contains("not retained"), "{error}");

    // deploy prunes beyond retain
    let (report, result) = deployer.deploy(&cancel, "v5", true);
    result.expect("deploy v5");
    let alpha = cluster.state("alpha");
    let web = versions(&alpha, "web");
    assert_eq!(
        web.len(),
        2,
        "retain 2 kept {web:?} (pruned {:?})",
        report.map(|r| r.pruned)
    );
    assert_eq!(web["v5"], "running");
    assert!(
        !web.contains_key("v1"),
        "oldest version not pruned: {web:?}"
    );
    assert!(
        !alpha["images"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|i| i == "registry.test/acme/shop:v1")
    );
    deployer.prune(&cancel).1.expect("prune");

    // logs: grep, prefixes and redaction
    let out = Buffer::default();
    deployer
        .logs(
            &cancel,
            out.writer(),
            LogsOptions {
                grep: "booted|connecting".into(),
                ..LogsOptions::default()
            },
        )
        .expect("logs");
    let text = out.text();
    for host in ["alpha", "beta"] {
        assert!(
            text.contains(&format!("{host}|2026-10-19T10:00:00Z booted shop-web-v5-")),
            "{text}"
        );
    }
    assert!(!text.contains("ERROR"), "grep did not filter:\n{text}");
    assert!(
        !text.contains("db-secret") && text.contains(REDACTED),
        "secret not redacted:\n{text}"
    );

    // logs --follow stops the remote processes on cancel
    let shutdown = Shutdown::new();
    let follow_cancel = Cancel::signal(shutdown.clone());
    let out = Buffer::default();
    std::thread::scope(|scope| {
        let streaming = scope.spawn(|| {
            deployer.logs(
                &follow_cancel,
                out.writer(),
                LogsOptions {
                    follow: true,
                    ..LogsOptions::default()
                },
            )
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        while out.text().matches("ERROR something failed").count() < 2 {
            assert!(
                Instant::now() < deadline,
                "no output from both hosts:\n{}",
                out.text()
            );
            // real-clock: polls the logs of a task or process that runs on its own threads, until a deadline
            std::thread::sleep(Duration::from_millis(20));
        }
        shutdown.request();
        let started = Instant::now();
        streaming
            .join()
            .expect("join")
            .expect("follow ends cleanly");
        assert!(started.elapsed() < Duration::from_secs(10));
    });
    for host in ["alpha", "beta"] {
        let state_dir = &cluster.hosts[host].1;
        let pids: Vec<i32> = std::fs::read_dir(state_dir)
            .expect("state")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().starts_with("logs-"))
            .filter_map(|entry| {
                std::fs::read_to_string(entry.path())
                    .ok()?
                    .trim()
                    .parse()
                    .ok()
            })
            .collect();
        assert!(!pids.is_empty(), "{host}: no follow process recorded");
        for pid in pids {
            let pid = rustix::process::Pid::from_raw(pid).expect("pid");
            let mut gone = false;
            for _ in 0..100 {
                if rustix::process::test_kill_process(pid).is_err() {
                    gone = true;
                    break;
                }
                // real-clock: polls a child process (an OS resource a fake clock cannot advance) until a deadline
                std::thread::sleep(Duration::from_millis(50));
            }
            if !gone {
                let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
                panic!("{host}: remote docker logs still running after cancel");
            }
        }
    }

    // app exec, one-off and in the running container
    let out = Buffer::default();
    deployer
        .exec(
            &cancel,
            out.writer(),
            &ExecOptions {
                argv: vec!["migrate".into(), "status".into()],
                ..ExecOptions::default()
            },
        )
        .expect("exec");
    assert_eq!(out.text().trim(), "ran migrate status");
    let one_offs = cluster.state("alpha")["one_offs"].clone();
    let last = one_offs
        .as_array()
        .and_then(|list| list.last())
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        last,
        serde_json::json!(["registry.test/acme/shop:v5", "migrate", "status"])
    );
    let out = Buffer::default();
    deployer
        .exec(
            &cancel,
            out.writer(),
            &ExecOptions {
                role: "jobs".into(),
                reuse: true,
                argv: vec!["/app/server".into(), "it's quoted".into()],
                ..ExecOptions::default()
            },
        )
        .expect("exec --reuse");
    let text = out.text();
    assert!(
        text.contains("exec in shop-jobs-v5-") && text.contains("/app/server it's quoted"),
        "{text}"
    );

    // details
    let (details, result) = deployer.details(&cancel);
    result.expect("details");
    assert_eq!(details.len(), 2);
    let proxy = details[0].proxy.as_ref().expect("proxy status");
    assert!(
        proxy[0]["target"]
            .as_str()
            .unwrap_or_default()
            .starts_with("shop-web-v5-")
    );
    assert!(!details[1].recent_deploys.is_empty());

    // a dry run changes nothing and names real containers
    let before = cluster.state("alpha");
    let (dry, plan) = cluster.deployer("alice@laptop", true);
    dry.deploy(&cancel, "v6", true).1.expect("dry run");
    let after = cluster.state("alpha");
    assert_eq!(
        after["containers"].as_array().map(Vec::len),
        before["containers"].as_array().map(Vec::len)
    );
    assert_eq!(route_version(&after), "v5");
    let text = plan.text();
    assert!(
        text.contains("docker stop -t 5 shop-web-v5-")
            && text.contains("forge-proxy deploy -service shop"),
        "{text}"
    );
    assert!(
        !text.contains(DATABASE_URL) && !text.contains(REGISTRY_PASSWORD),
        "plan leaks a secret"
    );

    // secrets never appear in remote command lines
    for host in ["alpha", "beta"] {
        let calls =
            std::fs::read_to_string(cluster.hosts[host].1.join("calls.log")).expect("calls");
        for secret in [REGISTRY_PASSWORD, DATABASE_URL, POSTGRES_PASSWORD] {
            assert!(!calls.contains(secret), "{host}: secret in docker argv");
        }
    }
    assert!(!log.text().contains("db-secret"), "secret in progress log");
}

/// A deploy with `migrate.env` over real SSH: only the migrate container
/// sees the schema owner's credential, through a 0600 file that is gone
/// afterwards, and no serving container or env file has it.
#[test]
fn migrate_env_reaches_only_the_migrate_container_over_real_ssh() {
    const MIGRATION_URL: &str = "postgres://shop_migrator:migration-secret-7654321@db/shop";
    let Some(cluster) = start_cluster(&["alpha", "beta"]) else {
        return;
    };
    let config = format!("{CONFIG}migrate:\n  env:\n    secret: [FORGE_MIGRATION_DATABASE_URL]\n");
    let (deployer, log) = cluster.deployer_for(
        "alice@laptop",
        false,
        &config,
        &[("FORGE_MIGRATION_DATABASE_URL", MIGRATION_URL)],
    );
    let cancel = Cancel::never();
    let (_, result) = deployer.setup(&cancel);
    result.unwrap_or_else(|error| panic!("setup: {error}\n{}", log.text()));
    let (_, result) = deployer.deploy(&cancel, "v1", true);
    result.unwrap_or_else(|error| panic!("deploy: {error}\n{}", log.text()));

    let (alpha, beta) = (cluster.state("alpha"), cluster.state("beta"));
    assert_eq!(alpha["one_offs"].as_array().map(Vec::len), Some(1));
    assert_eq!(alpha["one_offs"][0][1], "migrate");
    assert!(
        beta["one_offs"].as_array().is_none_or(Vec::is_empty),
        "migration ran on beta"
    );
    let mut migrate_env = String::new();
    for (name, content) in alpha["env_seen"].as_object().into_iter().flatten() {
        let content = content.as_str().unwrap_or_default();
        let is_migrate = name.starts_with("shop-migrate-");
        if is_migrate {
            content.clone_into(&mut migrate_env);
        }
        assert_eq!(
            content.contains(MIGRATION_URL),
            is_migrate,
            "{name}: holds the migration credential"
        );
    }
    for (name, content) in beta["env_seen"].as_object().into_iter().flatten() {
        assert!(
            !content.as_str().unwrap_or_default().contains(MIGRATION_URL),
            "{name} holds the migration credential"
        );
    }
    assert_eq!(
        migrate_env,
        format!(
            "FORGE_ENV=production\nFORGE_DATABASE_URL={DATABASE_URL}\nFORGE_MIGRATION_DATABASE_URL={MIGRATION_URL}\n"
        )
    );
    // Deleted after the step, with its temporary copy, on every host.
    for host in ["alpha", "beta"] {
        let directory = cluster.home(host).join(".forge/shop/env");
        for entry in std::fs::read_dir(&directory).expect("env directory") {
            let entry = entry.expect("entry");
            let name = entry.file_name().to_string_lossy().into_owned();
            let content = std::fs::read_to_string(entry.path()).unwrap_or_default();
            assert!(
                !name.contains("migrate") && !content.contains(MIGRATION_URL),
                "{host}: {name} remains on the host"
            );
        }
    }
    // Never on a command line, in the progress log or in a stored call.
    assert!(
        !log.text().contains("migration-secret-7654321"),
        "{}",
        log.text()
    );
    for host in ["alpha", "beta"] {
        let calls =
            std::fs::read_to_string(cluster.hosts[host].1.join("calls.log")).expect("calls");
        assert!(
            !calls.contains("migration-secret"),
            "{host}: secret in docker argv"
        );
    }
}
