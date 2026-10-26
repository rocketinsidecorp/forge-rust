//! The span API as an application uses it: attributes of every type and
//! their limits, renaming, the status rules (OK is final, an error carries a
//! type and never an unbounded message), spans that are not recorded, and
//! how a telemetry and its tracer print.

use std::time::Duration;

use forge_config::{OtlpCompression, TelemetryConfig, TelemetryExporter, TelemetryLogsExporter};
use forge_telemetry::{
    Attribute, Options, SpanKind, SpanSink, Status, Telemetry, TelemetryError,
    testing::{Behavior, Receiver},
    trace::{MAX_ATTRIBUTE_VALUE_LENGTH, MAX_SPAN_ATTRIBUTES},
};
use http::HeaderMap;
use serde_json::Value;

const FLUSH: Duration = Duration::from_secs(10);

fn attribute<'a>(span: &'a Value, key: &str) -> Option<&'a Value> {
    span["attributes"]
        .as_array()?
        .iter()
        .find(|attribute| attribute["key"] == key)
        .map(|attribute| &attribute["value"])
}

#[tokio::test]
async fn span_attributes_status_and_names_follow_the_rules() {
    let receiver = Receiver::start(Behavior::Accept).await;
    let mut options = Options::new("svc");
    options.spans = Some(SpanSink::Otlp(receiver.client(
        HeaderMap::new(),
        false,
        FLUSH,
    )));
    let telemetry = Telemetry::new(options).expect("valid");
    telemetry.start();
    let tracer = telemetry.tracer("test");
    assert_eq!(format!("{tracer:?}"), "Tracer { scope: \"test\", .. }");

    let span = tracer.start("before", SpanKind::Client, None);
    assert!(format!("{span:?}").contains("recording: true"));
    span.set_name("after");
    span.set_attributes([
        Attribute::string("string", "text"),
        Attribute::int("int", -3),
        Attribute::double("double", 2.5),
        Attribute::bool("bool", true),
        Attribute::string("", "an empty key is ignored"),
        Attribute::string("string", "replaced"),
        Attribute::string("long", "é".repeat(MAX_ATTRIBUTE_VALUE_LENGTH)),
    ]);
    // Beyond the limit, new attributes are counted and dropped; existing keys still update.
    span.set_attributes((0..MAX_SPAN_ATTRIBUTES + 3).map(|i| Attribute::int(format!("n{i}"), 1)));
    span.set_attributes([Attribute::int("int", 4)]);
    span.set_status(Status::Unset, "ignored");
    span.set_status(Status::Error, &"e".repeat(MAX_ATTRIBUTE_VALUE_LENGTH + 10));
    span.end();

    let ok = tracer.start("ok is final", SpanKind::Internal, None);
    ok.set_status(Status::Ok, "described");
    ok.record_error_type("timeout");
    assert_eq!(ok.context().flags & 1, 1);
    drop(ok);

    let failed = tracer.start("failed", SpanKind::Producer, None);
    failed.record_error_type("500");
    failed.end();
    telemetry.force_flush(FLUSH).await;

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
    let named = |name: &str| {
        spans
            .iter()
            .find(|span| span["name"] == name)
            .unwrap_or_else(|| panic!("{name}: {spans:?}"))
    };

    let renamed = named("after");
    assert_eq!(
        attribute(renamed, "string").expect("string")["stringValue"],
        "replaced"
    );
    assert_eq!(attribute(renamed, "int").expect("int")["intValue"], "4");
    assert_eq!(
        attribute(renamed, "double").expect("double")["doubleValue"],
        2.5
    );
    assert_eq!(attribute(renamed, "bool").expect("bool")["boolValue"], true);
    assert!(attribute(renamed, "").is_none());
    let long = attribute(renamed, "long").expect("long")["stringValue"]
        .as_str()
        .expect("text");
    assert!(long.len() <= MAX_ATTRIBUTE_VALUE_LENGTH && long.chars().all(|c| c == 'é'));
    assert_eq!(
        renamed["attributes"].as_array().expect("attributes").len(),
        MAX_SPAN_ATTRIBUTES
    );
    // Five attributes were stored first; of the 67 numbered ones only the
    // 59 that still fit were kept, so 8 were counted as dropped.
    assert_eq!(
        renamed["droppedAttributesCount"],
        MAX_SPAN_ATTRIBUTES + 3 - (MAX_SPAN_ATTRIBUTES - 5)
    );
    assert_eq!(renamed["status"]["code"], 2);
    assert_eq!(
        renamed["status"]["message"]
            .as_str()
            .expect("message")
            .len(),
        MAX_ATTRIBUTE_VALUE_LENGTH
    );

    let final_ok = named("ok is final");
    assert_eq!(final_ok["status"]["code"], 1);
    assert!(final_ok["status"]["message"].is_null() || final_ok["status"]["message"] == "");
    assert_eq!(
        attribute(final_ok, "error.type").expect("recorded")["stringValue"],
        "timeout"
    );
    let failed = named("failed");
    assert_eq!(failed["status"]["code"], 2);
    assert_eq!(failed["status"]["message"], "500");
}

