//! `config/deploy.yml`: the same schema, defaults and validation as
//! Forge-Go's `internal/deploy/config.go`, so a project can switch
//! frameworks without rewriting it.
//!
//! Decoding is strict: unknown and duplicate keys, several documents,
//! durations that are not strings, tags or digests in `image`, inline
//! registry passwords, option-like hosts and more than one proxied role are
//! errors. Defaults are applied once, in [`Config::parse`].

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::sync::LazyLock;

use forge_proxy::GoDuration;
use regex_automata::meta::Regex;

use super::yaml::{self, Node, Tag, Value};

/// Where `forge deploy` looks for its configuration.
pub(crate) const DEFAULT_CONFIG_PATH: &str = "config/deploy.yml";

pub(crate) const DEFAULT_RETAIN: i64 = 5;
/// `retain` counts the running version, so 1 would prune the previous one
/// after every deploy and leave nothing to roll back to.
pub(crate) const MIN_RETAIN: i64 = 2;
pub(crate) const DEFAULT_APP_PORT: i64 = 8080;
pub(crate) const DEFAULT_PARALLELISM: i64 = 4;
pub(crate) const DEFAULT_HOOKS_DIR: &str = ".forge/hooks";
pub(crate) const DEFAULT_HEALTH_PATH: &str = "/health/ready";
pub(crate) const DEFAULT_HEALTHY_COUNT: i64 = 3;
pub(crate) const DEFAULT_MAX_BODY_BYTES: i64 = 64 << 20;

/// The patterns Forge-Go validates with (RE2 syntax, quoted in messages).
pub(crate) mod patterns {
    pub(crate) const SERVICE: &str = "^[a-z][a-z0-9-]{0,39}$";
    pub(crate) const ROLE: &str = "^[a-z][a-z0-9-]{0,19}$";
    pub(crate) const ENV_KEY: &str = "^[A-Za-z_][A-Za-z0-9_]*$";
    pub(crate) const IMAGE: &str = "^[a-z0-9][a-z0-9._/:-]{0,254}$";
    pub(crate) const IMAGE_REF: &str = "^[a-z0-9][a-z0-9._/:@-]{0,254}$";
    pub(crate) const SSH_HOST: &str = "^[A-Za-z0-9_][A-Za-z0-9._:%-]{0,252}$";
    pub(crate) const USER: &str = "^[a-z_][a-z0-9_.-]{0,31}$";
    pub(crate) const DNS_LABEL: &str = "^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$";
    pub(crate) const VOLUME: &str = "^[A-Za-z0-9_./-]+:/[A-Za-z0-9_./-]*(:(ro|rw))?$";
    pub(crate) const HEALTH_PATH: &str = "^/[A-Za-z0-9._~/-]*$";
    pub(crate) const EMAIL: &str = r"^[^@\s]+@[^@\s]+$";
    pub(crate) const REGISTRY_HOST: &str = "^[A-Za-z0-9][A-Za-z0-9.-]*(:[0-9]{1,5})?$";
    pub(crate) const PLATFORM: &str =
        "^[a-z0-9]+/[a-z0-9]+(/[a-z0-9]+)?(,[a-z0-9]+/[a-z0-9]+(/[a-z0-9]+)?)*$";
    pub(crate) const ARG: &str = "^[^\\x00-\\x1f]*$";
    pub(crate) const PROXY_JUMP: &str =
        "^([a-z_][a-z0-9_.-]*@)?[A-Za-z0-9_][A-Za-z0-9._-]*(:[0-9]{1,5})?$";
    pub(crate) const VERSION: &str = "^[A-Za-z0-9_][A-Za-z0-9_.-]{0,63}$";
    pub(crate) const SINCE: &str = "^[0-9A-Za-z:.+-]{1,40}$";
    pub(crate) const PASSTHROUGH: &str = r"^\$(\{[A-Za-z_][A-Za-z0-9_]*\}|[A-Za-z_][A-Za-z0-9_]*)$";
}

/// A compiled, anchored validation pattern and its source text (quoted in
/// error messages, as Forge-Go does).
pub(crate) struct Pattern {
    source: &'static str,
    regex: LazyLock<Option<Regex>>,
}

impl Pattern {
    /// Whether `text` matches. A pattern that failed to compile matches
    /// nothing, so validation fails closed (the tests compile every one).
    pub(crate) fn matches(&self, text: &str) -> bool {
        self.regex
            .as_ref()
            .is_some_and(|regex| regex.is_match(text))
    }

    pub(crate) const fn source(&self) -> &'static str {
        self.source
    }
}

macro_rules! pattern {
    ($visibility:vis $name:ident = $source:path) => {
        $visibility static $name: Pattern = Pattern {
            source: $source,
            regex: LazyLock::new(|| Regex::new($source).ok()),
        };
    };
}

