//! The OTLP log layer: every field type, span fields (declared and recorded
//! later), the nearest span winning for a repeated field, trace context
//! from span or event fields, and the limits on attributes and text.

use std::time::Duration;

use forge_config::TelemetryLogLevel;
use forge_telemetry::{
    Options, Telemetry,
    logs::{MAX_LOG_ATTRIBUTES, MAX_LOG_BODY_LENGTH, SPAN_ID_FIELD, TRACE_ID_FIELD, severity},
    testing::{Behavior, Receiver},
};
use http::HeaderMap;
use serde_json::Value;
use tracing::Level;
use tracing_subscriber::layer::SubscriberExt as _;

const FLUSH: Duration = Duration::from_secs(10);
const TRACE: &str = "4bf92f3577b34da6a3ce929d0e0e4736";
const SPAN: &str = "00f067aa0ba902b7";

fn attribute<'a>(record: &'a Value, key: &str) -> Option<&'a Value> {
    record["attributes"]
        .as_array()?
        .iter()
        .find(|attribute| attribute["key"] == key)
        .map(|attribute| &attribute["value"])
}

#[derive(Debug)]
struct Failure;

impl std::fmt::Display for Failure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("disk on fire")
    }
}

impl std::error::Error for Failure {}

#[test]
fn severities_follow_the_opentelemetry_ranges() {
    assert_eq!(severity(&Level::TRACE), (1, "TRACE"));
    assert_eq!(severity(&Level::DEBUG), (5, "DEBUG"));
    assert_eq!(severity(&Level::INFO), (9, "INFO"));
    assert_eq!(severity(&Level::WARN), (13, "WARN"));
    assert_eq!(severity(&Level::ERROR), (17, "ERROR"));
}