#[tokio::test]
async fn unsampled_and_unexported_spans_keep_identifiers_but_record_nothing() {
    // No exporter: spans still carry identifiers for correlation.
    let telemetry = Telemetry::new(Options::new("svc")).expect("valid");
    let span = telemetry
        .tracer("test")
        .start("quiet", SpanKind::Server, None);
    assert!(!span.is_recording());
    assert!(span.context().is_valid());
    assert!(format!("{span:?}").contains("recording: false"));
    span.set_name("ignored");
    span.set_attributes([Attribute::int("ignored", 1)]);
    span.record_error_type("ignored");
    span.end();
    assert!(telemetry.log_layer().is_none());
    assert_eq!(telemetry.span_stats().exported, 0);
}

#[test]
fn options_and_configuration_are_validated_when_telemetry_is_built() {
    let invalid = |mutate: &dyn Fn(&mut Options)| {
        let mut options = Options::new("svc");
        mutate(&mut options);
        match Telemetry::new(options) {
            Err(TelemetryError::Invalid(reason)) => reason,
            other => panic!("{other:?}"),
        }
    };
    assert_eq!(
        invalid(&|o| o.service_name = String::new()),
        "the service name is required"
    );
    assert_eq!(
        invalid(&|o| o.sample_ratio = 1.5),
        "the sample ratio must be between 0 and 1"
    );
    assert_eq!(
        invalid(&|o| o.sample_ratio = -0.1),
        "the sample ratio must be between 0 and 1"
    );
    for mutate in [
        (&|o: &mut Options| o.max_queue = 0) as &dyn Fn(&mut Options),
        &|o| o.max_queue = (1 << 20) + 1,
        &|o| o.max_batch = 0,
    ] {
        assert_eq!(invalid(mutate), "queue sizes must be between 1 and 1048576");
    }
    let receiver = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(Receiver::start(Behavior::Accept));
    assert_eq!(
        invalid(&|o| {
            o.metrics = Some(receiver.client(HeaderMap::new(), false, FLUSH));
            o.metrics_interval = Duration::from_millis(500);
        }),
        "the metrics interval must be at least 1s"
    );
}

#[tokio::test]
async fn configuration_selects_the_exporters_and_names_the_service() {
    let mut config = TelemetryConfig {
        enabled: false,
        ..TelemetryConfig::default()
    };
    assert!(
        Telemetry::from_config(&config, forge_config::Environment::Test, "app", None)
            .expect("disabled")
            .is_none()
    );

    let resource = |telemetry: &Telemetry, key: &str| {
        telemetry
            .resource()
            .iter()
            .find(|attribute| attribute.key == key)
            .map(|attribute| format!("{:?}", attribute.value))
    };
    config.enabled = true;
    for (environment, label) in [
        (forge_config::Environment::Development, "development"),
        (forge_config::Environment::Test, "test"),
        (forge_config::Environment::Production, "production"),
    ] {
        let telemetry = Telemetry::from_config(&config, environment, "app", Some("1.0"))
            .expect("valid")
            .expect("enabled");
        assert!(
            resource(&telemetry, "deployment.environment.name")
                .is_some_and(|value| value.contains(label))
        );
        assert!(resource(&telemetry, "service.version").is_some_and(|v| v.contains("1.0")));
        assert!(format!("{telemetry:?}").contains("records_spans: false"));
    }
    // An empty application version is no version; configured names win.
    let telemetry =
        Telemetry::from_config(&config, forge_config::Environment::Test, "app", Some(""))
            .expect("valid")
            .expect("enabled");
    assert!(resource(&telemetry, "service.version").is_none());
    config.service_name = Some("configured".to_owned());
    config.service_version = Some("9.9".to_owned());
    let telemetry =
        Telemetry::from_config(&config, forge_config::Environment::Test, "app", Some("1.0"))
            .expect("valid")
            .expect("enabled");
    assert!(resource(&telemetry, "service.name").is_some_and(|v| v.contains("configured")));
    assert!(resource(&telemetry, "service.version").is_some_and(|v| v.contains("9.9")));

    // The log exporter writes spans to the log; no export task is needed.
    config.exporter = TelemetryExporter::Log;
    let telemetry = Telemetry::from_config(&config, forge_config::Environment::Test, "app", None)
        .expect("valid")
        .expect("enabled");
    assert!(format!("{telemetry:?}").contains("records_spans: true"));

    // OTLP: spans, metrics and logs all go to the collector.
    let receiver = Receiver::start(Behavior::Accept).await;
    config.exporter = TelemetryExporter::Otlp;
    config.logs_exporter = TelemetryLogsExporter::Otlp;
    config.otlp_endpoint = Some(receiver.endpoint());
    config.otlp_allow_plaintext = true;
    config.otlp_compression = OtlpCompression::None;
    let telemetry = Telemetry::from_config(&config, forge_config::Environment::Test, "app", None)
        .expect("valid")
        .expect("enabled");
    let debug = format!("{telemetry:?}");
    assert!(
        debug.contains("records_spans: true")
            && debug.contains("exports_logs: true")
            && debug.contains("pushes_metrics: true"),
        "{debug}"
    );
    assert!(telemetry.log_layer().is_some());
}