pattern!(pub(crate) SERVICE_NAME = patterns::SERVICE);
pattern!(pub(crate) ROLE_NAME = patterns::ROLE);
pattern!(pub(crate) ENV_KEY = patterns::ENV_KEY);
pattern!(IMAGE = patterns::IMAGE);
pattern!(pub(crate) IMAGE_REF = patterns::IMAGE_REF);
pattern!(SSH_HOST = patterns::SSH_HOST);
pattern!(USER = patterns::USER);
pattern!(DNS_LABEL = patterns::DNS_LABEL);
pattern!(VOLUME = patterns::VOLUME);
pattern!(HEALTH_PATH = patterns::HEALTH_PATH);
pattern!(EMAIL = patterns::EMAIL);
pattern!(REGISTRY_HOST = patterns::REGISTRY_HOST);
pattern!(PLATFORM = patterns::PLATFORM);
pattern!(pub(crate) ARG = patterns::ARG);
pattern!(PROXY_JUMP = patterns::PROXY_JUMP);
pattern!(pub(crate) VERSION = patterns::VERSION);
pattern!(pub(crate) SINCE = patterns::SINCE);
pattern!(pub(crate) PASSTHROUGH = patterns::PASSTHROUGH);

/// Every pattern, for the test that compiles them all.
#[cfg(test)]
pub(crate) fn all_patterns() -> [&'static Pattern; 18] {
    [
        &SERVICE_NAME,
        &ROLE_NAME,
        &ENV_KEY,
        &IMAGE,
        &IMAGE_REF,
        &SSH_HOST,
        &USER,
        &DNS_LABEL,
        &VOLUME,
        &HEALTH_PATH,
        &EMAIL,
        &REGISTRY_HOST,
        &PLATFORM,
        &ARG,
        &PROXY_JUMP,
        &VERSION,
        &SINCE,
        &PASSTHROUGH,
    ]
}

/// `config/deploy.yml`.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Config {
    pub(crate) service: String,
    pub(crate) image: String,
    pub(crate) registry: Registry,
    pub(crate) servers: BTreeMap<String, Role>,
    pub(crate) env: Env,
    pub(crate) volumes: Vec<String>,
    pub(crate) proxy: Proxy,
    pub(crate) migrate: Migrate,
    pub(crate) accessories: BTreeMap<String, Accessory>,
    pub(crate) builder: Builder,
    pub(crate) retain: i64,
    pub(crate) stop_timeout: GoDuration,
    pub(crate) boot_timeout: GoDuration,
    pub(crate) hooks: String,
    pub(crate) ssh: Ssh,
    pub(crate) parallelism: i64,
}

/// The image registry credentials; `password` names a secret.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Registry {
    pub(crate) server: String,
    pub(crate) username: String,
    pub(crate) password: String,
}

/// A group of hosts running the image with the same command.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Role {
    pub(crate) hosts: Vec<String>,
    pub(crate) cmd: Vec<String>,
    pub(crate) proxy: Option<bool>,
    pub(crate) env: Env,
    pub(crate) volumes: Vec<String>,
}

/// Clear values and the names of secrets read from `.forge/secrets`.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Env {
    pub(crate) clear: BTreeMap<String, String>,
    pub(crate) secret: Vec<String>,
}

/// forge-proxy for the proxied role.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Proxy {
    pub(crate) image: String,
    pub(crate) hosts: Vec<String>,
    pub(crate) ssl: bool,
    pub(crate) acme_email: String,
    pub(crate) app_port: i64,
    pub(crate) healthcheck: Healthcheck,
    pub(crate) drain_timeout: GoDuration,
    pub(crate) response_timeout: GoDuration,
    pub(crate) max_body_bytes: i64,
}

/// The gate before traffic reaches a new container.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Healthcheck {
    pub(crate) path: String,
    pub(crate) interval: GoDuration,
    pub(crate) timeout: GoDuration,
    pub(crate) healthy_count: i64,
    pub(crate) deploy_timeout: GoDuration,
}

/// The image's migrate command, run once per deploy before any switch.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Migrate {
    pub(crate) enabled: Option<bool>,
    pub(crate) cmd: Vec<String>,
    pub(crate) role: String,
}

/// A long-lived supporting container such as a database.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Accessory {
    pub(crate) image: String,
    pub(crate) host: String,
    pub(crate) port: String,
    pub(crate) volumes: Vec<String>,
    pub(crate) env: Env,
    pub(crate) cmd: Vec<String>,
}

/// The local image build.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Builder {
    pub(crate) context: String,
    pub(crate) dockerfile: String,
    pub(crate) platform: String,
    pub(crate) args: BTreeMap<String, String>,
}

/// The system ssh client; unset values defer to `~/.ssh/config`.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Ssh {
    pub(crate) user: String,
    pub(crate) port: i64,
    pub(crate) proxy_jump: String,
}

/// Collects decoding errors like yaml.v3: type errors are gathered, a
/// duration error ends decoding.
#[derive(Default)]
struct Decoder {
    errors: Vec<String>,
    fatal: Option<String>,
}

