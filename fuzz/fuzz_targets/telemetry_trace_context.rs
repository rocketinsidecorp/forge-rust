//! W3C trace context headers (forge_telemetry::context, ADR 0033): the
//! `traceparent` and `tracestate` a client sends to every instrumented
//! request.
//!
//! Properties: a parsed `traceparent` is valid and prints back in its
//! canonical form; a normalized `tracestate` is bounded, accepted again and
//! idempotent.
#![no_main]

use forge_telemetry::context::{normalize_tracestate, parse_traceparent};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    if let Some(context) = parse_traceparent(text) {
        assert!(context.is_valid());
        let printed = context.traceparent();
        assert_eq!(printed.len(), 55);
        let again = parse_traceparent(&printed).expect("a printed traceparent parses");
        assert_eq!(again.trace_id, context.trace_id);
        assert_eq!(again.span_id, context.span_id);
        assert_eq!(again.flags, context.flags);
    }
    if let Some(normalized) = normalize_tracestate(text) {
        assert!(normalized.len() <= 512);
        assert!(normalized.split(',').count() <= 32);
        assert_eq!(
            normalize_tracestate(&normalized).as_deref(),
            Some(normalized.as_str())
        );
    }
});
