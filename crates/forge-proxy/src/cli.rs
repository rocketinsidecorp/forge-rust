//! The `forge-proxy` command line: `run` (the default) serves traffic;
//! `deploy`, `remove`, `maintenance`, `status` and `healthcheck` talk to a
//! running proxy over its control socket. Commands, flags, output and exit
//! codes are Forge-Go's.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::control::{ControlRequest, call, listen_control};
use crate::duration::GoDuration;
use crate::flags::{self, Spec};
use crate::listen::{self, HttpLimits, Plain};
use crate::log::{Logger, text};
use crate::server::Server;
use crate::settings::{
    DEFAULT_BUFFER_BYTES, DEFAULT_DEPLOY_TIMEOUT, DEFAULT_DRAIN_TIMEOUT, DEFAULT_HEALTH_INTERVAL,
    DEFAULT_HEALTH_PATH, DEFAULT_HEALTH_TIMEOUT, DEFAULT_HEALTHY_COUNT, DEFAULT_MAX_BODY_BYTES,
    DEFAULT_RESPONSE_TIMEOUT, DeployRequest,
};

/// The control socket inside the forge-proxy image.
pub const DEFAULT_CONTROL_SOCKET: &str = "/run/forge-proxy/control.sock";
/// The routing state inside the forge-proxy image (on its volume).
pub const DEFAULT_STATE_PATH: &str = "/var/lib/forge-proxy/state.json";
/// The ACME account and certificate cache (on the volume).
pub const DEFAULT_ACME_CACHE: &str = "/var/lib/forge-proxy/acme";

/// Success.
pub const EXIT_OK: i32 = 0;
/// The command failed.
pub const EXIT_FAILED: i32 = 1;
/// Invalid usage.
pub const EXIT_USAGE: i32 = 2;
/// The new target never passed its health check.
pub const EXIT_UNHEALTHY: i32 = 4;

const RUN_FLAGS: &[Spec] = &[
    (
        "http-addr",
        false,
        "plain HTTP listen address (publish as host port 80)",
    ),
    (
        "https-addr",
        false,
        "HTTPS listen address (publish as host port 443); empty disables HTTPS and ACME",
    ),
    ("control", false, "control socket path"),
    (
        "state",
        false,
        "routing state file; empty disables persistence",
    ),
    (
        "acme-cache",
        false,
        "directory caching ACME account keys and certificates",
    ),
    (
        "acme-email",
        false,
        "contact address registered with the ACME CA",
    ),
    (
        "acme-directory",
        false,
        "ACME directory URL (default: Let's Encrypt production)",
    ),
    (
        "read-header-timeout",
        false,
        "time allowed to read request headers",
    ),
    (
        "idle-timeout",
        false,
        "keep-alive idle timeout; also how long a write to a client may make no progress and an upgraded connection may carry no data",
    ),
    ("shutdown-timeout", false, "graceful shutdown budget"),
];

const DEPLOY_FLAGS: &[Spec] = &[
    ("control", false, "control socket path"),
    ("service", false, "service name"),
    ("target", false, "new target host:port"),
    ("hosts", false, "comma-separated host names"),
    (
        "tls",
        true,
        "obtain certificates and redirect HTTP to HTTPS",
    ),
    ("health-path", false, "health check path"),
    ("health-interval", false, "time between health checks"),
    ("health-timeout", false, "timeout of one health check"),
    (
        "healthy-count",
        false,
        "consecutive passing checks required",
    ),
    ("deploy-timeout", false, "time allowed to become healthy"),
    (
        "drain-timeout",
        false,
        "time allowed for in-flight requests on the previous target",
    ),
    (
        "response-timeout",
        false,
        "time allowed for the target to send response headers",
    ),
    ("max-body-bytes", false, "largest accepted request body"),
    (
        "buffer-bytes",
        false,
        "largest request body buffered before forwarding",
    ),
];

const REMOVE_FLAGS: &[Spec] = &[
    ("control", false, "control socket path"),
    ("service", false, "service name"),
    (
        "drain-timeout",
        false,
        "time allowed for in-flight requests",
    ),
];

const MAINTENANCE_FLAGS: &[Spec] = &[
    ("control", false, "control socket path"),
    ("service", false, "service name"),
];

const CONTROL_ONLY_FLAGS: &[Spec] = &[("control", false, "control socket path")];