/// yaml.v3's short description of a node in type errors.
fn describe(node: &Node) -> String {
    match &node.value {
        Value::Null => "!!null".to_owned(),
        Value::Seq(_) => "!!seq".to_owned(),
        Value::Map(_) => "!!map".to_owned(),
        Value::Scalar { text, plain } => {
            let tag = if *plain {
                match yaml::resolve(text) {
                    Tag::Null => "!!null",
                    Tag::Bool => "!!bool",
                    Tag::Int => "!!int",
                    Tag::Float => "!!float",
                    Tag::Str => "!!str",
                }
            } else {
                "!!str"
            };
            let shown = if text.chars().count() > 10 {
                format!("{}...", text.chars().take(7).collect::<String>())
            } else {
                text.clone()
            };
            format!("{tag} `{shown}`")
        }
    }
}

impl Decoder {
    fn type_error(&mut self, node: &Node, target: &str) {
        self.errors.push(format!(
            "line {}: cannot unmarshal {} into {target}",
            node.line,
            describe(node)
        ));
    }

    fn string(&mut self, node: &Node) -> String {
        match &node.value {
            Value::Null => String::new(),
            Value::Scalar { text, .. } => text.clone(),
            _ => {
                self.type_error(node, "string");
                String::new()
            }
        }
    }

    fn strings(&mut self, node: &Node) -> Vec<String> {
        match &node.value {
            Value::Null => Vec::new(),
            Value::Seq(items) => items.iter().map(|item| self.string(item)).collect(),
            _ => {
                self.type_error(node, "[]string");
                Vec::new()
            }
        }
    }

    fn string_map(&mut self, node: &Node) -> BTreeMap<String, String> {
        let mut map = BTreeMap::new();
        match &node.value {
            Value::Null => {}
            Value::Map(entries) => {
                for (key, value) in entries {
                    let key = self.string(key);
                    let value = self.string(value);
                    map.insert(key, value);
                }
            }
            _ => self.type_error(node, "map[string]string"),
        }
        map
    }

    fn int(&mut self, node: &Node, target: &str) -> i64 {
        match &node.value {
            Value::Null => 0,
            Value::Scalar { text, plain: true } => match yaml::resolve(text) {
                Tag::Int => yaml::parse_go_int(&text.replace('_', "")).unwrap_or(0),
                // yaml.v3 truncates a float that fits.
                Tag::Float => match text.replace('_', "").parse::<f64>() {
                    #[allow(clippy::cast_possible_truncation)]
                    Ok(value) if value.is_finite() && value.abs() < 9.2e18 => value as i64,
                    _ => {
                        self.type_error(node, target);
                        0
                    }
                },
                _ => {
                    self.type_error(node, target);
                    0
                }
            },
            _ => {
                self.type_error(node, target);
                0
            }
        }
    }

    fn optional_bool(&mut self, node: &Node) -> Option<bool> {
        match &node.value {
            Value::Null => None,
            Value::Scalar { text, plain } => {
                if *plain && yaml::resolve(text) == Tag::Bool {
                    return Some(text.eq_ignore_ascii_case("true"));
                }
                // yaml.v3 accepts YAML 1.1 booleans for typed bool fields.
                match text.as_str() {
                    "y" | "Y" | "yes" | "Yes" | "YES" | "on" | "On" | "ON" => Some(true),
                    "n" | "N" | "no" | "No" | "NO" | "off" | "Off" | "OFF" => Some(false),
                    _ => {
                        self.type_error(node, "bool");
                        None
                    }
                }
            }
            _ => {
                self.type_error(node, "bool");
                None
            }
        }
    }

    fn duration(&mut self, node: &Node) -> GoDuration {
        let is_string = match &node.value {
            Value::Scalar { text, plain } => !*plain || yaml::resolve(text) == Tag::Str,
            Value::Null => return GoDuration::ZERO,
            _ => false,
        };
        let text = match &node.value {
            Value::Scalar { text, .. } if is_string => text,
            _ => {
                self.fatal.get_or_insert_with(|| {
                    format!(
                        "line {}: duration must be a string such as \"30s\"",
                        node.line
                    )
                });
                return GoDuration::ZERO;
            }
        };
        if let Ok(duration) = GoDuration::parse(text) {
            duration
        } else {
            self.fatal
                .get_or_insert_with(|| format!("line {}: invalid duration {text:?}", node.line));
            GoDuration::ZERO
        }
    }

    /// Visits the fields of a struct-typed mapping.
    fn fields(
        &mut self,
        node: &Node,
        type_name: &str,
        mut field: impl FnMut(&mut Self, &str, &Node) -> bool,
    ) {
        match &node.value {
            Value::Null => {}
            Value::Map(entries) => {
                for (key, value) in entries {
                    if self.fatal.is_some() {
                        return;
                    }
                    let name = match &key.value {
                        Value::Scalar { text, .. } => text.clone(),
                        _ => String::new(),
                    };
                    if !field(self, &name, value) {
                        self.errors.push(format!(
                            "line {}: field {name} not found in type deploy.{type_name}",
                            key.line
                        ));
                    }
                }
            }
            _ => self.type_error(node, &format!("deploy.{type_name}")),
        }
    }

    fn env(&mut self, node: &Node) -> Env {
        let mut env = Env::default();
        self.fields(node, "Env", |d, name, value| {
            match name {
                "clear" => env.clear = d.string_map(value),
                "secret" => env.secret = d.strings(value),
                _ => return false,
            }
            true
        });
        env
    }

