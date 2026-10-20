//! Forge's telemetry: W3C trace context, a minimal tracer, metric instruments
//! with a cardinality cap, the Prometheus text format, an OTLP/HTTP JSON
//! exporter and a `tracing` layer that exports log records correlated with
//! traces (ADR 0033, docs/observability.md).
//!
//! It is the Rust port of Forge-Go's dependency-free `telemetry` package: the
//! same variables, metric names, labels, limits and wire format, so one
//! OpenTelemetry Collector, Prometheus and Grafana setup serves both
//! frameworks. It implements the subset of OpenTelemetry an application
//! needs over the HTTP stack Forge already ships (hyper, rustls) instead of
//! the OpenTelemetry SDK.
//!
//! Disabled telemetry (`FORGE_TELEMETRY_ENABLED=false`) is `None`: no
//! instrument, span or task exists. Nothing here records request or response
//! bodies, query strings, raw paths, headers, cookies, client addresses, SQL,
//! job payloads or error messages.
//!
//! Lifecycle: [`Telemetry::new`] builds the queues (no task yet), so the log
//! layer can be installed before the runtime exists; [`Telemetry::start`]
//! spawns the exporter tasks on the current Tokio runtime and keeps their
//! handles; [`Telemetry::shutdown`] flushes and joins them before a
//! deadline. No task is detached and every queue is bounded.

mod batch;
pub mod context;
pub mod http;
pub mod jobs;
pub mod logs;
pub mod metrics;
pub mod otlp;
pub mod pool;
pub mod process;
pub mod prometheus;
pub mod trace;

