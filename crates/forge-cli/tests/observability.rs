//! The local observability stack `forge new` generates (ADR 0034): an
//! overlay for compose.yaml and the configuration under `observability/`,
//! the port of Forge-Go's `internal/scaffold/observability_test.go`.
//!
//! The YAML files are read with the small block-YAML reader at the end of
//! this file (the CLI has no YAML dependency); it understands the subset the
//! generated files use and fails on anything else, and `docker compose
//! config` validates the overlay itself in the end-to-end script.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use forge_config::{AppConfig, TelemetryExporter, TelemetryLogLevel, TelemetryLogsExporter};
use serde_json::{Map, Value};

/// The files `forge new` writes for the observability stack, relative to the
/// application directory.
const OBSERVABILITY_FILES: &[&str] = &[
    "compose.observability.yaml",
    "observability/otel-collector.yaml",
    "observability/prometheus.yml",
    "observability/tempo.yaml",
    "observability/loki.yaml",
    "observability/grafana/provisioning/datasources/datasources.yaml",
    "observability/grafana/provisioning/dashboards/dashboards.yaml",
    "observability/grafana/dashboards/forge-application.json",
];

/// The services the overlay adds to compose.yaml.
const STACK_SERVICES: &[&str] = &["otel-collector", "prometheus", "tempo", "loki", "grafana"];

const DASHBOARD: &str = "observability/grafana/dashboards/forge-application.json";

/// Generates application `name` with `forge new` in a temporary directory.
fn generate(name: &str, extra: &[&str]) -> (tempfile::TempDir, PathBuf) {
    let directory = tempfile::tempdir().expect("temporary directory should be available");
    let mut arguments = vec!["new", name, "--skip-lockfile", "--forge-path", "../forge"];
    arguments.extend_from_slice(extra);
    let output = Command::new(env!("CARGO_BIN_EXE_forge"))
        .args(&arguments)
        .current_dir(directory.path())
        .output()
        .expect("forge executable should start");
    assert!(output.status.success(), "forge {arguments:?}: {output:?}");
    let root = directory.path().join(name);
    (directory, root)
}

fn read(root: &Path, relative: &str) -> String {
    std::fs::read_to_string(root.join(relative))
        .unwrap_or_else(|error| panic!("{relative} should be readable: {error}"))
}

fn read_yaml(root: &Path, relative: &str) -> Value {
    let content = read(root, relative);
    assert!(
        !content.contains('\t'),
        "{relative} contains a tab; YAML indents with spaces"
    );
    yaml::parse(&content).unwrap_or_else(|error| panic!("{relative} is not valid YAML: {error}"))
}

fn read_json(root: &Path, relative: &str) -> Value {
    serde_json::from_str(&read(root, relative))
        .unwrap_or_else(|error| panic!("{relative} is not valid JSON: {error}"))
}

/// The value at `path` (mapping keys), which must exist.
fn at<'a>(value: &'a Value, path: &[&str]) -> &'a Value {
    path.iter().fold(value, |value, key| {
        value
            .get(key)
            .unwrap_or_else(|| panic!("missing key {key:?} in {value}"))
    })
}

fn mapping<'a>(value: &'a Value, path: &[&str]) -> &'a Map<String, Value> {
    at(value, path)
        .as_object()
        .unwrap_or_else(|| panic!("{path:?} is not a mapping"))
}

fn strings(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .map(|item| {
                    item.as_str()
                        .unwrap_or_else(|| panic!("list item {item} is not a string"))
                        .to_owned()
                })
                .collect()
        })
        .unwrap_or_default()
}

fn text<'a>(value: &'a Value, path: &[&str]) -> &'a str {
    at(value, path)
        .as_str()
        .unwrap_or_else(|| panic!("{path:?} is not a string"))
}

/// `name:vX.Y.Z@sha256:<64 hex>`: an exact version and a digest.
fn is_pinned(image: &str) -> bool {
    let Some((reference, digest)) = image.split_once("@sha256:") else {
        return false;
    };
    let Some((repository, tag)) = reference.rsplit_once(':') else {
        return false;
    };
    let version = tag.strip_prefix('v').unwrap_or(tag);
    let parts: Vec<&str> = version.split('.').collect();
    !repository.is_empty()
        && repository.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"./-".contains(&byte)
        })
        && parts.len() == 3
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
        && digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[test]