    fn config(&mut self, node: &Node) -> Config {
        let mut config = Config::default();
        self.fields(node, "Config", |d, name, value| {
            match name {
                "service" => config.service = d.string(value),
                "image" => config.image = d.string(value),
                "registry" => {
                    d.fields(value, "Registry", |d, name, value| {
                        match name {
                            "server" => config.registry.server = d.string(value),
                            "username" => config.registry.username = d.string(value),
                            "password" => config.registry.password = d.string(value),
                            _ => return false,
                        }
                        true
                    });
                }
                "servers" => match &value.value {
                    Value::Null => {}
                    Value::Map(entries) => {
                        for (key, role_node) in entries {
                            let role_name = d.string(key);
                            let mut role = Role::default();
                            d.fields(role_node, "Role", |d, name, value| {
                                match name {
                                    "hosts" => role.hosts = d.strings(value),
                                    "cmd" => role.cmd = d.strings(value),
                                    "proxy" => role.proxy = d.optional_bool(value),
                                    "env" => role.env = d.env(value),
                                    "volumes" => role.volumes = d.strings(value),
                                    _ => return false,
                                }
                                true
                            });
                            config.servers.insert(role_name, role);
                        }
                    }
                    _ => d.type_error(value, "map[string]deploy.Role"),
                },
                "env" => config.env = d.env(value),
                "volumes" => config.volumes = d.strings(value),
                "proxy" => {
                    let proxy = &mut config.proxy;
                    d.fields(value, "Proxy", |d, name, value| {
                        match name {
                            "image" => proxy.image = d.string(value),
                            "hosts" => proxy.hosts = d.strings(value),
                            "ssl" => proxy.ssl = d.optional_bool(value).unwrap_or(false),
                            "acme_email" => proxy.acme_email = d.string(value),
                            "app_port" => proxy.app_port = d.int(value, "int"),
                            "healthcheck" => {
                                let health = &mut proxy.healthcheck;
                                d.fields(value, "Healthcheck", |d, name, value| {
                                    match name {
                                        "path" => health.path = d.string(value),
                                        "interval" => health.interval = d.duration(value),
                                        "timeout" => health.timeout = d.duration(value),
                                        "healthy_count" => {
                                            health.healthy_count = d.int(value, "int")
                                        }
                                        "deploy_timeout" => {
                                            health.deploy_timeout = d.duration(value)
                                        }
                                        _ => return false,
                                    }
                                    true
                                });
                            }
                            "drain_timeout" => proxy.drain_timeout = d.duration(value),
                            "response_timeout" => proxy.response_timeout = d.duration(value),
                            "max_body_bytes" => proxy.max_body_bytes = d.int(value, "int64"),
                            _ => return false,
                        }
                        true
                    });
                }
                "migrate" => {
                    let migrate = &mut config.migrate;
                    d.fields(value, "Migrate", |d, name, value| {
                        match name {
                            "enabled" => migrate.enabled = d.optional_bool(value),
                            "cmd" => migrate.cmd = d.strings(value),
                            "role" => migrate.role = d.string(value),
                            _ => return false,
                        }
                        true
                    });
                }
                "accessories" => match &value.value {
                    Value::Null => {}
                    Value::Map(entries) => {
                        for (key, accessory_node) in entries {
                            let accessory_name = d.string(key);
                            let mut accessory = Accessory::default();
                            d.fields(accessory_node, "Accessory", |d, name, value| {
                                match name {
                                    "image" => accessory.image = d.string(value),
                                    "host" => accessory.host = d.string(value),
                                    "port" => accessory.port = d.string(value),
                                    "volumes" => accessory.volumes = d.strings(value),
                                    "env" => accessory.env = d.env(value),
                                    "cmd" => accessory.cmd = d.strings(value),
                                    _ => return false,
                                }
                                true
                            });
                            config.accessories.insert(accessory_name, accessory);
                        }
                    }
                    _ => d.type_error(value, "map[string]deploy.Accessory"),
                },
                "builder" => {
                    let builder = &mut config.builder;
                    d.fields(value, "Builder", |d, name, value| {
                        match name {
                            "context" => builder.context = d.string(value),
                            "dockerfile" => builder.dockerfile = d.string(value),
                            "platform" => builder.platform = d.string(value),
                            "args" => builder.args = d.string_map(value),
                            _ => return false,
                        }
                        true
                    });
                }
                "retain" => config.retain = d.int(value, "int"),
                "stop_timeout" => config.stop_timeout = d.duration(value),
                "boot_timeout" => config.boot_timeout = d.duration(value),
                "hooks" => config.hooks = d.string(value),
                "ssh" => {
                    let ssh = &mut config.ssh;
                    d.fields(value, "SSH", |d, name, value| {
                        match name {
                            "user" => ssh.user = d.string(value),
                            "port" => ssh.port = d.int(value, "int"),
                            "proxy_jump" => ssh.proxy_jump = d.string(value),
                            _ => return false,
                        }
                        true
                    });
                }
                "parallelism" => config.parallelism = d.int(value, "int"),
                _ => return false,
            }
            true
        });
        config
    }
}