use std::{
    fmt,
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use forge_config::{
    Environment, TelemetryConfig, TelemetryExporter, TelemetryLogLevel, TelemetryLogsExporter,
};
use thiserror::Error;
use tokio::{sync::Notify, task::JoinHandle, time::Instant};
use tracing::{info, warn};

pub use batch::Stats;
pub use context::{SpanContext, SpanId, TraceId};
pub use logs::{LogLayer, LogRecord};
pub use metrics::{Counter, FloatCounter, Gauge, Histogram, MetricError, Registry};
pub use otlp::{ExportError, OtlpClient, Signal};
pub use trace::{Attribute, Span, SpanData, SpanKind, Status, Tracer};

use crate::{
    batch::{Batcher, ExportFn, FailureFn},
    context::TraceId as Id,
    trace::ratio_threshold,
};

/// Instrumentation scope of Forge's own instrumentation.
pub const SCOPE_NAME: &str = "forge";
/// Spans waiting for export, and log records waiting for export.
pub const DEFAULT_MAX_QUEUE: usize = 2048;
/// Most items in one export.
pub const DEFAULT_MAX_BATCH: usize = 512;
/// Longest a span waits before export.
pub const DEFAULT_SPAN_INTERVAL: Duration = Duration::from_secs(5);
/// Longest a log record waits before export (people tail logs).
pub const DEFAULT_LOG_INTERVAL: Duration = Duration::from_secs(1);

/// `forge.telemetry.spans.dropped`.
pub const METRIC_SPANS_DROPPED: &str = "forge.telemetry.spans.dropped";
/// `forge.telemetry.logs.dropped`.
pub const METRIC_LOGS_DROPPED: &str = "forge.telemetry.logs.dropped";
/// `forge.telemetry.export.errors`.
pub const METRIC_EXPORT_ERRORS: &str = "forge.telemetry.export.errors";

/// Target of the telemetry's own diagnostics; the log layer never exports
/// it.
const EXPORT_TARGET: &str = "forge_telemetry::export";
/// Target of the `log` span exporter.
const SPANS_TARGET: &str = "forge_telemetry::spans";

/// Where sampled spans go.
#[derive(Clone, Debug)]
pub enum SpanSink {
    /// OTLP `/v1/traces`.
    Otlp(Arc<OtlpClient>),
    /// One `info` event per span on the `forge_telemetry::spans` target
    /// (development).
    Log,
}

/// Settings of [`Telemetry::new`]; [`Options::new`] has the defaults.
#[derive(Clone, Debug)]
pub struct Options {
    /// `service.name`.
    pub service_name: String,
    /// `service.version`.
    pub service_version: Option<String>,
    /// `deployment.environment.name`.
    pub environment: Option<String>,
    /// Fraction of new traces sampled.
    pub sample_ratio: f64,
    /// Continue traces from incoming `traceparent`.
    pub trust_incoming: bool,
    /// Span exporter; `None` records no span (identifiers still exist).
    pub spans: Option<SpanSink>,
    /// Metric exporter (OTLP push every `metrics_interval`).
    pub metrics: Option<Arc<OtlpClient>>,
    /// Log exporter; `None` means [`Telemetry::log_layer`] is `None`.
    pub logs: Option<Arc<OtlpClient>>,
    /// Lowest exported log level.
    pub log_level: TelemetryLogLevel,
    /// Metric push interval.
    pub metrics_interval: Duration,
    /// Queue bound of spans and of log records.
    pub max_queue: usize,
    /// Batch size of spans and log records.
    pub max_batch: usize,
    /// Span export interval.
    pub span_interval: Duration,
    /// Log export interval.
    pub log_interval: Duration,
    /// Series limit of each instrument.
    pub max_series: usize,
}

impl Options {
    /// Defaults for `service_name`: nothing exported, metrics in memory.
    #[must_use]
    pub fn new(service_name: impl Into<String>) -> Self {
        Self {
            service_name: service_name.into(),
            service_version: None,
            environment: None,
            sample_ratio: 1.0,
            trust_incoming: false,
            spans: None,
            metrics: None,
            logs: None,
            log_level: TelemetryLogLevel::Info,
            metrics_interval: Duration::from_secs(60),
            max_queue: DEFAULT_MAX_QUEUE,
            max_batch: DEFAULT_MAX_BATCH,
            span_interval: DEFAULT_SPAN_INTERVAL,
            log_interval: DEFAULT_LOG_INTERVAL,
            max_series: metrics::DEFAULT_MAX_SERIES,
        }
    }
}

/// Telemetry could not be built.
#[derive(Debug, Error)]
pub enum TelemetryError {
    /// The OTLP exporter settings are unusable.
    #[error(transparent)]
    Otlp(#[from] otlp::OtlpConfigError),
    /// A built-in instrument could not be registered.
    #[error(transparent)]
    Metric(#[from] MetricError),
    /// An option is out of range.
    #[error("telemetry: {0}")]
    Invalid(&'static str),
}

/// Some signals did not finish exporting before the shutdown deadline; what
/// they still held was dropped and counted.
#[derive(Debug, Error)]
#[error("telemetry export did not finish before the shutdown deadline: {}", signals.join(", "))]
pub struct ShutdownError {
    /// `traces`, `metrics` and/or `logs`.
    pub signals: Vec<&'static str>,
}

/// The process's telemetry: tracer state, metrics registry and exporter
/// tasks. Cheap to clone; every clone is the same telemetry.
#[derive(Clone)]
pub struct Telemetry {
    inner: Arc<Inner>,
}

struct Inner {
    resource: Vec<Attribute>,
    threshold: u64,
    trust_incoming: bool,
    registry: Registry,
    spans: Option<Arc<Batcher<SpanData>>>,
    logs: Option<Arc<Batcher<LogRecord>>>,
    log_level: TelemetryLogLevel,
    metrics: Option<MetricsPush>,
    dropped_spans: Counter,
    dropped_logs: Counter,
    started: AtomicBool,
    shut_down: AtomicBool,
}

struct MetricsPush {
    client: Arc<OtlpClient>,
    interval: Duration,
    stop: Arc<Notify>,
    task: Mutex<Option<JoinHandle<()>>>,
    export_errors: Counter,
}

impl fmt::Debug for Telemetry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Telemetry")
            .field("resource", &self.inner.resource)
            .field("records_spans", &self.inner.spans.is_some())
            .field("exports_logs", &self.inner.logs.is_some())
            .field("pushes_metrics", &self.inner.metrics.is_some())
            .finish_non_exhaustive()
    }
}

impl Telemetry {
    /// Builds the telemetry `config` describes, or `None` when
    /// `FORGE_TELEMETRY_ENABLED=false`. `name` and `version` are the
    /// application's, used when `FORGE_TELEMETRY_SERVICE_NAME` and
    /// `FORGE_TELEMETRY_SERVICE_VERSION` are not set. No task starts until
    /// [`Telemetry::start`].
    pub fn from_config(
        config: &TelemetryConfig,
        environment: Environment,
        name: &str,
        version: Option<&str>,
    ) -> Result<Option<Self>, TelemetryError> {
        if !config.enabled {
            return Ok(None);
        }
        let otlp = if config.uses_otlp() {
            Some(Arc::new(OtlpClient::from_config(config)?))
        } else {
            None
        };
        let mut options = Options::new(config.service_name.as_deref().unwrap_or(name));
        options.service_version = config.service_version.clone().or_else(|| {
            version
                .filter(|version| !version.is_empty())
                .map(str::to_owned)
        });
        options.environment = Some(
            match environment {
                Environment::Development => "development",
                Environment::Test => "test",
                Environment::Production => "production",
            }
            .to_owned(),
        );
        options.sample_ratio = config.sample_ratio.get();
        options.trust_incoming = config.trust_traceparent;
        options.metrics_interval = config.metrics_interval;
        options.log_level = config.logs_level;
        match config.exporter {
            TelemetryExporter::None => {}
            TelemetryExporter::Log => options.spans = Some(SpanSink::Log),
            TelemetryExporter::Otlp => {
                options.spans = otlp.clone().map(SpanSink::Otlp);
                options.metrics = otlp.clone();
            }
        }
        if config.logs_exporter == TelemetryLogsExporter::Otlp {
            options.logs = otlp;
        }
        Self::new(options).map(Some)
    }

    /// Builds telemetry from explicit options.
    pub fn new(options: Options) -> Result<Self, TelemetryError> {
        if options.service_name.is_empty() {
            return Err(TelemetryError::Invalid("the service name is required"));
        }
        if !(0.0..=1.0).contains(&options.sample_ratio) {
            return Err(TelemetryError::Invalid(
                "the sample ratio must be between 0 and 1",
            ));
        }
        if options.max_queue == 0 || options.max_queue > 1 << 20 || options.max_batch == 0 {
            return Err(TelemetryError::Invalid(
                "queue sizes must be between 1 and 1048576",
            ));
        }
        if options.metrics_interval < Duration::from_secs(1) && options.metrics.is_some() {
            return Err(TelemetryError::Invalid(
                "the metrics interval must be at least 1s",
            ));
        }
        let mut resource = vec![
            Attribute::string("service.name", options.service_name.clone()),
            Attribute::string("telemetry.sdk.name", "forge"),
            Attribute::string("telemetry.sdk.language", "rust"),
        ];
        if let Some(version) = &options.service_version {
            resource.push(Attribute::string("service.version", version.clone()));
        }
        if let Some(environment) = &options.environment {
            resource.push(Attribute::string(
                "deployment.environment.name",
                environment.clone(),
            ));
        }
        let registry = Registry::new(options.max_series);
        let dropped_spans = registry.counter(
            METRIC_SPANS_DROPPED,
            "Spans dropped because the export queue was full or export failed.",
            "{span}",
            &[],
        )?;
        let dropped_logs = registry.counter(
            METRIC_LOGS_DROPPED,
            "Log records dropped because the export queue was full or export failed.",
            "{log_record}",
            &[],
        )?;
        let export_errors = registry.counter(
            METRIC_EXPORT_ERRORS,
            "Failed export calls.",
            "{error}",
            &["signal"],
        )?;
        process::register(&registry)?;

        let spans = options.spans.map(|sink| {
            let export = span_export(sink, resource.clone());
            Arc::new(Batcher::new(
                options.max_queue,
                options.max_batch,
                options.span_interval,
                export,
                failure(Signal::Traces, export_errors.clone(), dropped_spans.clone()),
            ))
        });
        let logs = options.logs.map(|client| {
            let resource = resource.clone();
            let export: ExportFn<LogRecord> = Arc::new(move |records: Vec<LogRecord>| {
                let client = Arc::clone(&client);
                let body = otlp::encode::logs(&resource, &records);
                Box::pin(async move {
                    client
                        .export(Signal::Logs, body)
                        .await
                        .map_err(|error| error.to_string())
                })
            });
            Arc::new(Batcher::new(
                options.max_queue,
                options.max_batch,
                options.log_interval,
                export,
                failure(Signal::Logs, export_errors.clone(), dropped_logs.clone()),
            ))
        });
        let metrics = options.metrics.map(|client| MetricsPush {
            client,
            interval: options.metrics_interval,
            stop: Arc::new(Notify::new()),
            task: Mutex::new(None),
            export_errors: export_errors.clone(),
        });
        Ok(Self {
            inner: Arc::new(Inner {
                resource,
                threshold: ratio_threshold(options.sample_ratio),
                trust_incoming: options.trust_incoming,
                registry,
                spans,
                logs,
                log_level: options.log_level,
                metrics,
                dropped_spans,
                dropped_logs,
                started: AtomicBool::new(false),
                shut_down: AtomicBool::new(false),
            }),
        })
    }

    /// Spawns the exporter tasks on the current Tokio runtime. Only the first
    /// call has an effect. Must be called inside a runtime.
    pub fn start(&self) {
        if self.inner.started.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Some(spans) = &self.inner.spans {
            spans.start();
        }
        if let Some(logs) = &self.inner.logs {
            logs.start();
        }
        if let Some(push) = &self.inner.metrics {
            let telemetry = self.clone();
            let handle = tokio::spawn(async move { telemetry.push_metrics_loop().await });
            *push.task.lock().unwrap_or_else(PoisonError::into_inner) = Some(handle);
        }
    }

    /// The metrics registry; applications register their own instruments
    /// here, with the same limits.
    #[must_use]
    pub fn metrics(&self) -> &Registry {
        &self.inner.registry
    }

    /// The tracer of an instrumentation scope (a crate or module name).
    #[must_use]
    pub fn tracer(&self, scope: &str) -> Tracer {
        Tracer::new(self.clone(), scope)
    }

    /// The resource attributes sent with every export.
    #[must_use]
    pub fn resource(&self) -> &[Attribute] {
        &self.inner.resource
    }

    /// Whether incoming `traceparent` headers are honored.
    #[must_use]
    pub fn trust_incoming(&self) -> bool {
        self.inner.trust_incoming
    }

    /// The `tracing` layer exporting log records, when
    /// `FORGE_TELEMETRY_LOGS_EXPORTER=otlp`. Add it next to the
    /// standard-output layer of the global subscriber.
    #[must_use]
    pub fn log_layer(&self) -> Option<LogLayer> {
        self.inner.logs.as_ref().map(|queue| {
            LogLayer::new(
                Arc::clone(queue),
                self.inner.log_level,
                self.inner.dropped_logs.clone(),
            )
        })
    }

    /// Span export counters (all zero without a span exporter).
    #[must_use]
    pub fn span_stats(&self) -> Stats {
        self.inner
            .spans
            .as_ref()
            .map(|spans| spans.stats())
            .unwrap_or_default()
    }

    /// Log export counters (all zero without a log exporter).
    #[must_use]
    pub fn log_stats(&self) -> Stats {
        self.inner
            .logs
            .as_ref()
            .map(|logs| logs.stats())
            .unwrap_or_default()
    }

    /// Exports queued spans and log records and a metric snapshot now,
    /// waiting at most `timeout` for each.
    pub async fn force_flush(&self, timeout: Duration) {
        let spans = async {
            if let Some(spans) = &self.inner.spans {
                spans.flush(timeout).await;
            }
        };
        let logs = async {
            if let Some(logs) = &self.inner.logs {
                logs.flush(timeout).await;
            }
        };
        let metrics = async {
            if self.inner.metrics.is_some() {
                let _ = tokio::time::timeout(timeout, self.push_metrics()).await;
            }
        };
        tokio::join!(spans, logs, metrics);
    }

    /// Exports what is queued and a final metric snapshot, then stops the
    /// exporter tasks, all before `deadline`. The signals stop concurrently,
    /// so a hung collector costs each at most the whole budget, not their
    /// sum. What is still queued at the deadline is dropped and counted; so
    /// is what is produced afterwards (lines logged after shutdown reach
    /// standard output only). Only the first call has an effect.
    pub async fn shutdown(&self, deadline: Instant) -> Result<(), ShutdownError> {
        if self.inner.shut_down.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        let spans = async {
            match &self.inner.spans {
                Some(spans) => spans.shutdown(deadline).await,
                None => true,
            }
        };
        let logs = async {
            match &self.inner.logs {
                Some(logs) => logs.shutdown(deadline).await,
                None => true,
            }
        };
        let metrics = async {
            let Some(push) = &self.inner.metrics else {
                return true;
            };
            push.stop.notify_one();
            let handle = push
                .task
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take();
            match handle {
                Some(mut handle) => match tokio::time::timeout_at(deadline, &mut handle).await {
                    Ok(_) => true,
                    Err(_elapsed) => {
                        handle.abort();
                        let _ = handle.await;
                        false
                    }
                },
                None => true,
            }
        };
        let (spans, logs, metrics) = tokio::join!(spans, logs, metrics);
        let signals: Vec<&'static str> = [(spans, "traces"), (metrics, "metrics"), (logs, "logs")]
            .into_iter()
            .filter(|(finished, _)| !finished)
            .map(|(_, signal)| signal)
            .collect();
        if signals.is_empty() {
            Ok(())
        } else {
            Err(ShutdownError { signals })
        }
    }

    pub(crate) fn sample_root(&self, trace_id: Id) -> bool {
        if self.inner.threshold >= 1 << 63 {
            return true;
        }
        let bytes = trace_id.to_bytes();
        let mut low = [0_u8; 8];
        low.copy_from_slice(&bytes[8..]);
        (u64::from_be_bytes(low) >> 1) < self.inner.threshold
    }

    pub(crate) fn records_spans(&self) -> bool {
        self.inner.spans.is_some()
    }

    pub(crate) fn export_span(&self, data: SpanData) {
        if let Some(spans) = &self.inner.spans
            && !spans.enqueue(data)
        {
            self.inner.dropped_spans.add(1, &[]);
        }
    }

    async fn push_metrics(&self) -> Result<(), ExportError> {
        let Some(push) = &self.inner.metrics else {
            return Ok(());
        };
        let snapshot = self.inner.registry.collect();
        let body = otlp::encode::metrics(&self.inner.resource, &snapshot);
        let result = push.client.export(Signal::Metrics, body).await;
        if result.is_err() {
            push.export_errors.add(1, &[Signal::Metrics.label()]);
        }
        result
    }

    async fn push_metrics_loop(self) {
        let Some(push) = &self.inner.metrics else {
            return;
        };
        let stop = Arc::clone(&push.stop);
        let mut ticker = tokio::time::interval(push.interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await;
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    if let Err(error) = self.push_metrics().await {
                        warn!(target: EXPORT_TARGET, error_type = %error.error_type(), %error, "telemetry: metric export failed");
                    }
                }
                () = stop.notified() => {
                    // The final snapshot carries the counts of the last interval.
                    if let Err(error) = self.push_metrics().await {
                        warn!(target: EXPORT_TARGET, error_type = %error.error_type(), %error, "telemetry: final metric export failed");
                    }
                    return;
                }
            }
        }
    }
}