#[test]
fn the_log_layer_exports_fields_spans_context_and_limits() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let receiver = runtime.block_on(Receiver::start(Behavior::Accept));
    let mut options = Options::new("svc");
    options.logs = Some(receiver.client(HeaderMap::new(), false, FLUSH));
    options.log_level = TelemetryLogLevel::Debug;
    let telemetry = Telemetry::new(options).expect("valid");
    {
        let _guard = runtime.enter();
        telemetry.start();
    }
    let subscriber =
        tracing_subscriber::registry().with(telemetry.log_layer().expect("log exporter"));
    let long_text = "x".repeat(2000);
    let long_body = "é".repeat(3000);
    tracing::subscriber::with_default(subscriber, || {
        tracing::callsite::rebuild_interest_cache();
        let error = Failure;
        tracing::info!(
            signed = -5_i64,
            unsigned = 7_u64,
            huge = u64::MAX,
            ratio = 1.5_f64,
            flag = false,
            text = "plain",
            failure = &error as &(dyn std::error::Error + 'static),
            shown = ?vec![1, 2],
            "field types"
        );
        tracing::info!(log.target = "bridge", kept = 1_i64, "bridged");
        tracing::info!(message = "message as a field", kept = 1_i64);
        tracing::debug!("a debug line");
        tracing::trace!("never exported");
        tracing::warn!("a warning");
        tracing::error!("an error");
        tracing::info!(long = long_text.as_str(), "{}", long_body);
        tracing::info!(
            f00 = 0_i64,
            f01 = 1_i64,
            f02 = 2_i64,
            f03 = 3_i64,
            f04 = 4_i64,
            f05 = 5_i64,
            f06 = 6_i64,
            f07 = 7_i64,
            f08 = 8_i64,
            f09 = 9_i64,
            f10 = 10_i64,
            f11 = 11_i64,
            f12 = 12_i64,
            f13 = 13_i64,
            f14 = 14_i64,
            f15 = 15_i64,
            f16 = 16_i64,
            f17 = 17_i64,
            f18 = 18_i64,
            f19 = 19_i64,
            f20 = 20_i64,
            f21 = 21_i64,
            f22 = 22_i64,
            f23 = 23_i64,
            f24 = 24_i64,
            f25 = 25_i64,
            f26 = 26_i64,
            f27 = 27_i64,
            f28 = 28_i64,
            f29 = 29_i64,
            f30 = 30_i64,
            f31 = 31_i64,
            f32 = 32_i64,
            f33 = 33_i64,
            f34 = 34_i64,
            f35 = 35_i64,
            f36 = 36_i64,
            f37 = 37_i64,
            f38 = 38_i64,
            f39 = 39_i64,
            f40 = 40_i64,
            f41 = 41_i64,
            f42 = 42_i64,
            f43 = 43_i64,
            f44 = 44_i64,
            f45 = 45_i64,
            f46 = 46_i64,
            f47 = 47_i64,
            f48 = 48_i64,
            f49 = 49_i64,
            f50 = 50_i64,
            f51 = 51_i64,
            f52 = 52_i64,
            f53 = 53_i64,
            f54 = 54_i64,
            f55 = 55_i64,
            f56 = 56_i64,
            f57 = 57_i64,
            f58 = 58_i64,
            f59 = 59_i64,
            f60 = 60_i64,
            f61 = 61_i64,
            f62 = 62_i64,
            f63 = 63_i64,
            f64 = 64_i64,
            f65 = 65_i64,
            f66 = 66_i64,
            f67 = 67_i64,
            f68 = 68_i64,
            f69 = 69_i64,
            "many fields"
        );

        let outer = tracing::info_span!("outer", depth = 1_i64, shared = "outer");
        let _outer = outer.enter();
        let inner = tracing::info_span!(
            "inner",
            shared = "inner",
            late = tracing::field::Empty,
            trace_id = TRACE,
            span_id = SPAN,
        );
        let _inner = inner.enter();
        tracing::info!("before the late field");
        inner.record("late", 9_i64);
        inner.record("late", 9_i64);
        tracing::info!("nearest span wins");
        tracing::info!(shared = "event", "event wins");
        tracing::info!(
            trace_id = "11111111111111111111111111111111",
            span_id = "2222222222222222",
            "event context"
        );
    });
    // A span whose context is all zeros carries none.
    tracing::subscriber::with_default(
        tracing_subscriber::registry().with(telemetry.log_layer().expect("log exporter")),
        || {
            tracing::callsite::rebuild_interest_cache();
            let span = tracing::info_span!(
                "zeros",
                trace_id = %"00000000000000000000000000000000",
                span_id = %"0000000000000000",
            );
            let _entered = span.enter();
            tracing::info!("without context");
        },
    );
    runtime.block_on(telemetry.force_flush(FLUSH));

    let exports = receiver.received("/v1/logs");
    let records: Vec<Value> = exports
        .iter()
        .flat_map(|export| {
            export.body["resourceLogs"][0]["scopeLogs"][0]["logRecords"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        })
        .collect();
    let by_body = |body: &str| -> &Value {
        records
            .iter()
            .find(|record| record["body"]["stringValue"] == body)
            .unwrap_or_else(|| panic!("no record {body:?} in {records:?}"))
    };

    let types = by_body("field types");
    assert_eq!(
        attribute(types, "signed").expect("signed")["intValue"],
        "-5"
    );
    assert_eq!(
        attribute(types, "unsigned").expect("unsigned")["intValue"],
        "7"
    );
    assert_eq!(
        attribute(types, "huge").expect("huge")["stringValue"],
        u64::MAX.to_string(),
        "an unsigned value beyond i64 is kept exactly, as text"
    );
    assert_eq!(
        attribute(types, "ratio").expect("ratio")["doubleValue"],
        1.5
    );
    assert_eq!(attribute(types, "flag").expect("flag")["boolValue"], false);
    assert_eq!(
        attribute(types, "text").expect("text")["stringValue"],
        "plain"
    );
    assert_eq!(
        attribute(types, "failure").expect("failure")["stringValue"],
        "disk on fire"
    );
    assert_eq!(
        attribute(types, "shown").expect("shown")["stringValue"],
        "[1, 2]"
    );
    assert_eq!(types["severityText"], "INFO");

    let bridged = by_body("bridged");
    assert!(
        attribute(bridged, "log.target").is_none(),
        "tracing-log metadata"
    );
    assert!(attribute(bridged, "kept").is_some());
    let plain = by_body("message as a field");
    assert!(attribute(plain, "kept").is_some());
    assert!(attribute(plain, "message").is_none());

    assert_eq!(by_body("a debug line")["severityNumber"], 5);
    assert_eq!(by_body("a warning")["severityNumber"], 13);
    assert_eq!(by_body("an error")["severityNumber"], 17);
    assert!(
        !records
            .iter()
            .any(|record| record["body"]["stringValue"] == "never exported"),
        "below the configured level"
    );

    let long = records
        .iter()
        .find(|record| attribute(record, "long").is_some())
        .expect("the long record");
    let body = long["body"]["stringValue"].as_str().expect("body");
    assert!(
        body.len() <= MAX_LOG_BODY_LENGTH && body.len() > MAX_LOG_BODY_LENGTH - 4,
        "{}",
        body.len()
    );
    assert!(
        body.chars().all(|c| c == 'é'),
        "cut on a character boundary"
    );
    let text = attribute(long, "long").expect("long")["stringValue"]
        .as_str()
        .expect("text");
    assert_eq!(text.len(), 1024);

    let many = by_body("many fields");
    assert_eq!(
        many["attributes"].as_array().expect("attributes").len(),
        MAX_LOG_ATTRIBUTES
    );
    assert_eq!(many["droppedAttributesCount"], 70 - MAX_LOG_ATTRIBUTES);

    let before = by_body("before the late field");
    assert!(attribute(before, "late").is_none());
    assert_eq!(
        attribute(before, "depth").expect("outer span")["intValue"],
        "1"
    );
    assert_eq!(before["traceId"], TRACE);
    assert_eq!(before["spanId"], SPAN);
    let nearest = by_body("nearest span wins");
    assert_eq!(
        attribute(nearest, "shared").expect("shared")["stringValue"],
        "inner"
    );
    assert_eq!(
        attribute(nearest, "late").expect("recorded later")["intValue"],
        "9"
    );
    assert!(attribute(nearest, TRACE_ID_FIELD).is_none());
    assert!(attribute(nearest, SPAN_ID_FIELD).is_none());
    let event = by_body("event wins");
    assert_eq!(
        attribute(event, "shared").expect("shared")["stringValue"],
        "event"
    );
    let context = by_body("event context");
    assert_eq!(context["traceId"], "11111111111111111111111111111111");
    assert_eq!(context["spanId"], "2222222222222222");

    let zeros = by_body("without context");
    assert!(
        zeros["traceId"].is_null() || zeros["traceId"] == "",
        "{zeros}"
    );
    drop(receiver);
    runtime.shutdown_timeout(Duration::from_secs(1));
}