fn go_duration(nanos: i64) -> GoDuration {
    GoDuration::from_nanos(nanos)
}

const MILLISECOND: i64 = 1_000_000;
/// The largest `config/deploy.yml` read: deploy files are a few kilobytes,
/// and a bounded read keeps a wrong path from filling memory.
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;

const SECOND: i64 = 1_000 * MILLISECOND;
const MINUTE: i64 = 60 * SECOND;
const HOUR: i64 = 60 * MINUTE;

impl Config {
    /// Reads and validates the configuration at `path`.
    pub(crate) fn load(path: &std::path::Path) -> Result<Self, String> {
        use std::io::Read as _;
        let file = std::fs::File::open(path)
            .map_err(|error| format!("read deploy config: {}", go_io_error(path, &error)))?;
        let mut content = Vec::new();
        file.take(MAX_CONFIG_BYTES + 1)
            .read_to_end(&mut content)
            .map_err(|error| format!("read deploy config: {}", go_io_error(path, &error)))?;
        if content.len() as u64 > MAX_CONFIG_BYTES {
            return Err(format!(
                "read deploy config: {} is larger than {MAX_CONFIG_BYTES} bytes",
                path.display()
            ));
        }
        Self::parse(&content)
    }

    /// Decodes and validates a configuration, applying the defaults.
    pub(crate) fn parse(content: &[u8]) -> Result<Self, String> {
        let text = std::str::from_utf8(content)
            .map_err(|_| "deploy config: yaml: invalid UTF-8 in the document".to_owned())?;
        let node = match yaml::parse(text) {
            Ok(Some(node)) => node,
            Ok(None) => return Err("deploy config is empty".to_owned()),
            Err(error) if error.message.contains("exactly one YAML document") => {
                return Err("deploy config must contain exactly one YAML document".to_owned());
            }
            Err(error) => return Err(format!("deploy config: {error}")),
        };
        let mut decoder = Decoder::default();
        let mut config = decoder.config(&node);
        if let Some(fatal) = decoder.fatal {
            return Err(format!("deploy config: {fatal}"));
        }
        if !decoder.errors.is_empty() {
            return Err(format!(
                "deploy config: yaml: unmarshal errors:\n  {}",
                decoder.errors.join("\n  ")
            ));
        }
        config
            .normalize()
            .map_err(|problems| format!("deploy config: {problems}"))?;
        Ok(config)
    }

