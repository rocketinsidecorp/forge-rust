//! Trace and span identifiers and the context headers, through the public
//! API: construction from bytes, validity, hex forms and propagation.

use forge_telemetry::context::{
    SpanContext, SpanId, TraceId, extract, normalize_tracestate, parse_traceparent,
};
use http::{HeaderMap, HeaderValue};

#[test]
fn identifiers_round_trip_through_bytes_and_lowercase_hex() {
    let trace = TraceId::from_bytes([
        0x4b, 0xf9, 0x2f, 0x35, 0x77, 0xb3, 0x4d, 0xa6, 0xa3, 0xce, 0x92, 0x9d, 0x0e, 0x0e, 0x47,
        0x36,
    ]);
    assert_eq!(trace.to_string(), "4bf92f3577b34da6a3ce929d0e0e4736");
    assert_eq!(
        format!("{trace:?}"),
        "TraceId(4bf92f3577b34da6a3ce929d0e0e4736)"
    );
    assert_eq!(TraceId::from_hex(&trace.to_string()), Some(trace));
    assert_eq!(
        TraceId::from_hex(&trace.to_string()).map(TraceId::to_bytes),
        Some(trace.to_bytes())
    );
    assert!(trace.is_valid());

    let span = SpanId::from_bytes([0x00, 0xf0, 0x67, 0xaa, 0x0b, 0xa9, 0x02, 0xb7]);
    assert_eq!(span.to_string(), "00f067aa0ba902b7");
    assert_eq!(format!("{span:?}"), "SpanId(00f067aa0ba902b7)");
    assert_eq!(SpanId::from_hex("00f067aa0ba902b7"), Some(span));
    assert_eq!(span.to_bytes()[1], 0xf0);
    assert!(span.is_valid());

    // All zeros is the invalid identifier; hex must be lowercase and exact.
    assert!(!TraceId::default().is_valid());
    assert!(!SpanId::default().is_valid());
    for text in [
        "",
        "4BF92F3577B34DA6A3CE929D0E0E4736",
        "4bf92f3577b34da6a3ce929d0e0e473",
        "zz",
    ] {
        assert_eq!(TraceId::from_hex(text), None, "{text:?}");
    }
    assert_eq!(SpanId::from_hex("00F067AA0BA902B7"), None);
    assert_eq!(SpanId::from_hex("00f067aa0ba902b"), None);
}

#[test]
fn contexts_propagate_with_their_trace_state_and_never_when_invalid() {
    let context = parse_traceparent("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01")
        .expect("valid");
    let mut headers = HeaderMap::new();
    headers.insert("tracestate", HeaderValue::from_static("stale=1"));
    context.inject(&mut headers);
    // No trace state on the context: an old one is removed, not forwarded.
    assert!(headers.get("tracestate").is_none());
    assert_eq!(
        headers["traceparent"],
        "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
    );

    let with_state = SpanContext {
        trace_state: "vendor=opaque".to_owned(),
        ..context
    };
    let mut headers = HeaderMap::new();
    with_state.inject(&mut headers);
    assert_eq!(headers["tracestate"], "vendor=opaque");
    let extracted = extract(&headers).expect("round trip");
    assert!(extracted.remote && extracted.is_sampled());
    assert_eq!(extracted.trace_state, "vendor=opaque");

    let mut untouched = HeaderMap::new();
    SpanContext::default().inject(&mut untouched);
    assert!(untouched.is_empty(), "an invalid context sends nothing");

    // Tracestate limits: too many members, too long, duplicate keys.
    let many = (0..33)
        .map(|i| format!("k{i}=v"))
        .collect::<Vec<_>>()
        .join(",");
    assert_eq!(normalize_tracestate(&many), None);
    assert_eq!(normalize_tracestate(&"a=b,".repeat(300)), None);
    assert_eq!(normalize_tracestate("a=1,a=2"), None);
    assert_eq!(normalize_tracestate(""), Some(String::new()));
    assert_eq!(
        normalize_tracestate(" a=1 , , b@sys=2 ").as_deref(),
        Some("a=1,b@sys=2")
    );
}