fn every_application_with_docker_gets_the_stack() {
    for database in ["postgresql", "mysql", "mariadb", "sqlite3", "none"] {
        for shape in [&[][..], &["--api"][..]] {
            let mut extra = vec!["--database", database];
            extra.extend_from_slice(shape);
            let (_directory, root) = generate("shop", &extra);
            for relative in OBSERVABILITY_FILES {
                let path = root.join(relative);
                assert!(path.is_file(), "{database} {shape:?}: missing {relative}");
                #[cfg(unix)]
                if relative.starts_with("observability/") {
                    use std::os::unix::fs::PermissionsExt as _;
                    // Containers run as other users and read these bind mounts.
                    let mode = std::fs::metadata(&path)
                        .expect("metadata")
                        .permissions()
                        .mode();
                    assert_eq!(mode & 0o777, 0o644, "{relative} is {mode:o}");
                }
                match Path::new(relative).extension().and_then(|e| e.to_str()) {
                    Some("yaml" | "yml") => drop(read_yaml(&root, relative)),
                    Some("json") => drop(read_json(&root, relative)),
                    other => panic!("unexpected file type {other:?}"),
                }
            }
            #[cfg(unix)]
            for directory in [
                "observability",
                "observability/grafana",
                "observability/grafana/provisioning",
                "observability/grafana/provisioning/datasources",
                "observability/grafana/dashboards",
            ] {
                use std::os::unix::fs::PermissionsExt as _;
                let mode = std::fs::metadata(root.join(directory))
                    .expect("metadata")
                    .permissions()
                    .mode();
                assert_eq!(
                    mode & 0o777,
                    0o755,
                    "{directory} is {mode:o}; containers cannot traverse it"
                );
            }
        }
    }
}

/// The files containers read from bind mounts as their own users (the
/// observability stack, and the postgres image running
/// `docker/postgres/init.sql`) are `0644` under `0755` directories even
/// when `forge new` runs under a restrictive umask. The umask is set in a
/// child shell, never in this process, so parallel tests are unaffected.
#[cfg(unix)]
#[test]
fn bind_mounted_files_are_world_readable_whatever_the_umask() {
    use std::os::unix::fs::PermissionsExt as _;

    let directory = tempfile::tempdir().expect("temporary directory should be available");
    let output = Command::new("sh")
        .args([
            "-c",
            "umask 077 && exec \"$0\" \"$@\"",
            env!("CARGO_BIN_EXE_forge"),
            "new",
            "shop",
            "--database",
            "postgresql",
            "--skip-lockfile",
            "--forge-path",
            "../forge",
        ])
        .current_dir(directory.path())
        .output()
        .expect("sh should start");
    assert!(
        output.status.success(),
        "forge new under umask 077: {output:?}"
    );
    let root = directory.path().join("shop");
    let mode = |relative: &str| {
        std::fs::metadata(root.join(relative))
            .unwrap_or_else(|error| panic!("{relative}: {error}"))
            .permissions()
            .mode()
            & 0o777
    };
    // The umask applied: a file nothing mounts stays private.
    assert_eq!(mode("Cargo.toml"), 0o600, "umask 077 was not in effect");
    let mounted = OBSERVABILITY_FILES
        .iter()
        .copied()
        .filter(|relative| relative.starts_with("observability/"))
        .chain(["docker/postgres/init.sql"]);
    for relative in mounted {
        assert_eq!(mode(relative), 0o644, "{relative} is {:o}", mode(relative));
        let mut parent = Path::new(relative).parent();
        while let Some(directory) = parent.filter(|path| !path.as_os_str().is_empty()) {
            let display = directory.to_string_lossy();
            assert_eq!(mode(&display), 0o755, "{display} is {:o}", mode(&display));
            parent = directory.parent();
        }
    }
}

#[test]
fn skip_docker_omits_the_stack() {
    let (_directory, root) = generate("shop", &["--skip-docker"]);
    for relative in OBSERVABILITY_FILES {
        assert!(
            !root.join(relative).exists(),
            "{relative} should not be generated"
        );
    }
    assert!(!root.join("observability").exists());
    assert!(!read(&root, "README.md").contains("compose.observability.yaml"));
}