    #[allow(clippy::too_many_lines)]
    fn normalize(&mut self) -> Result<(), String> {
        let mut problems: Vec<String> = Vec::new();
        let mut add = |problem: String| problems.push(problem);

        if !SERVICE_NAME.matches(&self.service) {
            add(format!("service must match {}", SERVICE_NAME.source()));
        }
        if let Err(error) = valid_image_name(&self.image) {
            add(format!("image: {error}"));
        }
        if !self.registry.server.is_empty() && !REGISTRY_HOST.matches(&self.registry.server) {
            add("registry.server must be a host name with an optional port".to_owned());
        }
        if self.registry.username.is_empty() != self.registry.password.is_empty() {
            add(
                "registry.username and registry.password (a secret name) must be set together"
                    .to_owned(),
            );
        }
        if !self.registry.password.is_empty() && !ENV_KEY.matches(&self.registry.password) {
            add(
                "registry.password must name a secret in .forge/secrets, not contain the password"
                    .to_owned(),
            );
        }
        if !self.registry.username.is_empty()
            && self.registry.username.contains([' ', '\t', '\r', '\n'])
        {
            add("registry.username contains whitespace".to_owned());
        }
        if self.servers.is_empty() {
            add("servers must define at least one role".to_owned());
        }
        let mut proxy_roles = 0;
        for (name, role) in &mut self.servers {
            if !ROLE_NAME.matches(name) {
                add(format!(
                    "servers.{name}: role name must match {}",
                    ROLE_NAME.source()
                ));
            }
            if role.hosts.is_empty() {
                add(format!("servers.{name}.hosts must list at least one host"));
            }
            let mut seen = std::collections::HashSet::new();
            for host in &role.hosts {
                if !valid_ssh_host(host) {
                    add(format!(
                        "servers.{name}.hosts: {host:?} is not a valid host name or address"
                    ));
                }
                if !seen.insert(host.clone()) {
                    add(format!("servers.{name}.hosts: {host:?} is listed twice"));
                }
            }
            let proxied = *role.proxy.get_or_insert(name == "web");
            if proxied {
                proxy_roles += 1;
            }
            check_args(&format!("servers.{name}.cmd"), &role.cmd, &mut add);
            check_env(&format!("servers.{name}.env"), &role.env, &mut add);
            check_volumes(&format!("servers.{name}.volumes"), &role.volumes, &mut add);
        }
        if proxy_roles > 1 {
            add("only one role may use the proxy (proxy: true)".to_owned());
        }
        check_env("env", &self.env, &mut add);
        check_volumes("volumes", &self.volumes, &mut add);
        for (name, role) in &self.servers {
            for key in role.env.clear.keys() {
                if self.env.secret.contains(key) {
                    add(format!(
                        "servers.{name}.env.clear.{key} is also a secret in env.secret"
                    ));
                }
            }
            for key in &role.env.secret {
                if self.env.clear.contains_key(key) {
                    add(format!(
                        "servers.{name}.env.secret {key} is also in env.clear"
                    ));
                }
            }
        }

        self.normalize_proxy(proxy_roles > 0, &mut add);

        if self.migrate.enabled.is_none() {
            self.migrate.enabled = Some(true);
        }
        if self.migrate.cmd.is_empty() {
            self.migrate.cmd = vec!["migrate".to_owned()];
        }
        check_args("migrate.cmd", &self.migrate.cmd, &mut add);
        if self.migrate.role.is_empty() {
            self.migrate.role = self.primary_role();
        } else if !self.servers.contains_key(&self.migrate.role) {
            add(format!(
                "migrate.role {:?} is not a role in servers",
                self.migrate.role
            ));
        }

        for (name, accessory) in &self.accessories {
            if !ROLE_NAME.matches(name) {
                add(format!(
                    "accessories.{name}: name must match {}",
                    ROLE_NAME.source()
                ));
            }
            if !IMAGE_REF.matches(&accessory.image) {
                add(format!(
                    "accessories.{name}.image is required and must be an image reference"
                ));
            }
            if !valid_ssh_host(&accessory.host) {
                add(format!(
                    "accessories.{name}.host must be a host name or address"
                ));
            }
            if !accessory.port.is_empty()
                && let Err(error) = publish_spec(&accessory.port)
            {
                add(format!("accessories.{name}.port: {error}"));
            }
            check_volumes(
                &format!("accessories.{name}.volumes"),
                &accessory.volumes,
                &mut add,
            );
            check_env(&format!("accessories.{name}.env"), &accessory.env, &mut add);
            check_args(&format!("accessories.{name}.cmd"), &accessory.cmd, &mut add);
        }

        if self.builder.context.is_empty() {
            ".".clone_into(&mut self.builder.context);
        }
        if self.builder.dockerfile.is_empty() {
            "Dockerfile".clone_into(&mut self.builder.dockerfile);
        }
        if self.builder.context.starts_with('-') || self.builder.dockerfile.starts_with('-') {
            add("builder.context and builder.dockerfile must not start with '-'".to_owned());
        }
        if !self.builder.platform.is_empty() && !PLATFORM.matches(&self.builder.platform) {
            add("builder.platform must look like linux/amd64".to_owned());
        }
        for (key, value) in &self.builder.args {
            if !ENV_KEY.matches(key) || !ARG.matches(value) {
                add(format!("builder.args.{key} is invalid"));
            }
        }

        if self.retain == 0 {
            self.retain = DEFAULT_RETAIN;
        }
        if !(MIN_RETAIN..=100).contains(&self.retain) {
            add("retain must be between 2 and 100 (1 would remove the previous version, so rollback would be impossible)".to_owned());
        }
        duration_default(
            &mut self.stop_timeout,
            35 * SECOND,
            SECOND,
            10 * MINUTE,
            "stop_timeout",
            &mut add,
        );
        duration_default(
            &mut self.boot_timeout,
            60 * SECOND,
            SECOND,
            HOUR,
            "boot_timeout",
            &mut add,
        );
        if self.hooks.is_empty() {
            DEFAULT_HOOKS_DIR.clone_into(&mut self.hooks);
        }
        if !self.ssh.user.is_empty() && !USER.matches(&self.ssh.user) {
            add("ssh.user is not a valid user name".to_owned());
        }
        if !(0..=65535).contains(&self.ssh.port) {
            add("ssh.port must be between 1 and 65535".to_owned());
        }
        if !self.ssh.proxy_jump.is_empty() && !PROXY_JUMP.matches(&self.ssh.proxy_jump) {
            add("ssh.proxy_jump must look like [user@]host[:port]".to_owned());
        }
        if self.parallelism == 0 {
            self.parallelism = DEFAULT_PARALLELISM;
        }
        if !(1..=64).contains(&self.parallelism) {
            add("parallelism must be between 1 and 64".to_owned());
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(problems.join("; "))
        }
    }

