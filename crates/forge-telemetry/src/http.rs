//! HTTP server instrumentation: one server span per request and the
//! `http.server.request.duration` and `http.server.active_requests` metrics
//! with Forge-Go's labels.
//!
//! Recorded: the normalized method, the matched route template (never the
//! raw path), the status code and class, the scheme and protocol version,
//! and `error.type` for 5xx. Never recorded: path, query, headers, client
//! address, bodies.

use std::time::Instant;

use http::{HeaderMap, Method, Version};

use crate::{
    Telemetry,
    context::{SpanContext, extract},
    metrics::{DURATION_BUCKETS, Gauge, Histogram, MetricError},
    trace::{Attribute, Span, SpanKind, Tracer},
};

/// `http.server.request.duration`.
pub const METRIC_SERVER_DURATION: &str = "http.server.request.duration";
/// `http.server.active_requests`.
pub const METRIC_SERVER_ACTIVE: &str = "http.server.active_requests";
const HTTP_SCOPE: &str = "forge/http";

/// The method as OpenTelemetry records it: a standard method, or `_OTHER`,
/// so clients cannot create series with invented methods.
#[must_use]
pub fn normalize_method(method: &Method) -> &'static str {
    match method.as_str() {
        "GET" => "GET",
        "HEAD" => "HEAD",
        "POST" => "POST",
        "PUT" => "PUT",
        "PATCH" => "PATCH",
        "DELETE" => "DELETE",
        "CONNECT" => "CONNECT",
        "OPTIONS" => "OPTIONS",
        "TRACE" => "TRACE",
        "QUERY" => "QUERY",
        _ => "_OTHER",
    }
}

/// `1xx` to `5xx`, or `other`.
#[must_use]
pub fn status_class(status: u16) -> &'static str {
    match status {
        100..=199 => "1xx",
        200..=299 => "2xx",
        300..=399 => "3xx",
        400..=499 => "4xx",
        500..=599 => "5xx",
        _ => "other",
    }
}

fn protocol_version(version: Version) -> &'static str {
    match version {
        Version::HTTP_09 => "0.9",
        Version::HTTP_10 => "1.0",
        Version::HTTP_2 => "2",
        Version::HTTP_3 => "3",
        _ => "1.1",
    }
}

/// The HTTP server instruments; create once per server.
#[derive(Clone, Debug)]
pub struct ServerInstrumentation {
    tracer: Tracer,
    trust_incoming: bool,
    duration: Histogram,
    active: Gauge,
}

impl Telemetry {
    /// Registers the HTTP server instruments.
    pub fn http_server(&self) -> Result<ServerInstrumentation, MetricError> {
        let duration = self.metrics().histogram(
            METRIC_SERVER_DURATION,
            "Duration of HTTP server requests.",
            "s",
            DURATION_BUCKETS,
            &[
                "http.request.method",
                "http.route",
                "http.response.status_class",
            ],
        )?;
        let active = self.metrics().gauge(
            METRIC_SERVER_ACTIVE,
            "Number of HTTP server requests in progress.",
            "{request}",
            &["http.request.method"],
        )?;
        Ok(ServerInstrumentation {
            tracer: self.tracer(HTTP_SCOPE),
            trust_incoming: self.trust_incoming(),
            duration,
            active,
        })
    }
}

impl ServerInstrumentation {
    /// Starts the telemetry of one request: continues the incoming trace
    /// when `FORGE_TELEMETRY_TRUST_TRACEPARENT` is set, otherwise starts a
    /// new one, and counts the request as active.
    #[must_use]
    pub fn start(
        &self,
        method: &Method,
        version: Version,
        headers: &HeaderMap,
    ) -> RequestTelemetry {
        let started = Instant::now();
        let method = normalize_method(method);
        let parent = if self.trust_incoming {
            extract(headers)
        } else {
            None
        };
        let span = self.tracer.start(method, SpanKind::Server, parent.as_ref());
        if span.is_recording() {
            span.set_attributes([
                Attribute::string("http.request.method", method),
                Attribute::string("url.scheme", "http"),
                Attribute::string("network.protocol.version", protocol_version(version)),
            ]);
        }
        self.active.add(1.0, &[method]);
        RequestTelemetry {
            span: Some(span),
            method,
            started,
            duration: self.duration.clone(),
            active: self.active.clone(),
        }
    }
}

/// The telemetry of one request in progress. [`RequestTelemetry::finish`]
/// records it. Dropped unfinished (the client went away or shutdown aborted
/// the connection, so there is no response) it is a cancellation: the
/// active gauge goes down and the span ends with `error.type` `canceled`,
/// but no duration is recorded. A server that catches a panicking handler
/// calls `finish` with `500`.
#[derive(Debug)]
pub struct RequestTelemetry {
    span: Option<Span>,
    method: &'static str,
    started: Instant,
    duration: Histogram,
    active: Gauge,
}

impl RequestTelemetry {
    /// The server span's context: put it where handlers can parent their
    /// spans or propagate the trace, and in the request's log span.
    #[must_use]
    pub fn context(&self) -> SpanContext {
        self.span
            .as_ref()
            .map(|span| span.context().clone())
            .unwrap_or_default()
    }

    /// Ends the span and records the duration with the matched route
    /// template (`None` when no route matched) and the response status.
    pub fn finish(mut self, route: Option<&str>, status: u16) {
        self.record(route, status);
    }

    fn record(&mut self, route: Option<&str>, status: u16) {
        let Some(span) = self.span.take() else {
            return;
        };
        self.active.add(-1.0, &[self.method]);
        let route = route.unwrap_or("");
        if span.is_recording() {
            if !route.is_empty() {
                span.set_name(format!("{} {route}", self.method));
                span.set_attributes([Attribute::string("http.route", route)]);
            }
            span.set_attributes([Attribute::int(
                "http.response.status_code",
                i64::from(status),
            )]);
            if status >= 500 {
                span.record_error_type(&status.to_string());
            }
        }
        span.end();
        self.duration.record(
            self.started.elapsed().as_secs_f64(),
            &[self.method, route, status_class(status)],
        );
    }
}

impl Drop for RequestTelemetry {
    fn drop(&mut self) {
        let Some(span) = self.span.take() else {
            return;
        };
        self.active.add(-1.0, &[self.method]);
        if span.is_recording() {
            span.record_error_type("canceled");
        }
        span.end();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn methods_and_statuses_are_bounded() {
        assert_eq!(normalize_method(&Method::GET), "GET");
        assert_eq!(
            normalize_method(&Method::from_bytes(b"BREW").expect("token")),
            "_OTHER"
        );
        assert_eq!(status_class(204), "2xx");
        assert_eq!(status_class(503), "5xx");
        assert_eq!(status_class(42), "other");
    }

    #[test]
    fn a_dropped_request_is_a_cancellation_without_a_duration() {
        let telemetry = Telemetry::new(crate::Options::new("svc")).expect("valid");
        let http = telemetry.http_server().expect("instruments");
        drop(http.start(&Method::GET, Version::HTTP_11, &HeaderMap::new()));
        let text = crate::prometheus::render(telemetry.metrics());
        assert!(
            text.contains("http_server_active_requests{http_request_method=\"GET\"} 0"),
            "{text}"
        );
        assert!(
            !text.contains("http_server_request_duration_seconds_count"),
            "{text}"
        );
    }
}
