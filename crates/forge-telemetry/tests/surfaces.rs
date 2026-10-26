//! Telemetry's public surfaces that the exporters do not reach: how the
//! registry and instruments print, float counters, mail delivery spans, the
//! export error codes and the OTLP client's configuration.

use std::time::Duration;

use forge_config::{OtlpHeader, SecretString, TelemetryConfig};
use forge_telemetry::{
    ExportError, Options, OtlpClient, Registry, Signal, SpanKind, SpanSink, Telemetry,
    context::SpanContext,
    mail::OUTCOME_SENT,
    testing::{Behavior, Receiver},
};
use http::HeaderMap;

const FLUSH: Duration = Duration::from_secs(10);

#[test]
fn instruments_print_their_names_and_float_counters_ignore_bad_values() {
    let registry = Registry::new(100);
    let counter = registry
        .counter("test.count", "A count.", "{item}", &[])
        .expect("counter");
    let floats = registry
        .float_counter("test.seconds", "Seconds.", "s", &["kind"])
        .expect("float");
    let gauge = registry
        .gauge("test.level", "A level.", "1", &[])
        .expect("gauge");
    let histogram = registry
        .histogram("test.size", "A size.", "By", &[1.0, 10.0], &[])
        .expect("histogram");
    assert_eq!(format!("{counter:?}"), "Counter { name: \"test.count\" }");
    assert!(format!("{floats:?}").starts_with("FloatCounter"));
    assert!(format!("{gauge:?}").starts_with("Gauge"));
    assert!(format!("{histogram:?}").starts_with("Histogram"));
    let printed = format!("{registry:?}");
    for name in ["test.count", "test.seconds", "test.level", "test.size"] {
        assert!(printed.contains(name), "{printed}");
    }

    floats.add(1.5, &["a"]);
    floats.add(0.5, &["a"]);
    for ignored in [0.0, -1.0, f64::NAN, f64::INFINITY] {
        floats.add(ignored, &["a"]);
    }
    let text = forge_telemetry::prometheus::render(&registry);
    assert!(text.contains("test_seconds_total{kind=\"a\"} 2"), "{text}");
}

#[tokio::test]
async fn mail_deliveries_are_spans_only_inside_a_trace_and_always_a_duration() {
    let receiver = Receiver::start(Behavior::Accept).await;
    let mut options = Options::new("svc");
    options.spans = Some(SpanSink::Otlp(receiver.client(
        HeaderMap::new(),
        false,
        FLUSH,
    )));
    let telemetry = Telemetry::new(options).expect("valid");
    telemetry.start();
    let mail = telemetry.mail().expect("instruments");
    assert!(format!("{mail:?}").contains("MailInstrumentation"));

    // Outside a trace nothing starts a trace of its own.
    assert!(mail.start(None).is_none());
    assert!(mail.start(Some(&SpanContext::default())).is_none());
    mail.finish(None, "unavailable", Duration::from_millis(5));

    let parent = telemetry
        .tracer("test")
        .start("request", SpanKind::Server, None);
    let sent = mail.start(Some(parent.context())).expect("inside a trace");
    mail.finish(Some(sent), OUTCOME_SENT, Duration::from_millis(10));
    let failed = mail.start(Some(parent.context())).expect("inside a trace");
    mail.finish(Some(failed), "mail_rejected", Duration::from_millis(20));
    parent.end();
    telemetry.force_flush(FLUSH).await;

    let spans: Vec<serde_json::Value> = receiver
        .received("/v1/traces")
        .iter()
        .flat_map(|request| {
            request.body["resourceSpans"][0]["scopeSpans"][0]["spans"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        })
        .collect();
    let sends: Vec<&serde_json::Value> = spans
        .iter()
        .filter(|span| span["name"] == "mail send")
        .collect();
    assert_eq!(sends.len(), 2, "{spans:?}");
    assert_eq!(
        sends
            .iter()
            .filter(|span| span["status"]["code"] == 2)
            .count(),
        1
    );
    let text = forge_telemetry::prometheus::render(telemetry.metrics());
    for outcome in ["sent", "unavailable", "mail_rejected"] {
        assert!(
            text.contains(&format!("forge_mail_outcome=\"{outcome}\"")),
            "{text}"
        );
    }
}

#[tokio::test]
async fn export_failures_have_stable_codes_and_never_name_the_endpoint() {
    // A collector that never answers: the export deadline.
    let hanging = Receiver::start(Behavior::Hang).await;
    let client = hanging.client(HeaderMap::new(), false, Duration::from_millis(200));
    assert_eq!(client.timeout(), Duration::from_millis(200));
    assert!(format!("{client:?}").contains("OtlpClient"));
    let timeout = client
        .export(Signal::Traces, b"{}".to_vec())
        .await
        .unwrap_err();
    assert!(
        matches!(timeout, ExportError::Timeout { .. }),
        "{timeout:?}"
    );
    assert_eq!(timeout.error_type(), "timeout");
    assert!(!timeout.to_string().contains("127.0.0.1"));

    // Nothing listens: a transport failure. The port is bound and never
    // listening, so connecting is refused and nobody else can take it while
    // the test runs.
    let refusing = tokio::net::TcpSocket::new_v4().expect("socket");
    refusing
        .bind("127.0.0.1:0".parse().expect("address"))
        .expect("bind");
    let closed = format!("http://{}", refusing.local_addr().expect("addr"));
    let client = OtlpClient::new(&closed, HeaderMap::new(), false, Duration::from_secs(5))
        .expect("valid endpoint")
        .with_initial_backoff(Duration::from_millis(1));
    let transport = client
        .export(Signal::Logs, b"{}".to_vec())
        .await
        .unwrap_err();
    assert_eq!(transport.error_type(), "transport");
    assert!(!transport.to_string().contains("127.0.0.1"), "{transport}");

    // An answer with a status keeps the status as its code.
    let refusing = Receiver::start(Behavior::Always(403)).await;
    let status = refusing
        .client(HeaderMap::new(), false, FLUSH)
        .export(Signal::Metrics, b"{}".to_vec())
        .await
        .unwrap_err();
    assert_eq!(status.error_type(), "403");
}

#[test]
fn headers_of_the_configuration_must_be_valid_http() {
    let config = |name: &str, value: &str| TelemetryConfig {
        otlp_endpoint: Some("http://127.0.0.1:4318".to_owned()),
        otlp_headers: vec![OtlpHeader {
            name: name.to_owned(),
            value: SecretString::new(value),
        }],
        ..TelemetryConfig::default()
    };
    assert!(OtlpClient::from_config(&config("authorization", "Bearer token")).is_ok());
    assert!(OtlpClient::from_config(&config("bad name", "x")).is_err());
    assert!(OtlpClient::from_config(&config("authorization", "line\nbreak")).is_err());
    assert!(OtlpClient::from_config(&TelemetryConfig::default()).is_err());
}