/// Runs the command line. `run` serves until `shutdown` is cancelled.
pub async fn main(
    args: &[String],
    stdout: Box<dyn Write + Send>,
    mut stderr: Box<dyn Write + Send>,
    shutdown: CancellationToken,
) -> i32 {
    let (command, rest) = match args.first() {
        Some(first) if !first.starts_with('-') => (first.as_str(), &args[1..]),
        _ => ("run", args),
    };
    match command {
        "run" => match run(rest, stdout, &mut stderr, shutdown).await {
            Ok(()) => EXIT_OK,
            Err(error) => {
                let _ = writeln!(stderr, "forge-proxy: {error}");
                EXIT_FAILED
            }
        },
        "deploy" | "remove" | "maintenance" | "status" | "healthcheck" => {
            client(command, rest, stdout, &mut stderr).await
        }
        other => {
            let _ = writeln!(
                stderr,
                "forge-proxy: unknown command {other:?} (run, deploy, remove, maintenance, status, healthcheck)"
            );
            EXIT_USAGE
        }
    }
}

/// `:8080` listens on every IPv4 address, as Go's `:8080` does.
fn listen_address(address: &str) -> String {
    if address.starts_with(':') {
        format!("0.0.0.0{address}")
    } else {
        address.to_owned()
    }
}

async fn run(
    args: &[String],
    stdout: Box<dyn Write + Send>,
    stderr: &mut Box<dyn Write + Send>,
    shutdown: CancellationToken,
) -> Result<(), String> {
    let parsed = flags::parse(args, RUN_FLAGS).map_err(|error| {
        let _ = write!(stderr, "{}\n{}", error.0, flags::usage("run", RUN_FLAGS));
        error.0
    })?;
    if !parsed.rest.is_empty() {
        return Err("run accepts flags only".to_owned());
    }
    let http_addr = parsed.string("http-addr", ":8080");
    let https_addr = parsed.string("https-addr", ":8443");
    let control_path = PathBuf::from(parsed.string("control", DEFAULT_CONTROL_SOCKET));
    let state = parsed.string("state", DEFAULT_STATE_PATH);
    let acme_cache = PathBuf::from(parsed.string("acme-cache", DEFAULT_ACME_CACHE));
    let acme_email = parsed.string("acme-email", "");
    let acme_directory = parsed.string("acme-directory", "");
    let flag_error = |error: flags::FlagError| error.0;
    let read_header = parsed
        .duration("read-header-timeout", GoDuration::from_secs(10))
        .map_err(flag_error)?;
    let idle = parsed
        .duration("idle-timeout", GoDuration::from_secs(120))
        .map_err(flag_error)?;
    if idle.as_nanos() <= 0 {
        return Err("-idle-timeout must be positive".to_owned());
    }
    let shutdown_timeout = parsed
        .duration("shutdown-timeout", GoDuration::from_secs(30))
        .map_err(flag_error)?;
    let limits = HttpLimits {
        read_header_timeout: read_header.to_std().max(Duration::from_millis(1)),
        idle_timeout: idle.to_std(),
        shutdown_timeout: shutdown_timeout.to_std(),
    };

    let logger = Logger::new(stdout);
    let state_path = (!state.is_empty()).then(|| PathBuf::from(&state));
    let server = Server::new(logger.clone(), state_path)?;
    // Everything that can refuse the configuration runs before the control
    // socket exists, so a failed start leaves nothing behind.
    let tls = if https_addr.is_empty() {
        None
    } else {
        Some(crate::tls::Tls::new(
            server.clone(),
            crate::tls::AcmeSettings {
                cache: acme_cache,
                email: acme_email,
                directory: acme_directory,
                directory_roots: Vec::new(),
            },
        )?)
    };
    let plain = tokio::net::TcpListener::bind(listen_address(&http_addr))
        .await
        .map_err(|error| format!("listen tcp {http_addr}: {error}"))?;
    let https = match tls {
        Some(tls) => {
            let listener = tokio::net::TcpListener::bind(listen_address(&https_addr))
                .await
                .map_err(|error| format!("listen tcp {https_addr}: {error}"))?;
            Some((listener, tls))
        }
        None => None,
    };
    let control = listen_control(&control_path)?;
    logger.info(
        "forge-proxy started",
        &[
            ("http_addr", text(&http_addr)),
            ("https_addr", text(&https_addr)),
            ("control", text(control_path.display().to_string())),
        ],
    );
    let stop = shutdown.child_token();
    let plain_loop = listen::serve(server.clone(), plain, Arc::new(Plain), limits, stop.clone());
    let control_loop = server.serve_control(control, stop.clone());
    match https {
        Some((listener, tls)) => {
            let acceptor = Arc::new(tls.acceptor());
            let tls_loop = listen::serve(server.clone(), listener, acceptor, limits, stop.clone());
            tokio::join!(plain_loop, control_loop, tls_loop, tls.run(stop.clone()));
        }
        None => {
            tokio::join!(plain_loop, control_loop);
        }
    }
    server.shutdown().await;
    // Like Go's Unix listener, remove the socket on the way out.
    let _ = std::fs::remove_file(&control_path);
    logger.info("forge-proxy stopped", &[]);
    Ok(())
}