#[test]
fn the_overlay_is_opt_in_and_loopback_only() {
    let (_directory, root) = generate("shop", &[]);
    let default_compose = read(&root, "compose.yaml");
    for forbidden in [
        "grafana",
        "prometheus",
        "tempo",
        "loki",
        "otel",
        "FORGE_TELEMETRY",
        "GRAFANA_ADMIN_PASSWORD",
        "observability",
    ] {
        assert!(
            !default_compose.contains(forbidden),
            "the default compose.yaml mentions {forbidden:?}; the stack must stay opt-in"
        );
    }

    let overlay = read_yaml(&root, "compose.observability.yaml");
    assert!(
        overlay.get("profiles").is_none(),
        "top-level profiles is not valid compose"
    );
    let services = mapping(&overlay, &["services"]);
    let names: BTreeSet<&str> = services.keys().map(String::as_str).collect();
    let mut expected: BTreeSet<&str> = STACK_SERVICES.iter().copied().collect();
    expected.insert("app");
    assert_eq!(names, expected);

    for name in STACK_SERVICES {
        let service = &services[*name];
        let image = text(service, &["image"]);
        assert!(
            !image.contains("latest") && is_pinned(image),
            "{name} image {image:?} must be pinned to an exact version and digest"
        );
        for port in strings(service.get("ports")) {
            assert!(
                port.starts_with("127.0.0.1:"),
                "{name} publishes {port:?}; every published port must bind 127.0.0.1"
            );
        }
        for volume in strings(service.get("volumes")) {
            let source = volume.split(':').next().unwrap_or_default();
            if let Some(relative) = source.strip_prefix("./") {
                assert!(
                    root.join(relative).exists(),
                    "{name} mounts missing {source}"
                );
                assert!(volume.ends_with(":ro"), "{name} mounts {volume} read-write");
            }
        }
    }
    // Tempo and Loki stay off the host: only the collector's OTLP and the two
    // UIs are published.
    for name in ["tempo", "loki"] {
        assert!(
            services[name].get("ports").is_none(),
            "{name} must not publish ports"
        );
    }
    let grafana_ports = strings(services["grafana"].get("ports"));
    assert_eq!(
        grafana_ports,
        ["127.0.0.1:${GRAFANA_PORT:-3001}:3000"],
        "Grafana defaults to host port 3001: the app publishes 3000"
    );

    let dockerignore = read(&root, ".dockerignore");
    assert!(dockerignore.lines().any(|line| line == "/observability"));
    assert!(
        dockerignore
            .lines()
            .any(|line| line == "/compose.observability.yaml")
    );
    assert!(read(&root, ".github/dependabot.yml").contains("package-ecosystem: docker-compose"));

    assert!(
        read(&root, ".github/workflows/ci.yml").contains(
            "docker compose -f compose.yaml -f compose.observability.yaml config --quiet"
        ),
        "the generated CI validates the overlay"
    );

    let readme = read(&root, "README.md");
    for command in [
        "export GRAFANA_ADMIN_PASSWORD=\"$(openssl rand -base64 18)\"",
        "docker compose -f compose.yaml -f compose.observability.yaml up --build",
        "docker compose -f compose.yaml -f compose.observability.yaml down",
        "http://127.0.0.1:3001",
        "GRAFANA_ADMIN_PASSWORD must be set for down too",
    ] {
        assert!(readme.contains(command), "README.md lacks {command:?}");
    }
}

#[test]
fn grafana_is_secured_by_default_and_nothing_holds_a_secret() {
    let (_directory, root) = generate("shop", &["--database", "sqlite3"]);
    let overlay = read_yaml(&root, "compose.observability.yaml");
    let environment = at(&overlay, &["services", "grafana", "environment"]);
    let password = text(environment, &["GF_SECURITY_ADMIN_PASSWORD"]);
    assert!(
        password.starts_with("${GRAFANA_ADMIN_PASSWORD:?"),
        "admin password = {password:?}; it must be required with no default"
    );
    for (key, expected) in [
        ("GF_AUTH_ANONYMOUS_ENABLED", "false"),
        ("GF_USERS_ALLOW_SIGN_UP", "false"),
        ("GF_ANALYTICS_REPORTING_ENABLED", "false"),
        ("GF_ANALYTICS_CHECK_FOR_UPDATES", "false"),
        ("GF_ANALYTICS_CHECK_FOR_PLUGIN_UPDATES", "false"),
        ("GF_PLUGINS_PREINSTALL_DISABLED", "true"),
        ("GF_NEWS_NEWS_FEED_ENABLED", "false"),
    ] {
        assert_eq!(text(environment, &[key]), expected, "{key}");
    }

    for relative in OBSERVABILITY_FILES {
        for line in read(&root, relative).lines() {
            // Comments and ${VAR} references carry no value.
            if line.trim_start().starts_with('#') || line.contains("${") {
                continue;
            }
            assert!(
                !assigns_a_secret(line),
                "{relative} may contain a secret: {line:?}"
            );
        }
    }
}

/// Whether `line` assigns a literal value to something named like a secret
/// (`password`, `secret`, `token`, `api_key`, `apikey`, `api-key`).
fn assigns_a_secret(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    [
        "password", "secret", "token", "api_key", "api-key", "apikey",
    ]
    .iter()
    .any(|word| {
        lower.match_indices(word).any(|(index, _)| {
            let rest = lower[index + word.len()..].trim_start();
            let Some(rest) = rest.strip_prefix([':', '=']) else {
                return false;
            };
            let rest = rest.trim_start();
            let rest = rest.strip_prefix(['"', '\'']).unwrap_or(rest);
            rest.chars()
                .next()
                .is_some_and(|first| !first.is_whitespace() && !"\"'$#".contains(first))
        })
    })
}

