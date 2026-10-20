//! OTLP log export as a `tracing-subscriber` layer next to the standard
//! output formatter (Forge-Go's tee handler).
//!
//! The layer turns each event into an OpenTelemetry log record and queues it
//! without waiting. It sees only what the global `FORGE_LOG` filter lets
//! through, and records only the event's fields and the fields of its
//! enclosing spans, formatted the way the standard-output formatter formats
//! them (`Debug` for values recorded with `?` or `%`, so a
//! [`SecretString`](forge_config::SecretString) exports as `[REDACTED]`). It
//! therefore never exports more than standard output shows.
//!
//! Events of the `forge_telemetry` targets (export failures, the `log` span
//! exporter) are never exported, so a failing collector cannot feed itself.

use std::{fmt, sync::Arc, time::SystemTime};

use forge_config::TelemetryLogLevel;
use tracing::{
    Event, Level, Subscriber,
    field::{Field, Visit},
    span::{Attributes, Id, Record},
};
use tracing_subscriber::{Layer, layer::Context, registry::LookupSpan};

use crate::{
    batch::Batcher,
    context::{SpanId, TraceId},
    metrics::Counter,
    trace::{Attribute, Value, truncate},
};

/// Bodies are cut to this many bytes.
pub const MAX_LOG_BODY_LENGTH: usize = 4096;
/// Most attributes a record keeps; the rest are counted as dropped.
pub const MAX_LOG_ATTRIBUTES: usize = 64;
const MAX_STRING_LENGTH: usize = 1024;
/// Span fields that carry the trace context into log lines.
pub const TRACE_ID_FIELD: &str = "trace_id";
/// See [`TRACE_ID_FIELD`].
pub const SPAN_ID_FIELD: &str = "span_id";
/// Target prefix of the telemetry's own diagnostics, never exported.
pub const DIAGNOSTICS_TARGET: &str = "forge_telemetry";

/// A log record, as the exporter receives it.
#[derive(Clone, Debug)]
pub struct LogRecord {
    /// When the event happened.
    pub time: SystemTime,
    /// When the layer queued it.
    pub observed_time: SystemTime,
    /// OpenTelemetry severity number.
    pub severity_number: u8,
    /// `TRACE`, `DEBUG`, `INFO`, `WARN` or `ERROR`.
    pub severity_text: &'static str,
    /// The message, at most [`MAX_LOG_BODY_LENGTH`] bytes.
    pub body: String,
    /// Event and span fields, at most [`MAX_LOG_ATTRIBUTES`].
    pub attributes: Vec<Attribute>,
    /// Fields beyond the limit.
    pub dropped_attributes: u32,
    /// Trace of the enclosing span carrying `trace_id`.
    pub trace_id: Option<TraceId>,
    /// Span of the enclosing span carrying `span_id`.
    pub span_id: Option<SpanId>,
    /// W3C flags (unknown from span fields, so 0).
    pub flags: u8,
}

/// Severity number and text of a `tracing` level (OpenTelemetry ranges:
/// TRACE 1, DEBUG 5, INFO 9, WARN 13, ERROR 17).
#[must_use]
pub fn severity(level: &Level) -> (u8, &'static str) {
    match *level {
        Level::TRACE => (1, "TRACE"),
        Level::DEBUG => (5, "DEBUG"),
        Level::INFO => (9, "INFO"),
        Level::WARN => (13, "WARN"),
        _ => (17, "ERROR"),
    }
}

fn level_of(level: TelemetryLogLevel) -> Level {
    match level {
        TelemetryLogLevel::Debug => Level::DEBUG,
        TelemetryLogLevel::Info => Level::INFO,
        TelemetryLogLevel::Warn => Level::WARN,
        TelemetryLogLevel::Error => Level::ERROR,
    }
}

/// The OTLP log layer; build it with
/// [`Telemetry::log_layer`](crate::Telemetry::log_layer).
pub struct LogLayer {
    queue: Arc<Batcher<LogRecord>>,
    level: Level,
    dropped: Counter,
}

impl fmt::Debug for LogLayer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LogLayer")
            .field("level", &self.level)
            .finish_non_exhaustive()
    }
}

impl LogLayer {
    pub(crate) fn new(
        queue: Arc<Batcher<LogRecord>>,
        level: TelemetryLogLevel,
        dropped: Counter,
    ) -> Self {
        Self {
            queue,
            level: level_of(level),
            dropped,
        }
    }
}

/// Fields of a span, kept in its extensions.
#[derive(Default)]
struct SpanFields {
    trace_id: Option<TraceId>,
    span_id: Option<SpanId>,
    attributes: Vec<Attribute>,
}