    fn normalize_proxy(&mut self, used: bool, add: &mut impl FnMut(String)) {
        let proxy = &mut self.proxy;
        if !used {
            if !proxy.image.is_empty() || !proxy.hosts.is_empty() || proxy.ssl {
                add(
                    "proxy is configured but no role uses it (set proxy: true on a role)"
                        .to_owned(),
                );
            }
            return;
        }
        if !IMAGE_REF.matches(&proxy.image) {
            add("proxy.image is required: the forge-proxy image built from crates/forge-proxy/Dockerfile, ideally pinned by digest".to_owned());
        }
        let mut seen = std::collections::HashSet::new();
        for host in &mut proxy.hosts {
            let lowered = host.to_lowercase();
            if !valid_dns_name(&lowered) {
                add(format!("proxy.hosts: {lowered:?} is not a DNS name"));
            }
            if !seen.insert(lowered.clone()) {
                add(format!("proxy.hosts: {lowered:?} is listed twice"));
            }
            *host = lowered;
        }
        if proxy.ssl && proxy.hosts.is_empty() {
            add("proxy.ssl requires proxy.hosts".to_owned());
        }
        if !proxy.acme_email.is_empty() && !EMAIL.matches(&proxy.acme_email) {
            add("proxy.acme_email is not an e-mail address".to_owned());
        }
        if proxy.app_port == 0 {
            proxy.app_port = DEFAULT_APP_PORT;
        }
        if !(1..=65535).contains(&proxy.app_port) {
            add("proxy.app_port must be between 1 and 65535".to_owned());
        }
        let health = &mut proxy.healthcheck;
        if health.path.is_empty() {
            DEFAULT_HEALTH_PATH.clone_into(&mut health.path);
        }
        if !HEALTH_PATH.matches(&health.path) {
            add("proxy.healthcheck.path must be an absolute path without query".to_owned());
        }
        if health.healthy_count == 0 {
            health.healthy_count = DEFAULT_HEALTHY_COUNT;
        }
        if !(1..=100).contains(&health.healthy_count) {
            add("proxy.healthcheck.healthy_count must be between 1 and 100".to_owned());
        }
        duration_default(
            &mut health.interval,
            SECOND,
            10 * MILLISECOND,
            MINUTE,
            "proxy.healthcheck.interval",
            add,
        );
        duration_default(
            &mut health.timeout,
            2 * SECOND,
            10 * MILLISECOND,
            MINUTE,
            "proxy.healthcheck.timeout",
            add,
        );
        duration_default(
            &mut health.deploy_timeout,
            60 * SECOND,
            SECOND,
            HOUR,
            "proxy.healthcheck.deploy_timeout",
            add,
        );
        duration_default(
            &mut proxy.drain_timeout,
            30 * SECOND,
            0,
            HOUR,
            "proxy.drain_timeout",
            add,
        );
        duration_default(
            &mut proxy.response_timeout,
            30 * SECOND,
            SECOND,
            HOUR,
            "proxy.response_timeout",
            add,
        );
        if proxy.max_body_bytes == 0 {
            proxy.max_body_bytes = DEFAULT_MAX_BODY_BYTES;
        }
        if proxy.max_body_bytes < 0 {
            add("proxy.max_body_bytes must be positive".to_owned());
        }
    }

    /// The proxied role, else `web`, else the first role by name.
    pub(crate) fn primary_role(&self) -> String {
        if let Some((name, _)) = self
            .servers
            .iter()
            .find(|(_, role)| role.proxy == Some(true))
        {
            return name.clone();
        }
        if self.servers.contains_key("web") {
            return "web".to_owned();
        }
        self.servers.keys().next().cloned().unwrap_or_default()
    }

    /// Role names, the primary role first, then by name.
    pub(crate) fn roles(&self) -> Vec<String> {
        let primary = self.primary_role();
        let mut roles = vec![primary.clone()];
        roles.extend(
            self.servers
                .keys()
                .filter(|name| **name != primary)
                .cloned(),
        );
        roles
    }

    /// Whether `role` routes traffic through forge-proxy.
    pub(crate) fn proxied(&self, role: &str) -> bool {
        self.servers
            .get(role)
            .is_some_and(|role| role.proxy == Some(true))
    }

    /// Every host of every role, in role order then config order, once.
    pub(crate) fn app_hosts(&self) -> Vec<String> {
        let mut hosts: Vec<String> = Vec::new();
        for role in self.roles() {
            if let Some(role) = self.servers.get(&role) {
                for host in &role.hosts {
                    if !hosts.contains(host) {
                        hosts.push(host.clone());
                    }
                }
            }
        }
        hosts
    }

    /// The application hosts followed by accessory hosts.
    pub(crate) fn all_hosts(&self) -> Vec<String> {
        let mut hosts = self.app_hosts();
        for accessory in self.accessories.values() {
            if !hosts.contains(&accessory.host) {
                hosts.push(accessory.host.clone());
            }
        }
        hosts
    }

    /// The roles that run on `host`, in [`Config::roles`] order.
    pub(crate) fn roles_on(&self, host: &str) -> Vec<String> {
        self.roles()
            .into_iter()
            .filter(|role| {
                self.servers
                    .get(role)
                    .is_some_and(|r| r.hosts.iter().any(|h| h == host))
            })
            .collect()
    }

    /// Every secret the configuration references, sorted.
    pub(crate) fn secret_names(&self) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        let mut add = |list: &[String]| {
            for name in list {
                if !names.contains(name) {
                    names.push(name.clone());
                }
            }
        };
        if !self.registry.password.is_empty() {
            add(std::slice::from_ref(&self.registry.password));
        }
        add(&self.env.secret);
        for role in self.servers.values() {
            add(&role.env.secret);
        }
        for accessory in self.accessories.values() {
            add(&accessory.env.secret);
        }
        names.sort();
        names
    }
}