#[test]
fn the_app_settings_are_valid_framework_configuration() {
    for database in ["postgresql", "sqlite3", "none"] {
        let (_directory, root) = generate("shop", &["--database", database]);
        let overlay = read_yaml(&root, "compose.observability.yaml");
        let app = at(&overlay, &["services", "app"]);
        assert!(
            app.get("logging").is_none(),
            "logs travel over OTLP, not a log driver"
        );
        assert_eq!(
            text(app, &["depends_on", "otel-collector", "condition"]),
            "service_started"
        );

        // What the container sees: the image's environment, compose.yaml's
        // and the overlay's.
        let mut variables: BTreeMap<String, String> = BTreeMap::from([
            ("FORGE_ENV".to_owned(), "production".to_owned()),
            ("FORGE_HTTP_ADDR".to_owned(), "0.0.0.0:3000".to_owned()),
        ]);
        let dockerfile = read(&root, "Dockerfile");
        assert!(dockerfile.contains("ENV FORGE_ENV=production"));
        assert!(dockerfile.contains("FORGE_HTTP_ADDR=0.0.0.0:3000"));
        let compose = read_yaml(&root, "compose.yaml");
        for environment in [
            mapping(&compose, &["services", "app", "environment"]),
            mapping(app, &["environment"]),
        ] {
            for (key, value) in environment {
                let value = value
                    .as_str()
                    .unwrap_or_else(|| panic!("{key} = {value} is not a string"));
                // `${NAME:-}` passes an exported secret through; Compose
                // interpolates it to empty when it is not exported.
                let value = if value == format!("${{{key}:-}}") {
                    ""
                } else {
                    value
                };
                variables.insert(key.clone(), value.to_owned());
            }
        }
        let config = AppConfig::from_vars(
            variables
                .iter()
                .map(|(key, value)| (OsString::from(key), OsString::from(value))),
        )
        .unwrap_or_else(|error| {
            panic!("{database}: the overlay's app environment is not valid configuration: {error}")
        });

        let telemetry = &config.telemetry;
        assert!(telemetry.enabled);
        assert_eq!(telemetry.service_name.as_deref(), Some("shop"));
        assert_eq!(telemetry.exporter, TelemetryExporter::Otlp);
        // Logs go over OTLP like traces and metrics, with the same service
        // name and endpoint.
        assert_eq!(telemetry.logs_exporter, TelemetryLogsExporter::Otlp);
        assert_eq!(telemetry.logs_level, TelemetryLogLevel::Info);
        assert_eq!(
            telemetry.otlp_endpoint.as_deref(),
            Some("http://otel-collector:4318")
        );
        assert!(telemetry.otlp_allow_plaintext);
        assert_eq!(telemetry.metrics_interval, Duration::from_secs(15));
        assert!(
            telemetry.metrics_addr.is_none(),
            "metrics are pushed, no port is opened"
        );
    }
}