impl Visit for SpanFields {
    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            TRACE_ID_FIELD => self.trace_id = TraceId::from_hex(value),
            SPAN_ID_FIELD => self.span_id = SpanId::from_hex(value),
            name => upsert(&mut self.attributes, name, Value::String(value.to_owned())),
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        let text = format!("{value:?}");
        match field.name() {
            TRACE_ID_FIELD => self.trace_id = TraceId::from_hex(&text),
            SPAN_ID_FIELD => self.span_id = SpanId::from_hex(&text),
            name => upsert(&mut self.attributes, name, Value::String(text)),
        }
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        upsert(&mut self.attributes, field.name(), Value::Int(value));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        upsert(&mut self.attributes, field.name(), unsigned(value));
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        upsert(&mut self.attributes, field.name(), Value::Double(value));
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        upsert(&mut self.attributes, field.name(), Value::Bool(value));
    }

    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        upsert(
            &mut self.attributes,
            field.name(),
            Value::String(value.to_string()),
        );
    }
}

fn unsigned(value: u64) -> Value {
    i64::try_from(value).map_or_else(|_| Value::String(value.to_string()), Value::Int)
}

fn upsert(attributes: &mut Vec<Attribute>, name: &str, value: Value) {
    if name.starts_with("log.") {
        // tracing-log metadata; the formatter does not print it either.
        return;
    }
    let value = match value {
        Value::String(text) => Value::String(truncate(text, MAX_STRING_LENGTH)),
        other => other,
    };
    match attributes
        .iter_mut()
        .find(|attribute| attribute.key == name)
    {
        Some(existing) => existing.value = value,
        None => attributes.push(Attribute {
            key: name.to_owned(),
            value,
        }),
    }
}

/// An event's message and fields.
#[derive(Default)]
struct EventFields {
    message: Option<String>,
    fields: SpanFields,
}

impl Visit for EventFields {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = Some(value.to_owned());
        } else {
            self.fields.record_str(field, value);
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() == "message" {
            self.message = Some(format!("{value:?}"));
        } else {
            self.fields.record_debug(field, value);
        }
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.fields.record_i64(field, value);
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.fields.record_u64(field, value);
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.fields.record_f64(field, value);
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.fields.record_bool(field, value);
    }

    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        self.fields.record_error(field, value);
    }
}

impl<S> Layer<S> for LogLayer
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    fn on_new_span(&self, attributes: &Attributes<'_>, id: &Id, context: Context<'_, S>) {
        let Some(span) = context.span(id) else {
            return;
        };
        let mut fields = SpanFields::default();
        attributes.record(&mut fields);
        span.extensions_mut().insert(fields);
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, context: Context<'_, S>) {
        let Some(span) = context.span(id) else {
            return;
        };
        let mut extensions = span.extensions_mut();
        if let Some(fields) = extensions.get_mut::<SpanFields>() {
            values.record(fields);
        }
    }

    // Filtering happens here, not in `enabled`: a layer's `enabled` would
    // filter the event for the standard-output layer too.
    fn on_event(&self, event: &Event<'_>, context: Context<'_, S>) {
        let metadata = event.metadata();
        if *metadata.level() > self.level || metadata.target().starts_with(DIAGNOSTICS_TARGET) {
            return;
        }
        let now = SystemTime::now();
        let mut visited = EventFields::default();
        event.record(&mut visited);
        let mut attributes = visited.fields.attributes;
        let mut trace_id = visited.fields.trace_id;
        let mut span_id = visited.fields.span_id;
        if let Some(scope) = context.event_scope(event) {
            // Innermost first: the nearest span wins for a repeated field.
            for span in scope {
                let extensions = span.extensions();
                let Some(fields) = extensions.get::<SpanFields>() else {
                    continue;
                };
                if trace_id.is_none() && fields.trace_id.is_some() {
                    trace_id = fields.trace_id;
                    span_id = fields.span_id;
                }
                for attribute in &fields.attributes {
                    if !attributes
                        .iter()
                        .any(|existing| existing.key == attribute.key)
                    {
                        attributes.push(attribute.clone());
                    }
                }
            }
        }
        let dropped = attributes.len().saturating_sub(MAX_LOG_ATTRIBUTES);
        attributes.truncate(MAX_LOG_ATTRIBUTES);
        let (severity_number, severity_text) = severity(metadata.level());
        let queued = self.queue.enqueue(LogRecord {
            time: now,
            observed_time: SystemTime::now(),
            severity_number,
            severity_text,
            body: truncate(visited.message.unwrap_or_default(), MAX_LOG_BODY_LENGTH),
            attributes,
            dropped_attributes: u32::try_from(dropped).unwrap_or(u32::MAX),
            trace_id: trace_id.filter(|id| id.is_valid()),
            span_id: span_id.filter(|id| id.is_valid()),
            flags: 0,
        });
        if !queued {
            self.dropped.add(1, &[]);
        }
    }
}
