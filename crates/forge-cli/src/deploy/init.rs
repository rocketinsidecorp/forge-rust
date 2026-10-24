//! `forge deploy init`: a commented `config/deploy.yml`, `.forge/secrets`
//! (0600) and the ignore entries that keep secrets out of Git and images.

use std::path::Path;

use super::config::{DEFAULT_CONFIG_PATH, SERVICE_NAME};
use super::secrets::DEFAULT_SECRETS_PATH;

/// Written by `forge deploy init`; `SERVICE` is replaced with the
/// application name. It must stay a valid configuration (tests parse it).
pub(crate) const CONFIG_TEMPLATE: &str = r#"# forge deploy configuration. Reference: docs/deployment.md#forge-deploy
#
#   forge deploy setup     # once per host: docker check, network, proxy, accessories
#   forge deploy           # build, push, migrate, switch with zero downtime
#   forge deploy rollback VERSION
#
# Every command accepts --dry-run, which prints the remote commands
# (secrets redacted) instead of running them.

# Container names, labels and remote state (~/.forge/SERVICE) use this name.
service: SERVICE

# Repository without tag: the deploy version (git commit, or --version) is
# the tag.
image: registry.example.com/acme/SERVICE

registry:
  server: registry.example.com
  username: acme
  # Name of a secret in .forge/secrets, never the password itself.
  password: REGISTRY_PASSWORD

# Roles and their hosts. "web" is routed through forge-proxy; other roles
# (jobs workers, ...) run the same image with their own cmd.
servers:
  web:
    hosts:
      - 192.0.2.10
  # jobs:
  #   hosts:
  #     - 192.0.2.11
  #   cmd: ["jobs"]
  #   proxy: false

env:
  clear:
    FORGE_ENV: production
    # The proxied role trusts X-Forwarded-For only from the forge Docker
    # network (forge-proxy), set automatically. Override with explicit CIDRs,
    # or "" to trust nobody:
    # FORGE_HTTP_TRUSTED_PROXIES: "172.18.0.0/16"
  # Names of secrets from .forge/secrets. They reach containers through an
  # env file written 0600 on the host, never through the command line.
  # `migrate` runs with the same env file, so the role's containers also
  # hold FORGE_MIGRATION_DATABASE_URL. To keep the schema-owner credential
  # out of them, drop it here, set migrate.enabled: false and run
  # migrations as a separate job (docs/deployment.md).
  secret:
    - FORGE_DATABASE_URL
    - FORGE_MIGRATION_DATABASE_URL

# SQLite applications keep their database on a volume:
# volumes:
#   - SERVICE-data:/data

proxy:
  # Build and push it once from the Forge repository:
  #   docker build -f crates/forge-proxy/Dockerfile -t registry.example.com/acme/forge-proxy:0.1.0 .
  # then pin the pushed digest: registry.example.com/acme/forge-proxy@sha256:...
  image: registry.example.com/acme/forge-proxy:0.1.0
  hosts:
    - app.example.com
  # Let's Encrypt certificates (accepts the Let's Encrypt terms). Point the
  # DNS records at the web hosts first.
  ssl: true
  # acme_email: ops@example.com
  # Forge-Rust applications listen on 3000 (FORGE_BIND).
  app_port: 3000
  healthcheck:
    path: /health/ready
    interval: 1s
    timeout: 2s
    healthy_count: 3
    deploy_timeout: 60s
  drain_timeout: 30s

# Runs "<image> migrate" once, on the first web host, before any switch.
migrate:
  enabled: true

# accessories:
#   db:
#     image: postgres:18-alpine
#     host: 192.0.2.20
#     port: 5432            # published on 127.0.0.1 only
#     volumes:
#       - SERVICE-postgres:/var/lib/postgresql
#     env:
#       secret:
#         - POSTGRES_PASSWORD

builder:
  context: .
  dockerfile: Dockerfile
  # platform: linux/amd64

retain: 5            # versions kept per role for rollback (2 or more; the running one counts)
stop_timeout: 35s    # SIGTERM grace; above FORGE_SHUTDOWN_GRACE_SECS (15s)

ssh:
  user: deploy
  # port: 22
  # proxy_jump: bastion.example.com
"#;

/// Written to `.forge/secrets` with mode 0600; only `$VARIABLE`
/// references, no values.
pub(crate) const SECRETS_TEMPLATE: &str = r"# Secrets for forge deploy. This file is listed in .gitignore and
# .dockerignore; never commit it.
#
#   NAME=value      literal value (quotes are removed, nothing is expanded)
#   NAME=$VARIABLE  copied from your local environment at deploy time, so the
#                   value can come from a password manager or CI secret store
#
# Command substitution is not supported.
REGISTRY_PASSWORD=$REGISTRY_PASSWORD
FORGE_DATABASE_URL=$FORGE_DATABASE_URL
FORGE_MIGRATION_DATABASE_URL=$FORGE_MIGRATION_DATABASE_URL
";