#[test]
fn the_configuration_agrees_across_files() {
    let (_directory, root) = generate("shop", &[]);
    let overlay = read_yaml(&root, "compose.observability.yaml");
    let services = mapping(&overlay, &["services"]);

    // Collector: exporters point at services that exist, on ports they serve.
    let collector = read_yaml(&root, "observability/otel-collector.yaml");
    for (exporter, host) in [
        ("otlp_http/tempo", "tempo:4318"),
        ("otlp_http/prometheus", "prometheus:9090"),
        ("otlp_http/loki", "loki:3100"),
    ] {
        let endpoint = text(&collector, &["exporters", exporter, "endpoint"]);
        let authority = endpoint
            .strip_prefix("http://")
            .and_then(|rest| rest.split('/').next())
            .unwrap_or_default();
        assert_eq!(
            authority, host,
            "collector exporter {exporter} = {endpoint}"
        );
        let service = host.split(':').next().unwrap_or_default();
        assert!(
            services.contains_key(service),
            "{exporter} targets a missing service"
        );
    }
    for pipeline in ["traces", "metrics", "logs"] {
        assert_eq!(
            strings(Some(at(
                &collector,
                &["service", "pipelines", pipeline, "receivers"]
            ))),
            ["otlp"],
            "{pipeline} pipeline"
        );
    }
    assert_eq!(
        strings(services["otel-collector"].get("ports")),
        ["127.0.0.1:4318:4318"],
        "the collector publishes only OTLP/HTTP"
    );
    assert!(
        at(&collector, &["receivers"])
            .get("fluent_forward")
            .is_none()
    );
    assert!(
        at(&collector, &["processors"])
            .get("transform/logs")
            .is_none()
    );

    // Tempo and Loki serve what the collector and Grafana expect.
    let tempo = read_yaml(&root, "observability/tempo.yaml");
    assert_eq!(
        text(
            &tempo,
            &[
                "distributor",
                "receivers",
                "otlp",
                "protocols",
                "http",
                "endpoint"
            ]
        ),
        "0.0.0.0:4318"
    );
    assert_eq!(at(&tempo, &["server", "http_listen_port"]), 3200);
    let loki = read_yaml(&root, "observability/loki.yaml");
    assert_eq!(at(&loki, &["server", "http_listen_port"]), 3100);
    assert_eq!(
        at(&loki, &["limits_config", "allow_structured_metadata"]),
        true
    );
    let command = strings(services["prometheus"].get("command")).join(" ");
    assert!(
        command.contains("--web.enable-otlp-receiver"),
        "prometheus must enable the OTLP receiver the collector writes to"
    );

    // Grafana: data sources on reachable service names, correlation set up.
    let datasources = read_yaml(
        &root,
        "observability/grafana/provisioning/datasources/datasources.yaml",
    );
    let by_uid: BTreeMap<&str, &Value> = at(&datasources, &["datasources"])
        .as_array()
        .expect("a list of data sources")
        .iter()
        .map(|source| (text(source, &["uid"]), source))
        .collect();
    for (uid, url) in [
        ("prometheus", "http://prometheus:9090"),
        ("tempo", "http://tempo:3200"),
        ("loki", "http://loki:3100"),
    ] {
        let source = by_uid
            .get(uid)
            .unwrap_or_else(|| panic!("data source {uid} is not provisioned"));
        assert_eq!(text(source, &["url"]), url, "{uid}");
    }
    let to_logs = at(by_uid["tempo"], &["jsonData", "tracesToLogsV2"]);
    assert_eq!(text(to_logs, &["datasourceUid"]), "loki");
    // trace_id is structured metadata of the OTLP log record, not text of the
    // line, so the link filters on it instead of using a line filter.
    assert_eq!(at(to_logs, &["customQuery"]), true);
    let query = text(to_logs, &["query"]);
    assert!(query.contains("| trace_id=\"$${__span.traceId}\"") && query.contains("__tags"));
    let derived = at(by_uid["loki"], &["jsonData", "derivedFields"])
        .as_array()
        .expect("derived fields");
    assert_eq!(derived.len(), 1, "one trace_id derived field");
    assert_eq!(text(&derived[0], &["datasourceUid"]), "tempo");
    assert_eq!(text(&derived[0], &["matcherType"]), "label");
    assert_eq!(text(&derived[0], &["matcherRegex"]), "trace_id");

    let providers = read_yaml(
        &root,
        "observability/grafana/provisioning/dashboards/dashboards.yaml",
    );
    let provider = &at(&providers, &["providers"])[0];
    assert_eq!(
        text(provider, &["options", "path"]),
        "/etc/grafana/dashboards"
    );
    assert_eq!(
        text(
            &services["grafana"],
            &["environment", "GF_DASHBOARDS_DEFAULT_HOME_DASHBOARD_PATH"]
        ),
        "/etc/grafana/dashboards/forge-application.json"
    );
}

#[test]
fn the_dashboard_covers_every_signal_forge_rust_exports() {
    let (_directory, root) = generate("shop", &[]);
    let dashboard = read_json(&root, DASHBOARD);
    assert_eq!(text(&dashboard, &["uid"]), "forge-app");
    assert_eq!(text(&dashboard, &["title"]), "Forge application");

    let mut ids = BTreeSet::new();
    let mut queries = String::new();
    for panel in at(&dashboard, &["panels"]).as_array().expect("panels") {
        let id = at(panel, &["id"]).as_u64().expect("numeric id");
        assert!(ids.insert(id), "duplicate panel id {id}");
        let grid = at(panel, &["gridPos"]);
        let (x, w) = (at(grid, &["x"]).as_u64(), at(grid, &["w"]).as_u64());
        assert!(
            x.zip(w).is_some_and(|(x, w)| x + w <= 24),
            "panel {id} is off the grid"
        );
        if text(panel, &["type"]) == "row" {
            continue;
        }
        let title = text(panel, &["title"]);
        let uid = text(panel, &["datasource", "uid"]);
        assert!(
            ["prometheus", "loki", "tempo"].contains(&uid),
            "panel {title:?} uses unknown data source {uid:?}"
        );
        let targets = at(panel, &["targets"]).as_array().expect("targets");
        assert!(!targets.is_empty(), "panel {title:?} has no queries");
        for target in targets {
            for key in ["expr", "query"] {
                if let Some(query) = target.get(key).and_then(Value::as_str) {
                    queries.push_str(query);
                    queries.push('\n');
                }
            }
        }
    }
    for needle in [
        "http_server_request_duration_seconds_bucket",
        "http_server_request_duration_seconds_count",
        "http_server_active_requests",
        r#"http_response_status_class="5xx""#,
        "db_client_connection_count",
        "db_client_connection_max",
        "process_memory_usage_bytes",
        "process_memory_virtual_bytes",
        "process_cpu_time_seconds_total",
        "process_thread_count",
        "process_unix_file_descriptor_count",
        "tokio_worker_count",
        "tokio_task_count",
        "tokio_global_queue_depth",
        "forge_queue_job_duration_seconds_count",
        "forge_queue_job_duration_seconds_bucket",
        "forge_outbox_dispatch_duration_seconds_count",
        "forge_telemetry_spans_dropped_total",
        "forge_telemetry_logs_dropped_total",
        "forge_telemetry_export_errors_total",
        "forge_telemetry_metric_overflows_total",
        r#"{service_name=~"$service"}"#,
        r#"count_over_time({service_name=~"$service"}["#,
        r#"{ resource.service.name =~ "$service" }"#,
    ] {
        assert!(
            queries.contains(needle),
            "no dashboard panel queries {needle}"
        );
    }
    // Forge-Go-only signals Forge-Rust does not export.
    for absent in [
        "go_",
        "db_client_operation",
        "forge_db_pool",
        "forge_scheduler",
    ] {
        assert!(!queries.contains(absent), "the dashboard queries {absent}");
    }
}

