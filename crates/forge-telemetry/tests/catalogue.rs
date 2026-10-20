//! The metric catalogue is Forge-Go's: same OpenTelemetry names, kinds,
//! units, labels and Prometheus names, so one dashboard and one set of
//! alerts serve both frameworks (ADR 0033). Every metric of Forge-Go's
//! catalogue (docs/OBSERVABILITY.md there) is either registered here
//! identically or listed as a deviation that docs/observability.md explains.

use forge_telemetry::{
    Options, Telemetry,
    metrics::MetricKind,
    pool::PoolStats,
    prometheus::{self, prometheus_name},
};

/// A metric both frameworks record: name, kind, unit, labels, Prometheus
/// name.
type Shared = (
    &'static str,
    MetricKind,
    &'static str,
    &'static [&'static str],
    &'static str,
);

/// Forge-Go's catalogue entries Forge-Rust records identically.
const SHARED: &[Shared] = &[
    (
        "http.server.request.duration",
        MetricKind::Histogram,
        "s",
        &[
            "http.request.method",
            "http.route",
            "http.response.status_class",
        ],
        "http_server_request_duration_seconds",
    ),
    (
        "http.server.active_requests",
        MetricKind::Gauge,
        "{request}",
        &["http.request.method"],
        "http_server_active_requests",
    ),
    (
        "forge.queue.job.duration",
        MetricKind::Histogram,
        "s",
        &[
            "messaging.destination.name",
            "forge.job.kind",
            "forge.job.outcome",
        ],
        "forge_queue_job_duration_seconds",
    ),
    (
        "forge.outbox.dispatch.duration",
        MetricKind::Histogram,
        "s",
        &["forge.event.type", "forge.outbox.outcome"],
        "forge_outbox_dispatch_duration_seconds",
    ),
    (
        "db.client.connection.count",
        MetricKind::Gauge,
        "{connection}",
        &[
            "db.system.name",
            "db.client.connection.pool.name",
            "db.client.connection.state",
        ],
        "db_client_connection_count",
    ),
    (
        "db.client.connection.max",
        MetricKind::Gauge,
        "{connection}",
        &["db.system.name", "db.client.connection.pool.name"],
        "db_client_connection_max",
    ),
    (
        "forge.mail.send.duration",
        MetricKind::Histogram,
        "s",
        &["forge.mail.outcome"],
        "forge_mail_send_duration_seconds",
    ),
    (
        "forge.telemetry.spans.dropped",
        MetricKind::Counter,
        "{span}",
        &[],
        "forge_telemetry_spans_dropped_total",
    ),
    (
        "forge.telemetry.logs.dropped",
        MetricKind::Counter,
        "{log_record}",
        &[],
        "forge_telemetry_logs_dropped_total",
    ),
    (
        "forge.telemetry.export.errors",
        MetricKind::Counter,
        "{error}",
        &["signal"],
        "forge_telemetry_export_errors_total",
    ),
    (
        "forge.telemetry.metric.overflows",
        MetricKind::Counter,
        "{measurement}",
        &[],
        "forge_telemetry_metric_overflows_total",
    ),
];

/// Forge-Go's catalogue entries Forge-Rust does not record, each explained
/// under "Differences from Forge-Go" in docs/observability.md.
const NOT_PORTED: &[&str] = &[
    "http.client.request.duration",
    "db.client.operation.duration",
    "forge.scheduler.job.duration",
    "forge.db.pool.waits",
    "forge.db.pool.wait.duration",
    "go.goroutine.count",
    "go.memory.heap.alloc",
    "go.memory.heap.inuse",
    "go.memory.total",
    "go.memory.gc.goal",
    "go.memory.allocated",
    "go.gc.cycles",
    "go.gc.pause.duration",
    "go.processor.limit",
];

/// Forge-Go's whole catalogue, as its docs/OBSERVABILITY.md lists it.
const GO_CATALOGUE_SIZE: usize = 25;

/// Forge-Rust's own runtime metrics, in place of `go.*`.
const RUST_ONLY: &[&str] = &[
    "process.memory.usage",
    "process.memory.virtual",
    "process.cpu.time",
    "process.thread.count",
    "process.unix.file_descriptor.count",
    "tokio.worker.count",
    "tokio.task.count",
    "tokio.global_queue.depth",
];

fn every_instrument() -> (Telemetry, forge_telemetry::pool::PoolObservation) {
    let telemetry = Telemetry::new(Options::new("catalogue")).expect("valid");
    telemetry.http_server().expect("http");
    telemetry.jobs().expect("jobs");
    telemetry.outbox().expect("outbox");
    telemetry.mail().expect("mail");
    let pool = telemetry
        .observe_pool("postgresql", "default", PoolStats::default)
        .expect("pool");
    (telemetry, pool)
}

#[test]
fn shared_metrics_match_forge_go() {
    let (telemetry, _pool) = every_instrument();
    let registry = telemetry.metrics();
    for (name, kind, unit, labels, prometheus) in SHARED {
        let (registered_kind, registered_unit, registered_labels) = registry
            .definition(name)
            .unwrap_or_else(|| panic!("{name} is not registered"));
        assert_eq!(registered_kind, *kind, "{name}");
        assert_eq!(registered_unit, *unit, "{name}");
        assert_eq!(registered_labels, *labels, "{name}");
        assert_eq!(prometheus_name(name, unit, *kind), *prometheus, "{name}");
    }
    assert_eq!(SHARED.len() + NOT_PORTED.len(), GO_CATALOGUE_SIZE);
    for name in NOT_PORTED {
        assert!(
            registry.definition(name).is_none(),
            "{name} is ported now: move it to SHARED"
        );
    }
}

#[tokio::test]
async fn every_registered_metric_is_documented() {
    let (telemetry, _pool) = every_instrument();
    // Collection runs the observers, which register nothing new.
    let _ = prometheus::render(telemetry.metrics());
    let documentation = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/observability.md"
    ))
    .expect("docs/observability.md");
    let registered = telemetry.metrics().names();
    for name in &registered {
        assert!(
            documentation.contains(&format!("`{name}`")),
            "{name} is missing from docs/observability.md"
        );
        let (kind, unit, _) = telemetry.metrics().definition(name).expect("registered");
        let prometheus = prometheus_name(name, &unit, kind);
        assert!(
            documentation.contains(&format!("`{prometheus}`")),
            "{prometheus} is missing from docs/observability.md"
        );
    }
    for name in RUST_ONLY {
        if cfg!(target_os = "linux") || name.starts_with("tokio.") {
            assert!(
                registered.iter().any(|registered| registered == name),
                "{name}"
            );
        }
    }
    for name in NOT_PORTED {
        assert!(
            documentation.contains(&format!("`{name}`")),
            "the deviation {name} is not explained in docs/observability.md"
        );
    }
}
