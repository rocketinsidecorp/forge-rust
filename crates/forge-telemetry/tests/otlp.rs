//! End-to-end export to an OTLP/HTTP receiver: a hyper server standing in
//! for an OpenTelemetry Collector records every request it gets.

use std::{
    convert::Infallible,
    io::Read as _,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};

use bytes::Bytes;
use forge_config::{SecretString, TelemetryLogLevel};
use forge_telemetry::{
    Attribute, Options, OtlpClient, SpanKind, SpanSink, Telemetry, logs::TRACE_ID_FIELD,
};
use http::{HeaderMap, HeaderValue, Request, Response, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper::{body::Incoming, service::service_fn};
use hyper_util::rt::TokioIo;
use serde_json::Value;
use tokio::{net::TcpListener, task::JoinHandle, time::Instant};
use tracing_subscriber::layer::SubscriberExt as _;

#[derive(Clone, Debug)]
struct Received {
    path: String,
    headers: HeaderMap,
    body: Value,
}

/// What the receiver answers.
#[derive(Clone, Copy)]
enum Behavior {
    Accept,
    /// 503 with `Retry-After: 0`-like immediate retry for the first `n`.
    FailFirst(usize, u16),
    /// Always this status.
    Always(u16),
    /// Never answers.
    Hang,
}

struct Receiver {
    address: SocketAddr,
    received: Arc<Mutex<Vec<Received>>>,
    attempts: Arc<Mutex<usize>>,
    task: JoinHandle<()>,
}

impl Receiver {
    async fn start(behavior: Behavior) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        let received = Arc::new(Mutex::new(Vec::new()));
        let attempts = Arc::new(Mutex::new(0_usize));
        let (store, count) = (Arc::clone(&received), Arc::clone(&attempts));
        let task = tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let (store, count) = (Arc::clone(&store), Arc::clone(&count));
                connections.spawn(async move {
                    let service = service_fn(move |request: Request<Incoming>| {
                        let (store, count) = (Arc::clone(&store), Arc::clone(&count));
                        async move {
                            let attempt = {
                                let mut count = count.lock().expect("lock");
                                *count += 1;
                                *count
                            };
                            let (parts, body) = request.into_parts();
                            let bytes = body.collect().await.expect("body").to_bytes();
                            let status = match behavior {
                                Behavior::Accept => 200,
                                Behavior::FailFirst(n, status) if attempt <= n => status,
                                Behavior::FailFirst(..) => 200,
                                Behavior::Always(status) => status,
                                Behavior::Hang => {
                                    std::future::pending::<()>().await;
                                    200
                                }
                            };
                            if status == 200 {
                                let json = if parts.headers.get("content-encoding")
                                    == Some(&HeaderValue::from_static("gzip"))
                                {
                                    let mut text = String::new();
                                    flate2::read::GzDecoder::new(&bytes[..])
                                        .read_to_string(&mut text)
                                        .expect("gzip");
                                    text
                                } else {
                                    String::from_utf8(bytes.to_vec()).expect("utf-8")
                                };
                                store.lock().expect("lock").push(Received {
                                    path: parts.uri.path().to_owned(),
                                    headers: parts.headers,
                                    body: serde_json::from_str(&json).expect("JSON body"),
                                });
                            }
                            let mut response = Response::new(Full::new(Bytes::from_static(b"{}")));
                            *response.status_mut() = StatusCode::from_u16(status).expect("status");
                            Ok::<_, Infallible>(response)
                        }
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
        });
        Self {
            address,
            received,
            attempts,
            task,
        }
    }

    fn endpoint(&self) -> String {
        format!("http://{}", self.address)
    }

    fn client(&self, headers: HeaderMap, gzip: bool, timeout: Duration) -> Arc<OtlpClient> {
        Arc::new(
            OtlpClient::new(&self.endpoint(), headers, gzip, timeout)
                .expect("valid endpoint")
                .with_initial_backoff(Duration::from_millis(10)),
        )
    }

    fn received(&self, path: &str) -> Vec<Received> {
        self.received
            .lock()
            .expect("lock")
            .iter()
            .filter(|received| received.path == path)
            .cloned()
            .collect()
    }

    fn attempts(&self) -> usize {
        *self.attempts.lock().expect("lock")
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn attribute<'a>(attributes: &'a Value, key: &str) -> Option<&'a Value> {
    attributes
        .as_array()?
        .iter()
        .find(|attribute| attribute["key"] == key)
        .map(|attribute| &attribute["value"])
}

const FLUSH: Duration = Duration::from_secs(10);

#[tokio::test]
async fn spans_are_exported_as_otlp_json() {
    let receiver = Receiver::start(Behavior::Accept).await;
    let mut headers = HeaderMap::new();
    let mut secret = HeaderValue::from_static("Bearer token-value");
    secret.set_sensitive(true);
    headers.insert("authorization", secret);
    let mut options = Options::new("billing");
    options.service_version = Some("1.2.3".to_owned());
    options.environment = Some("test".to_owned());
    options.spans = Some(SpanSink::Otlp(receiver.client(headers, true, FLUSH)));
    let telemetry = Telemetry::new(options).expect("valid");
    telemetry.start();

    let parent = forge_telemetry::context::parse_traceparent(
        "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
    )
    .map(|mut context| {
        context.remote = true;
        context
    });
    let span =
        telemetry
            .tracer("test/scope")
            .start("GET /users/{id}", SpanKind::Server, parent.as_ref());
    span.set_attributes([
        Attribute::string("http.route", "/users/{id}"),
        Attribute::int("http.response.status_code", 503),
    ]);
    span.record_error_type("503");
    span.end();
    telemetry.force_flush(FLUSH).await;

    let traces = receiver.received("/v1/traces");
    assert_eq!(traces.len(), 1, "{traces:?}");
    let request = &traces[0];
    assert_eq!(request.headers["content-type"], "application/json");
    assert_eq!(request.headers["content-encoding"], "gzip");
    assert_eq!(request.headers["authorization"], "Bearer token-value");
    let resource_spans = &request.body["resourceSpans"][0];
    let resource = &resource_spans["resource"]["attributes"];
    assert_eq!(
        attribute(resource, "service.name").expect("name")["stringValue"],
        "billing"
    );
    assert_eq!(
        attribute(resource, "service.version").expect("version")["stringValue"],
        "1.2.3"
    );
    assert_eq!(
        attribute(resource, "deployment.environment.name").expect("environment")["stringValue"],
        "test"
    );
    let scope = &resource_spans["scopeSpans"][0];
    assert_eq!(scope["scope"]["name"], "test/scope");
    let exported = &scope["spans"][0];
    assert_eq!(exported["traceId"], "4bf92f3577b34da6a3ce929d0e0e4736");
    assert_eq!(exported["parentSpanId"], "00f067aa0ba902b7");
    assert_eq!(exported["name"], "GET /users/{id}");
    assert_eq!(exported["kind"], 2);
    // Sampled flag, plus "parent is remote" known and set.
    assert_eq!(exported["flags"], 0x301);
    assert_eq!(exported["status"]["code"], 2);
    assert_eq!(exported["status"]["message"], "503");
    let attributes = &exported["attributes"];
    assert_eq!(
        attribute(attributes, "http.route").expect("route")["stringValue"],
        "/users/{id}"
    );
    assert_eq!(
        attribute(attributes, "http.response.status_code").expect("status")["intValue"],
        "503"
    );
    assert!(
        exported["startTimeUnixNano"]
            .as_str()
            .expect("string")
            .len()
            > 15
    );
    assert_eq!(telemetry.span_stats().exported, 1);
}

#[tokio::test]
async fn metrics_are_pushed_on_flush_and_at_shutdown() {
    let receiver = Receiver::start(Behavior::Accept).await;
    let mut options = Options::new("svc");
    options.metrics = Some(receiver.client(HeaderMap::new(), false, FLUSH));
    let telemetry = Telemetry::new(options).expect("valid");
    telemetry.start();
    let http = telemetry.http_server().expect("instruments");
    let request = http.start(
        &http::Method::GET,
        http::Version::HTTP_11,
        &HeaderMap::new(),
    );
    request.finish(Some("/users/{id}"), 200);
    telemetry.force_flush(FLUSH).await;

    let pushed = receiver.received("/v1/metrics");
    assert_eq!(pushed.len(), 1);
    let metrics = &pushed[0].body["resourceMetrics"][0]["scopeMetrics"][0];
    assert_eq!(metrics["scope"]["name"], "forge");
    let duration = metrics["metrics"]
        .as_array()
        .expect("metrics")
        .iter()
        .find(|metric| metric["name"] == "http.server.request.duration")
        .expect("duration histogram");
    assert_eq!(duration["unit"], "s");
    let point = &duration["histogram"]["dataPoints"][0];
    assert_eq!(duration["histogram"]["aggregationTemporality"], 2);
    assert_eq!(point["count"], "1");
    assert_eq!(
        attribute(&point["attributes"], "http.route").expect("route")["stringValue"],
        "/users/{id}"
    );
    assert_eq!(
        attribute(&point["attributes"], "http.response.status_class").expect("class")["stringValue"],
        "2xx"
    );
    assert_eq!(
        point["explicitBounds"].as_array().expect("bounds").len(),
        14
    );

    telemetry
        .shutdown(Instant::now() + FLUSH)
        .await
        .expect("final push in time");
    assert_eq!(
        receiver.received("/v1/metrics").len(),
        2,
        "a final snapshot"
    );
}

#[tokio::test]
async fn retryable_failures_are_retried_and_other_failures_counted() {
    let receiver = Receiver::start(Behavior::FailFirst(2, 503)).await;
    let mut options = Options::new("svc");
    options.spans = Some(SpanSink::Otlp(receiver.client(
        HeaderMap::new(),
        false,
        FLUSH,
    )));
    let telemetry = Telemetry::new(options).expect("valid");
    telemetry.start();
    telemetry
        .tracer("t")
        .start("a", SpanKind::Internal, None)
        .end();
    telemetry.force_flush(FLUSH).await;
    assert_eq!(receiver.attempts(), 3);
    assert_eq!(receiver.received("/v1/traces").len(), 1);

    let refusing = Receiver::start(Behavior::Always(400)).await;
    let mut options = Options::new("svc");
    options.spans = Some(SpanSink::Otlp(refusing.client(
        HeaderMap::new(),
        false,
        FLUSH,
    )));
    let telemetry = Telemetry::new(options).expect("valid");
    telemetry.start();
    telemetry
        .tracer("t")
        .start("a", SpanKind::Internal, None)
        .end();
    telemetry.force_flush(FLUSH).await;
    assert_eq!(refusing.attempts(), 1, "400 is not retried");
    assert_eq!(telemetry.span_stats().failed, 1);
    let text = forge_telemetry::prometheus::render(telemetry.metrics());
    assert!(
        text.contains("forge_telemetry_export_errors_total{signal=\"traces\"} 1"),
        "{text}"
    );
    assert!(
        text.contains("forge_telemetry_spans_dropped_total 1"),
        "{text}"
    );
}

#[tokio::test]
async fn a_full_queue_drops_and_counts_without_blocking() {
    let receiver = Receiver::start(Behavior::Hang).await;
    let mut options = Options::new("svc");
    options.max_queue = 8;
    options.max_batch = 4;
    options.spans = Some(SpanSink::Otlp(receiver.client(
        HeaderMap::new(),
        false,
        Duration::from_secs(30),
    )));
    let telemetry = Telemetry::new(options).expect("valid");
    telemetry.start();
    let tracer = telemetry.tracer("t");
    let started = std::time::Instant::now();
    for _ in 0..1000 {
        tracer.start("x", SpanKind::Internal, None).end();
    }
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "ending spans never waits for the exporter"
    );
    let stats = telemetry.span_stats();
    assert!(stats.dropped >= 1000 - 8 - 4, "{stats:?}");
    let text = forge_telemetry::prometheus::render(telemetry.metrics());
    assert!(
        text.contains("forge_telemetry_spans_dropped_total"),
        "{text}"
    );

    // The collector never answers: shutdown still ends at its deadline.
    let started = std::time::Instant::now();
    let error = telemetry
        .shutdown(Instant::now() + Duration::from_millis(300))
        .await
        .expect_err("export cannot finish");
    assert_eq!(error.signals, ["traces"]);
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn logs_are_exported_with_trace_context_and_redacted_values() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let receiver = runtime.block_on(Receiver::start(Behavior::Accept));
    let mut options = Options::new("svc");
    options.logs = Some(receiver.client(HeaderMap::new(), false, FLUSH));
    options.log_level = TelemetryLogLevel::Info;
    let telemetry = Telemetry::new(options).expect("valid");
    {
        let _guard = runtime.enter();
        telemetry.start();
    }
    let layer = telemetry.log_layer().expect("log exporter configured");
    let subscriber = tracing_subscriber::registry().with(layer);
    tracing::subscriber::with_default(subscriber, || {
        let span = tracing::info_span!(
            "http.request",
            trace_id = %"4bf92f3577b34da6a3ce929d0e0e4736",
            span_id = %"00f067aa0ba902b7",
            method = "GET",
        );
        let _entered = span.enter();
        let password = SecretString::new("hunter2-secret");
        tracing::info!(user = 42_u64, password = ?password, ok = true, "signed in");
        tracing::debug!("below the export level");
        tracing::warn!(target: "forge_telemetry::export", "diagnostics are never exported");
    });
    runtime.block_on(telemetry.force_flush(FLUSH));

    let logs = receiver.received("/v1/logs");
    assert_eq!(logs.len(), 1, "{logs:?}");
    let records = logs[0].body["resourceLogs"][0]["scopeLogs"][0]["logRecords"]
        .as_array()
        .expect("records")
        .clone();
    assert_eq!(records.len(), 1, "{records:?}");
    let record = &records[0];
    assert_eq!(record["body"]["stringValue"], "signed in");
    assert_eq!(record["severityNumber"], 9);
    assert_eq!(record["severityText"], "INFO");
    assert_eq!(record["traceId"], "4bf92f3577b34da6a3ce929d0e0e4736");
    assert_eq!(record["spanId"], "00f067aa0ba902b7");
    let attributes = &record["attributes"];
    assert_eq!(
        attribute(attributes, "user").expect("user")["intValue"],
        "42"
    );
    assert_eq!(attribute(attributes, "ok").expect("ok")["boolValue"], true);
    assert_eq!(
        attribute(attributes, "method").expect("span field")["stringValue"],
        "GET"
    );
    assert_eq!(
        attribute(attributes, "password").expect("password")["stringValue"],
        "[REDACTED]"
    );
    assert!(
        attribute(attributes, TRACE_ID_FIELD).is_none(),
        "a field of the record"
    );
    assert!(!logs[0].body.to_string().contains("hunter2-secret"));
    drop(receiver);
    runtime.shutdown_timeout(Duration::from_secs(1));
}

/// Counts the telemetry's own export diagnostics.
struct Diagnostics(Arc<Mutex<usize>>);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Diagnostics {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        if event.metadata().target() == "forge_telemetry::export" {
            *self.0.lock().expect("lock") += 1;
        }
    }
}