#[test]
fn every_queried_metric_is_in_the_documented_catalogue() {
    let catalogue = documented_metrics();
    assert!(
        catalogue.contains("tokio_task_count"),
        "catalogue parsed: {catalogue:?}"
    );
    let (_directory, root) = generate("shop", &[]);
    let mut queried = BTreeSet::new();
    for relative in [
        DASHBOARD,
        "observability/grafana/provisioning/datasources/datasources.yaml",
    ] {
        queried.extend(metric_names(&read(&root, relative)));
    }
    assert!(queried.len() >= 15, "found {queried:?}");
    for name in &queried {
        assert!(
            catalogue.contains(name),
            "the stack queries {name}, which the metric catalogue of docs/observability.md does not list"
        );
    }
}

/// The Prometheus names in the metric table of docs/observability.md:
/// rows like `| `http.server.active_requests` (`http_server_active_requests`) | gauge | ... |`.
fn documented_metrics() -> BTreeSet<String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/observability.md");
    let document = std::fs::read_to_string(&path).expect("docs/observability.md");
    let section = document
        .split("\n### Metrics\n")
        .nth(1)
        .and_then(|rest| rest.split("\n### ").next())
        .expect("a Metrics section");
    section
        .lines()
        .filter(|line| line.starts_with("| `"))
        .filter_map(|line| {
            let cell = line.split('|').nth(1)?;
            let start = cell.find("(`")? + 2;
            let end = start + cell[start..].find("`)")?;
            Some(cell[start..end].to_owned())
        })
        .collect()
}

/// The metric names in PromQL text: `http_...`, `db_...`, `forge_...`,
/// `process_...`, `tokio_...` or `go_...` identifiers followed by a selector,
/// a range or a call, without the histogram's `_bucket`/`_count`/`_sum`.
fn metric_names(text: &str) -> BTreeSet<String> {
    let bytes = text.as_bytes();
    let identifier = |byte: u8| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_';
    let mut names = BTreeSet::new();
    let mut index = 0;
    while index < bytes.len() {
        if !identifier(bytes[index]) || (index > 0 && identifier(bytes[index - 1])) {
            index += 1;
            continue;
        }
        let start = index;
        while index < bytes.len() && identifier(bytes[index]) {
            index += 1;
        }
        let word = &text[start..index];
        let followed = bytes.get(index).is_some_and(|byte| b"{[(".contains(byte));
        let prefixed = ["http_", "db_", "forge_", "process_", "tokio_", "go_"]
            .iter()
            .any(|prefix| word.starts_with(prefix));
        if followed && prefixed {
            // A histogram's series end in `_seconds_bucket`/`_count`/`_sum`;
            // `db_client_connection_count` is a gauge of its own.
            let histogram = ["_bucket", "_count", "_sum"]
                .iter()
                .find_map(|suffix| word.strip_suffix(suffix))
                .filter(|base| base.ends_with("_seconds"));
            names.insert(histogram.unwrap_or(word).to_owned());
        }
    }
    names
}

#[test]
fn application_names_that_are_yaml_keywords_stay_strings() {
    // Names `forge new` accepts that YAML 1.1 reads as null or booleans.
    for name in ["null", "yes", "no", "on", "off", "y", "n"] {
        let (_directory, root) = generate(name, &["--database", "none"]);
        let overlay = read_yaml(&root, "compose.observability.yaml");
        assert_eq!(
            at(
                &overlay,
                &[
                    "services",
                    "app",
                    "environment",
                    "FORGE_TELEMETRY_SERVICE_NAME"
                ]
            ),
            &Value::String(name.to_owned()),
            "application {name}"
        );
    }
}

