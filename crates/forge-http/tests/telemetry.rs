//! Server instrumentation over real connections (ADR 0033).

mod support;

use forge_telemetry::{
    context::{TRACEPARENT_HEADER, parse_traceparent},
    prometheus,
};
use support::{INCOMING, get, serve, telemetry};

#[tokio::test]
async fn metrics_use_route_templates_and_status_classes() {
    let telemetry = telemetry(false, None);
    let running = serve(&telemetry).await;
    assert_eq!(get(running.address, "/users/123", &[]).await.0, 200);
    assert_eq!(
        get(running.address, "/users/456?secret=1", &[]).await.0,
        200
    );
    assert_eq!(get(running.address, "/missing/789", &[]).await.0, 404);
    assert_eq!(get(running.address, "/teapot", &[]).await.0, 418);
    assert_eq!(get(running.address, "/fail", &[]).await.0, 503);
    running.stop().await;

    let text = prometheus::render(telemetry.metrics());
    let count = |labels: &str| {
        text.lines()
            .find(|line| {
                line.starts_with("http_server_request_duration_seconds_count{")
                    && line.contains(labels)
            })
            .and_then(|line| line.rsplit(' ').next())
            .map(str::to_owned)
    };
    assert_eq!(
        count("http_route=\"/users/{id}\",http_response_status_class=\"2xx\"").as_deref(),
        Some("2"),
        "{text}"
    );
    // Unmatched requests, including those a layer answers, have no route.
    assert_eq!(
        count("http_route=\"\",http_response_status_class=\"4xx\"").as_deref(),
        Some("2"),
        "{text}"
    );
    assert_eq!(
        count("http_route=\"/fail\",http_response_status_class=\"5xx\"").as_deref(),
        Some("1")
    );
    for raw in ["/users/123", "/missing/789", "secret"] {
        assert!(!text.contains(raw), "{raw} leaked: {text}");
    }
    assert!(
        text.contains("http_server_active_requests{http_request_method=\"GET\"} 0"),
        "{text}"
    );
}

#[tokio::test]
async fn incoming_traceparent_is_ignored_unless_trusted() {
    let incoming = parse_traceparent(INCOMING).expect("valid");

    let untrusted = telemetry(false, None);
    let running = serve(&untrusted).await;
    let (_, body) = get(
        running.address,
        "/users/1",
        &[(TRACEPARENT_HEADER, INCOMING)],
    )
    .await;
    running.stop().await;
    let context = parse_traceparent(&body).expect("handlers see a trace context");
    assert_ne!(
        context.trace_id, incoming.trace_id,
        "a client cannot choose the trace"
    );

    let trusted = telemetry(true, None);
    let running = serve(&trusted).await;
    let (_, body) = get(
        running.address,
        "/users/1",
        &[(TRACEPARENT_HEADER, INCOMING)],
    )
    .await;
    let (_, invalid) = get(
        running.address,
        "/users/1",
        &[(TRACEPARENT_HEADER, "00-zz-00f067aa0ba902b7-01")],
    )
    .await;
    running.stop().await;
    let context = parse_traceparent(&body).expect("context");
    assert_eq!(context.trace_id, incoming.trace_id, "the trace continues");
    assert_ne!(context.span_id, incoming.span_id, "with a new server span");
    assert!(
        parse_traceparent(&invalid).is_some_and(|context| context.trace_id != incoming.trace_id)
    );
}

#[tokio::test]
async fn a_panicking_handler_is_recorded_as_a_server_error() {
    let telemetry = telemetry(false, None);
    let running = serve(&telemetry).await;
    let (status, _) = get(running.address, "/panic", &[]).await;
    assert_ne!(status, 200);
    running.stop().await;
    let text = prometheus::render(telemetry.metrics());
    assert!(
        text.contains(
            "http_server_request_duration_seconds_count{http_request_method=\"GET\",http_route=\"\",http_response_status_class=\"5xx\"} 1"
        ),
        "{text}"
    );
    assert!(
        text.contains("http_server_active_requests{http_request_method=\"GET\"} 0"),
        "{text}"
    );
}