#[test]
fn a_failing_log_export_does_not_feed_itself() {
    // One thread: the exporter task runs where the subscriber is the default,
    // so its failure diagnostic reaches the log layer, which must skip it.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let receiver = runtime.block_on(Receiver::start(Behavior::Always(400)));
    let mut options = Options::new("svc");
    options.logs = Some(receiver.client(HeaderMap::new(), false, FLUSH));
    let telemetry = Telemetry::new(options).expect("valid");
    let diagnostics = Arc::new(Mutex::new(0));
    let subscriber = tracing_subscriber::registry()
        .with(telemetry.log_layer().expect("layer"))
        .with(Diagnostics(Arc::clone(&diagnostics)));
    let _default = tracing::subscriber::set_default(subscriber);
    runtime.block_on(async {
        telemetry.start();
        tracing::info!("one line");
        telemetry.force_flush(FLUSH).await;
        telemetry.force_flush(FLUSH).await;
    });
    assert_eq!(
        *diagnostics.lock().expect("lock"),
        1,
        "the failure was reported"
    );
    assert_eq!(
        receiver.attempts(),
        1,
        "only the application's line was exported"
    );
    let stats = telemetry.log_stats();
    assert_eq!(stats.failed, 1);
    assert_eq!(stats.exported, 0);
    let text = forge_telemetry::prometheus::render(telemetry.metrics());
    assert!(
        text.contains("forge_telemetry_export_errors_total{signal=\"logs\"} 1"),
        "{text}"
    );
    assert!(
        text.contains("forge_telemetry_logs_dropped_total 1"),
        "{text}"
    );
}