/// Writes `config/deploy.yml` and `.forge/secrets` under `root` and adds
/// `.forge/secrets` to `.gitignore` and `.forge` to `.dockerignore`.
/// Existing files are never overwritten. Returns the paths it created or
/// changed (also when a later step fails).
pub(crate) fn init(root: &Path, service: &str) -> (Vec<String>, Result<(), String>) {
    let mut changed = Vec::new();
    let result = (|| {
        if !SERVICE_NAME.matches(service) {
            return Err(format!(
                "service name {service:?} must match {} (pass --service)",
                SERVICE_NAME.source()
            ));
        }
        let config_path = root.join(DEFAULT_CONFIG_PATH);
        let secrets_path = root.join(DEFAULT_SECRETS_PATH);
        for path in [&config_path, &secrets_path] {
            if path.exists() {
                return Err(format!(
                    "{} already exists; not overwriting",
                    path.display()
                ));
            }
        }
        create_dir(config_path.parent(), 0o750)?;
        write_new(&config_path, &CONFIG_TEMPLATE.replace("SERVICE", service))?;
        changed.push(DEFAULT_CONFIG_PATH.to_owned());
        create_dir(secrets_path.parent(), 0o700)?;
        write_new(&secrets_path, SECRETS_TEMPLATE)?;
        changed.push(DEFAULT_SECRETS_PATH.to_owned());
        for (file, line) in [
            (".gitignore", ".forge/secrets"),
            (".dockerignore", ".forge"),
        ] {
            if ensure_line(&root.join(file), line)? {
                changed.push(file.to_owned());
            }
        }
        Ok(())
    })();
    (changed, result)
}

fn create_dir(path: Option<&Path>, mode: u32) -> Result<(), String> {
    let Some(path) = path else { return Ok(()) };
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(mode);
    }
    #[cfg(not(unix))]
    let _ = mode;
    builder
        .create(path)
        .map_err(|error| format!("mkdir {}: {error}", path.display()))
}

fn write_new(path: &Path, content: &str) -> Result<(), String> {
    use std::io::Write as _;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|error| format!("open {}: {error}", path.display()))?;
    file.write_all(content.as_bytes())
        .map_err(|error| format!("write {}: {error}", path.display()))
}

/// Appends `line` to the ignore file at `path` unless present.
fn ensure_line(path: &Path, line: &str) -> Result<bool, String> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(format!("read {}: {error}", path.display())),
    };
    let present = content.lines().any(|existing| {
        let trimmed = existing.trim();
        trimmed == line || trimmed == format!("/{line}") || trimmed == format!("{line}/")
    });
    if present {
        return Ok(false);
    }
    let mut text = content;
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&format!("# forge deploy\n{line}\n"));
    std::fs::write(path, text).map_err(|error| format!("write {}: {error}", path.display()))?;
    Ok(true)
}

/// The service name from Cargo.toml's package name or the directory name:
/// lower case, runs of other characters as `-`, at most 40 characters,
/// starting with a letter.
pub(crate) fn default_service_name(root: &Path) -> String {
    let mut name = String::new();
    if let Ok(manifest) = std::fs::read_to_string(root.join("Cargo.toml")) {
        let mut in_package = false;
        for line in manifest.lines() {
            let line = line.trim();
            if line.starts_with('[') {
                in_package = line == "[package]";
                continue;
            }
            if in_package
                && let Some(value) = line.strip_prefix("name")
                && let Some(value) = value.trim_start().strip_prefix('=')
            {
                name = value.trim().trim_matches('"').to_owned();
                break;
            }
        }
    }
    if name.is_empty()
        && let Ok(absolute) = std::path::absolute(root)
    {
        name = absolute
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
    }
    // Each run of characters outside [a-z0-9-] becomes one "-".
    let mut cleaned = String::new();
    let mut in_run = false;
    for character in name.to_lowercase().chars() {
        if character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-' {
            cleaned.push(character);
            in_run = false;
        } else if !in_run {
            cleaned.push('-');
            in_run = true;
        }
    }
    let mut cleaned = cleaned.trim_matches('-').to_owned();
    cleaned.truncate(40);
    if cleaned.is_empty() || !cleaned.as_bytes()[0].is_ascii_lowercase() {
        cleaned = format!("app-{cleaned}");
    }
    cleaned.trim_end_matches('-').to_owned()
}