fn duration_default(
    value: &mut GoDuration,
    default: i64,
    minimum: i64,
    maximum: i64,
    name: &str,
    add: &mut impl FnMut(String),
) {
    if value.is_zero() {
        *value = go_duration(default);
    }
    if value.as_nanos() < minimum || value.as_nanos() > maximum {
        add(format!(
            "{name} must be between {} and {}",
            go_duration(minimum),
            go_duration(maximum)
        ));
    }
}

fn check_env(prefix: &str, env: &Env, add: &mut impl FnMut(String)) {
    for (key, value) in &env.clear {
        if !ENV_KEY.matches(key) {
            add(format!(
                "{prefix}.clear: {key:?} is not a valid variable name"
            ));
        }
        if value.contains(['\r', '\n', '\0']) {
            add(format!("{prefix}.clear.{key} must be a single line"));
        }
    }
    let mut seen = std::collections::HashSet::new();
    for key in &env.secret {
        if !ENV_KEY.matches(key) {
            add(format!(
                "{prefix}.secret: {key:?} is not a valid secret name"
            ));
        }
        if !seen.insert(key.clone()) {
            add(format!("{prefix}.secret: {key} is listed twice"));
        }
        if env.clear.contains_key(key) {
            add(format!("{prefix}: {key} is both clear and secret"));
        }
    }
}

fn check_volumes(prefix: &str, volumes: &[String], add: &mut impl FnMut(String)) {
    for volume in volumes {
        if !VOLUME.matches(volume) || volume.contains("..") || volume.starts_with('-') {
            add(format!(
                "{prefix}: {volume:?} must be NAME:/container/path or /host/path:/container/path with optional :ro"
            ));
        }
    }
}

fn check_args(prefix: &str, args: &[String], add: &mut impl FnMut(String)) {
    for arg in args {
        if !ARG.matches(arg) {
            add(format!("{prefix} contains a control character"));
        }
    }
}

fn valid_image_name(image: &str) -> Result<(), &'static str> {
    if !IMAGE.matches(image) {
        return Err("required, lower-case repository such as registry.example.com/team/app");
    }
    let last = image.rsplit('/').next().unwrap_or(image);
    if last.contains(':') || image.contains('@') {
        return Err("must not include a tag or digest; the deploy version is the tag");
    }
    Ok(())
}

pub(crate) fn valid_ssh_host(host: &str) -> bool {
    SSH_HOST.matches(host)
}

fn valid_dns_name(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && host.parse::<IpAddr>().is_err()
        && host.split('.').all(|label| DNS_LABEL.matches(label))
}

/// An accessory port as a docker `--publish` value bound to loopback unless
/// an address is given: `5432` and `6543:5432` bind 127.0.0.1;
/// `10.0.0.5:6543:5432` binds that address.
pub(crate) fn publish_spec(port: &str) -> Result<String, String> {
    let parts: Vec<&str> = port.split(':').collect();
    let check = |value: &str| -> Result<(), String> {
        match value.parse::<i64>() {
            Ok(number) if (1..=65535).contains(&number) => Ok(()),
            _ => Err(format!("{value:?} is not a port number")),
        }
    };
    match parts.as_slice() {
        [single] => {
            check(single)?;
            Ok(format!("127.0.0.1:{single}:{single}"))
        }
        [host_port, container_port] => {
            check(host_port)?;
            check(container_port)?;
            Ok(format!("127.0.0.1:{port}"))
        }
        [address, host_port, container_port] => {
            if address.is_empty() || address.parse::<IpAddr>().is_err() {
                return Err("the bind address must be an IP address".to_owned());
            }
            check(host_port)?;
            check(container_port)?;
            Ok(port.to_owned())
        }
        _ => Err("use PORT, HOST_PORT:CONTAINER_PORT or IP:HOST_PORT:CONTAINER_PORT".to_owned()),
    }
}

/// Go's `*PathError` text: `open PATH: no such file or directory`.
pub(crate) fn go_io_error(path: &std::path::Path, error: &std::io::Error) -> String {
    let reason = match error.kind() {
        std::io::ErrorKind::NotFound => "no such file or directory".to_owned(),
        std::io::ErrorKind::PermissionDenied => "permission denied".to_owned(),
        _ => error.to_string(),
    };
    format!("open {}: {reason}", path.display())
}

#[cfg(test)]
mod option_like_values {
    use super::{PROXY_JUMP, REGISTRY_HOST};

    #[test]
    fn jump_and_registry_hosts_never_start_with_a_dash() {
        // `ssh -J VALUE` and `docker login VALUE` would read a leading `-` as
        // an option.
        for value in ["-Fevil", "ops@-Fevil", "--help", "-x:22"] {
            assert!(!PROXY_JUMP.matches(value), "proxy_jump {value:?}");
            assert!(!REGISTRY_HOST.matches(value), "registry {value:?}");
        }
        assert!(PROXY_JUMP.matches("ops@bastion.example.com:2222"));
        assert!(PROXY_JUMP.matches("_bastion"));
        assert!(REGISTRY_HOST.matches("registry.example.com:5000"));
        assert!(REGISTRY_HOST.matches("ghcr.io"));
    }
}
