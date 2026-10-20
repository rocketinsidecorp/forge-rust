//! End-to-end export to an OTLP/HTTP receiver: a hyper server standing in
//! for an OpenTelemetry Collector records every request it gets.

use std::time::Duration;

use forge_config::{SecretString, TelemetryLogLevel};
use forge_telemetry::{
    Attribute, Options, SpanKind, SpanSink, Telemetry,
    logs::TRACE_ID_FIELD,
    testing::{Behavior, Receiver},
};
use http::{HeaderMap, HeaderValue};
use serde_json::Value;
use tokio::time::Instant;
use tracing_subscriber::layer::SubscriberExt as _;

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