/// Counts a failed batch and reports it on the telemetry's own target,
/// which the log layer never exports.
fn failure(signal: Signal, export_errors: Counter, dropped: Counter) -> FailureFn {
    Arc::new(move |count: usize, error: &str| {
        export_errors.add(1, &[signal.label()]);
        dropped.add(count as u64, &[]);
        warn!(
            target: EXPORT_TARGET,
            signal = signal.label(),
            count,
            error,
            "telemetry: export failed; batch dropped"
        );
    })
}

fn span_export(sink: SpanSink, resource: Vec<Attribute>) -> ExportFn<SpanData> {
    match sink {
        SpanSink::Otlp(client) => Arc::new(move |spans: Vec<SpanData>| {
            let client = Arc::clone(&client);
            let body = otlp::encode::traces(&resource, &spans);
            Box::pin(async move {
                client
                    .export(Signal::Traces, body)
                    .await
                    .map_err(|error| error.to_string())
            })
        }),
        SpanSink::Log => Arc::new(|spans: Vec<SpanData>| {
            for span in &spans {
                let duration_ms = span
                    .end
                    .duration_since(span.start)
                    .map_or(0.0, |elapsed| elapsed.as_secs_f64() * 1000.0);
                info!(
                    target: SPANS_TARGET,
                    trace_id = %span.context.trace_id,
                    span_id = %span.context.span_id,
                    parent_span_id = span.parent_span_id.map(|id| id.to_string()).unwrap_or_default(),
                    name = %span.name,
                    kind = ?span.kind,
                    status = ?span.status,
                    duration_ms,
                    attributes = ?span.attributes.iter().map(|a| (&a.key, &a.value)).collect::<Vec<_>>(),
                    "span"
                );
            }
            Box::pin(async { Ok(()) })
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sampling_follows_the_ratio() {
        let mut options = Options::new("svc");
        options.spans = Some(SpanSink::Log);
        options.sample_ratio = 0.0;
        let never = Telemetry::new(options.clone()).expect("valid");
        options.sample_ratio = 1.0;
        let always = Telemetry::new(options.clone()).expect("valid");
        options.sample_ratio = 0.5;
        let half = Telemetry::new(options).expect("valid");
        let tracer = |t: &Telemetry| t.tracer("test");
        let mut sampled = 0;
        for _ in 0..2000 {
            assert!(
                !tracer(&never)
                    .start("x", SpanKind::Internal, None)
                    .context()
                    .is_sampled()
            );
            assert!(
                tracer(&always)
                    .start("x", SpanKind::Internal, None)
                    .is_recording()
            );
            if tracer(&half)
                .start("x", SpanKind::Internal, None)
                .context()
                .is_sampled()
            {
                sampled += 1;
            }
        }
        assert!((800..1200).contains(&sampled), "{sampled}");
    }

    #[test]
    fn children_follow_their_parent() {
        let mut options = Options::new("svc");
        options.spans = Some(SpanSink::Log);
        options.sample_ratio = 0.0;
        let telemetry = Telemetry::new(options).expect("valid");
        let parent =
            context::parse_traceparent("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01")
                .expect("valid");
        let child = telemetry
            .tracer("t")
            .start("child", SpanKind::Server, Some(&parent));
        assert_eq!(child.context().trace_id, parent.trace_id);
        assert_ne!(child.context().span_id, parent.span_id);
        assert!(child.is_recording(), "a sampled parent is followed");
    }

    #[test]
    fn without_an_exporter_spans_have_ids_but_do_not_record() {
        let telemetry = Telemetry::new(Options::new("svc")).expect("valid");
        let span = telemetry.tracer("t").start("x", SpanKind::Server, None);
        assert!(span.context().is_valid());
        assert!(!span.is_recording());
        assert!(telemetry.log_layer().is_none());
    }

    #[test]
    fn options_are_validated() {
        assert!(Telemetry::new(Options::new("")).is_err());
        let mut options = Options::new("svc");
        options.sample_ratio = 2.0;
        assert!(Telemetry::new(options).is_err());
    }

    #[test]
    fn disabled_configuration_builds_nothing() {
        let config = TelemetryConfig {
            enabled: false,
            ..TelemetryConfig::default()
        };
        let built =
            Telemetry::from_config(&config, Environment::Development, "app", None).expect("valid");
        assert!(built.is_none());
        let default = Telemetry::from_config(
            &TelemetryConfig::default(),
            Environment::Development,
            "app",
            Some("1.0"),
        )
        .expect("valid")
        .expect("enabled by default");
        assert!(!default.records_spans());
        assert!(default.log_layer().is_none());
        assert!(
            default
                .resource()
                .iter()
                .any(|attribute| attribute.key == "service.version")
        );
    }
}