#[test]
fn the_yaml_reader_reads_the_subset_and_types_keywords() {
    let value = yaml::parse(
        "# comment\nroot:\n  plain: text # trailing\n  quoted: \"null\"\n  bare: null\n  flag: off\n  number: 3200\n  flow: [\"a\", b]\n  empty: {}\n  list:\n    - one\n    - key: value\n      other: 'it''s'\n  multi:\n    [\n      \"x, y\",\n      z,\n    ]\n",
    )
    .expect("parses");
    assert_eq!(
        value,
        serde_json::json!({"root": {
            "plain": "text", "quoted": "null", "bare": null, "flag": false, "number": 3200,
            "flow": ["a", "b"], "empty": {}, "list": ["one", {"key": "value", "other": "it's"}],
            "multi": ["x, y", "z"]
        }})
    );
    assert!(yaml::parse("a: [unclosed\n").is_err());
    assert!(yaml::parse("a: 1\n b: 2\n").is_err());
}

/// A reader for the block YAML the generated files use: mappings, sequences,
/// comments, single- and double-quoted scalars, flow sequences (also over
/// several lines) and `{}`. Plain `null`/`~` and YAML 1.1 booleans
/// (`yes`/`no`/`on`/`off`/`y`/`n`/`true`/`false`) are typed, so a value that
/// must stay a string has to be quoted. Anything else is an error.
mod yaml {
    use serde_json::{Map, Number, Value};

    pub(super) fn parse(source: &str) -> Result<Value, String> {
        let mut lines = logical_lines(source)?;
        if lines.is_empty() {
            return Ok(Value::Null);
        }
        let mut index = 0;
        let indent = lines[0].0;
        let value = block(&mut lines, &mut index, indent)?;
        match lines.get(index) {
            None => Ok(value),
            Some((_, text)) => Err(format!("unexpected line {text:?}")),
        }
    }

    /// Lines without comments or blanks, as (indent, text); a flow sequence
    /// that spans several lines is joined into one.
    fn logical_lines(source: &str) -> Result<Vec<(usize, String)>, String> {
        let mut lines: Vec<(usize, String)> = Vec::new();
        let mut open: Option<(usize, String)> = None;
        for raw in source.lines() {
            let text = strip_comment(raw);
            let trimmed = text.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Some((indent, mut joined)) = open.take() {
                joined.push(' ');
                joined.push_str(trimmed);
                if depth(&joined) == 0 {
                    lines.push((indent, joined));
                } else {
                    open = Some((indent, joined));
                }
                continue;
            }
            let indent = text.len() - text.trim_start().len();
            if depth(trimmed) > 0 {
                open = Some((indent, trimmed.to_owned()));
            } else {
                lines.push((indent, trimmed.to_owned()));
            }
        }
        match open {
            Some((_, text)) => Err(format!("unclosed flow sequence {text:?}")),
            None => Ok(lines),
        }
    }

    /// Calls `visit` with each character outside quoted scalars and its
    /// byte index, until it returns false. Double-quoted scalars may contain
    /// `\"`; single-quoted ones `''`.
    fn outside_quotes(text: &str, mut visit: impl FnMut(usize, char) -> bool) {
        let mut quote = None;
        let mut escaped = false;
        for (index, character) in text.char_indices() {
            match quote {
                Some('"') if escaped => escaped = false,
                Some('"') if character == '\\' => escaped = true,
                Some(open) if character == open => quote = None,
                Some(_) => {}
                None if character == '"' || character == '\'' => quote = Some(character),
                None => {
                    if !visit(index, character) {
                        return;
                    }
                }
            }
        }
    }

    /// Unclosed `[` outside quotes.
    fn depth(text: &str) -> i32 {
        let mut depth = 0;
        outside_quotes(text, |_, character| {
            match character {
                '[' => depth += 1,
                ']' => depth -= 1,
                _ => {}
            }
            true
        });
        depth
    }

    fn strip_comment(line: &str) -> &str {
        let mut end = line.len();
        outside_quotes(line, |index, character| {
            let comment =
                character == '#' && (index == 0 || line[..index].ends_with(char::is_whitespace));
            if comment {
                end = index;
            }
            !comment
        });
        &line[..end]
    }

    fn block(
        lines: &mut [(usize, String)],
        index: &mut usize,
        indent: usize,
    ) -> Result<Value, String> {
        if lines[*index].1 == "-" || lines[*index].1.starts_with("- ") {
            sequence(lines, index, indent)
        } else {
            mapping(lines, index, indent)
        }
    }

    fn sequence(
        lines: &mut [(usize, String)],
        index: &mut usize,
        indent: usize,
    ) -> Result<Value, String> {
        let mut items = Vec::new();
        while *index < lines.len() && lines[*index].0 == indent {
            let text = lines[*index].1.clone();
            let Some(rest) = text.strip_prefix('-') else {
                break;
            };
            let rest = rest.trim_start();
            if rest.is_empty() {
                *index += 1;
                items.push(nested(lines, index, indent)?);
            } else if key_value(rest).is_some() {
                // `- key: value` opens a mapping indented past the dash.
                let inner = indent + (text.len() - rest.len());
                lines[*index] = (inner, rest.to_owned());
                items.push(mapping(lines, index, inner)?);
            } else {
                items.push(scalar(rest)?);
                *index += 1;
            }
        }
        Ok(Value::Array(items))
    }