async fn client(
    command: &str,
    args: &[String],
    mut stdout: Box<dyn Write + Send>,
    stderr: &mut Box<dyn Write + Send>,
) -> i32 {
    let spec = match command {
        "deploy" => DEPLOY_FLAGS,
        "remove" => REMOVE_FLAGS,
        "maintenance" => MAINTENANCE_FLAGS,
        _ => CONTROL_ONLY_FLAGS,
    };
    let parsed = match flags::parse(args, spec) {
        Ok(parsed) => parsed,
        Err(error) => {
            let _ = write!(stderr, "{}\n{}", error.0, flags::usage(command, spec));
            return EXIT_USAGE;
        }
    };
    let control = PathBuf::from(parsed.string("control", DEFAULT_CONTROL_SOCKET));
    let mut request = ControlRequest {
        op: command.to_owned(),
        ..ControlRequest::default()
    };
    let mut rest = parsed.rest.clone();
    let mut timeout = Duration::from_secs(30);
    let typed = (|| -> Result<(), flags::FlagError> {
        match command {
            "deploy" => {
                let hosts = parsed.string("hosts", "");
                let deploy = DeployRequest {
                    service: parsed.string("service", ""),
                    target: parsed.string("target", ""),
                    hosts: if hosts.is_empty() {
                        Vec::new()
                    } else {
                        hosts.split(',').map(str::to_owned).collect()
                    },
                    tls: parsed.bool("tls"),
                    health_path: parsed.string("health-path", DEFAULT_HEALTH_PATH),
                    health_interval: parsed.duration("health-interval", DEFAULT_HEALTH_INTERVAL)?,
                    health_timeout: parsed.duration("health-timeout", DEFAULT_HEALTH_TIMEOUT)?,
                    healthy_count: parsed.int("healthy-count", DEFAULT_HEALTHY_COUNT)?,
                    deploy_timeout: parsed.duration("deploy-timeout", DEFAULT_DEPLOY_TIMEOUT)?,
                    drain_timeout: parsed.duration("drain-timeout", DEFAULT_DRAIN_TIMEOUT)?,
                    response_timeout: parsed
                        .duration("response-timeout", DEFAULT_RESPONSE_TIMEOUT)?,
                    max_body_bytes: parsed.int("max-body-bytes", DEFAULT_MAX_BODY_BYTES)?,
                    buffer_bytes: parsed.int("buffer-bytes", DEFAULT_BUFFER_BYTES)?,
                };
                timeout = deploy.deploy_timeout.to_std()
                    + deploy.drain_timeout.to_std()
                    + Duration::from_secs(30);
                request.deploy = Some(deploy);
            }
            "remove" => {
                request.service = parsed.string("service", "");
                request.drain_timeout = parsed.duration("drain-timeout", DEFAULT_DRAIN_TIMEOUT)?;
                timeout = request.drain_timeout.to_std() + Duration::from_secs(30);
            }
            "maintenance" => {
                request.service = parsed.string("service", "");
            }
            "healthcheck" => {
                "status".clone_into(&mut request.op);
                timeout = Duration::from_secs(3);
            }
            _ => {}
        }
        Ok(())
    })();
    if let Err(error) = typed {
        let _ = write!(stderr, "{}\n{}", error.0, flags::usage(command, spec));
        return EXIT_USAGE;
    }
    if command == "maintenance" {
        if rest.len() != 1 || (rest[0] != "on" && rest[0] != "off") {
            let _ = writeln!(
                stderr,
                "usage: forge-proxy maintenance -service NAME on|off"
            );
            return EXIT_USAGE;
        }
        request.maintenance = rest[0] == "on";
        rest.clear();
    }
    if !rest.is_empty() {
        let _ = writeln!(stderr, "forge-proxy {command}: unexpected arguments");
        return EXIT_USAGE;
    }
    let response = match call(&control, &request, timeout).await {
        Ok(response) => response,
        Err(error) => {
            let _ = writeln!(stderr, "forge-proxy: {error}");
            return EXIT_FAILED;
        }
    };
    if command != "healthcheck"
        && let Ok(encoded) = serde_json::to_string(&response)
    {
        let _ = writeln!(stdout, "{encoded}");
        let _ = stdout.flush();
    }
    if !response.ok {
        let _ = writeln!(stderr, "forge-proxy: {}", response.error);
        if response.code == "unhealthy" {
            return EXIT_UNHEALTHY;
        }
        return EXIT_FAILED;
    }
    EXIT_OK
}
