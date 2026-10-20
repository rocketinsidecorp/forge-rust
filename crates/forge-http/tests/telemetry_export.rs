//! Spans and correlated log records of an instrumented server reach an
//! OTLP receiver (ADR 0033). Alone in its binary: it installs the global
//! `tracing` subscriber.

mod support;

use std::time::Duration;

use forge_telemetry::{
    context::{TRACEPARENT_HEADER, parse_traceparent},
    testing::{Behavior, Receiver},
};
use serde_json::Value;
use support::{INCOMING, get, serve, telemetry};
use tracing_subscriber::layer::SubscriberExt as _;

#[tokio::test]
async fn server_spans_and_correlated_logs_are_exported() {
    let receiver = Receiver::start(Behavior::Accept).await;
    let telemetry = telemetry(true, Some(&receiver));
    // The only test of this binary, so the subscriber can be global.
    tracing::subscriber::set_global_default(
        tracing_subscriber::registry().with(telemetry.log_layer().expect("layer")),
    )
    .expect("first subscriber");
    let running = serve(&telemetry).await;
    let (_, body) = get(
        running.address,
        "/users/7",
        &[(TRACEPARENT_HEADER, INCOMING)],
    )
    .await;
    assert_eq!(get(running.address, "/fail", &[]).await.0, 503);
    running.stop().await;
    telemetry.force_flush(Duration::from_secs(10)).await;
    let context = parse_traceparent(&body).expect("context");

    let spans: Vec<Value> = receiver
        .received("/v1/traces")
        .iter()
        .flat_map(|request| {
            request.body["resourceSpans"][0]["scopeSpans"][0]["spans"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        })
        .collect();
    let user = spans
        .iter()
        .find(|span| span["name"] == "GET /users/{id}")
        .expect("named after the route template");
    assert_eq!(user["kind"], 2);
    assert_eq!(user["traceId"], context.trace_id.to_string());
    assert_eq!(user["spanId"], context.span_id.to_string());
    assert_eq!(user["parentSpanId"], "00f067aa0ba902b7");
    let text = user.to_string();
    assert!(text.contains("\"http.route\""), "{text}");
    assert!(!text.contains("/users/7"), "{text}");
    let failed = spans
        .iter()
        .find(|span| span["name"] == "GET /fail")
        .expect("failed span");
    assert_eq!(failed["status"]["code"], 2);
    assert!(failed.to_string().contains("error.type"));

    let records: Vec<Value> = receiver
        .received("/v1/logs")
        .iter()
        .flat_map(|request| {
            request.body["resourceLogs"][0]["scopeLogs"][0]["logRecords"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        })
        .collect();
    let handled = records
        .iter()
        .find(|record| record["body"]["stringValue"] == "handled")
        .expect("the handler's line");
    assert_eq!(handled["traceId"], context.trace_id.to_string());
    assert_eq!(handled["spanId"], context.span_id.to_string());
}