    fn mapping(
        lines: &mut [(usize, String)],
        index: &mut usize,
        indent: usize,
    ) -> Result<Value, String> {
        let mut map = Map::new();
        while *index < lines.len() && lines[*index].0 == indent {
            let text = lines[*index].1.clone();
            let (key, value) =
                key_value(&text).ok_or_else(|| format!("expected `key: value`, found {text:?}"))?;
            *index += 1;
            let value = if value.is_empty() {
                nested(lines, index, indent)?
            } else {
                scalar(value)?
            };
            if map.insert(key.to_owned(), value).is_some() {
                return Err(format!("duplicate key {key:?}"));
            }
        }
        if *index < lines.len() && lines[*index].0 > indent {
            return Err(format!("unexpected indentation at {:?}", lines[*index].1));
        }
        Ok(Value::Object(map))
    }

    /// The value of a `key:` with nothing after it: a deeper block, a
    /// sequence at the same indentation, or null.
    fn nested(
        lines: &mut [(usize, String)],
        index: &mut usize,
        indent: usize,
    ) -> Result<Value, String> {
        match lines.get(*index) {
            // A flow sequence on the lines below its key (compose's
            // multi-line healthcheck `test:`).
            Some((next, text)) if *next > indent && text.starts_with('[') => {
                let value = scalar(text)?;
                *index += 1;
                Ok(value)
            }
            Some((next, _)) if *next > indent => {
                let next = *next;
                block(lines, index, next)
            }
            Some((next, text)) if *next == indent && (text == "-" || text.starts_with("- ")) => {
                sequence(lines, index, indent)
            }
            _ => Ok(Value::Null),
        }
    }

    /// Splits `key: value` (or `key:`) with a plain key.
    fn key_value(text: &str) -> Option<(&str, &str)> {
        if text.starts_with(['"', '\'', '[', '{']) {
            return None;
        }
        let (key, value) = match text.find(": ") {
            Some(position) => (&text[..position], text[position + 2..].trim()),
            None => (text.strip_suffix(':')?, ""),
        };
        (!key.is_empty() && !key.contains(' ')).then_some((key, value))
    }

    fn scalar(text: &str) -> Result<Value, String> {
        if let Some(inner) = text.strip_prefix('"') {
            let inner = inner
                .strip_suffix('"')
                .ok_or_else(|| format!("unterminated string {text:?}"))?;
            let mut value = String::new();
            let mut characters = inner.chars();
            while let Some(character) = characters.next() {
                if character == '\\' {
                    match characters.next() {
                        Some(escaped @ ('"' | '\\')) => value.push(escaped),
                        other => return Err(format!("unsupported escape {other:?} in {text:?}")),
                    }
                } else {
                    value.push(character);
                }
            }
            return Ok(Value::String(value));
        }
        if let Some(inner) = text.strip_prefix('\'') {
            let inner = inner
                .strip_suffix('\'')
                .ok_or_else(|| format!("unterminated string {text:?}"))?;
            return Ok(Value::String(inner.replace("''", "'")));
        }
        if let Some(inner) = text.strip_prefix('[') {
            let inner = inner
                .strip_suffix(']')
                .ok_or_else(|| format!("unterminated flow sequence {text:?}"))?;
            return split_flow(inner)
                .into_iter()
                .map(|item| scalar(item.trim()))
                .collect::<Result<Vec<_>, _>>()
                .map(Value::Array);
        }
        if text == "{}" {
            return Ok(Value::Object(Map::new()));
        }
        if text.starts_with(['{', '&', '*', '!', '|', '>', '%', '@', '`']) {
            return Err(format!("unsupported YAML {text:?}"));
        }
        Ok(match text.to_ascii_lowercase().as_str() {
            "null" | "~" => Value::Null,
            "true" | "yes" | "on" | "y" => Value::Bool(true),
            "false" | "no" | "off" | "n" => Value::Bool(false),
            _ => match text.parse::<i64>() {
                Ok(number) => Value::Number(Number::from(number)),
                Err(_) => Value::String(text.to_owned()),
            },
        })
    }

    /// Splits flow sequence items at commas outside quotes; a trailing comma
    /// ends the sequence.
    fn split_flow(inner: &str) -> Vec<&str> {
        let mut items = Vec::new();
        let mut start = 0;
        outside_quotes(inner, |index, character| {
            if character == ',' {
                items.push(&inner[start..index]);
                start = index + 1;
            }
            true
        });
        items.push(&inner[start..]);
        items.retain(|item| !item.trim().is_empty());
        items
    }
}
