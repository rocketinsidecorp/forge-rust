//! A minimal tracer: spans with a kind, attributes and a status, sampled
//! parent-based with OpenTelemetry's `TraceIdRatioBased` algorithm.

use std::{
    fmt,
    sync::{Arc, Mutex, PoisonError},
    time::SystemTime,
};

use crate::{
    Telemetry,
    context::{SpanContext, SpanId, new_span_id, new_trace_id},
};

/// Most attributes a span keeps; the rest are counted as dropped.
pub const MAX_SPAN_ATTRIBUTES: usize = 64;
/// String attribute values are cut to this many bytes.
pub const MAX_ATTRIBUTE_VALUE_LENGTH: usize = 1024;

/// An attribute value.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    /// A string, at most [`MAX_ATTRIBUTE_VALUE_LENGTH`] bytes.
    String(String),
    /// A signed integer.
    Int(i64),
    /// A float.
    Double(f64),
    /// A boolean.
    Bool(bool),
}

/// A key and its value.
#[derive(Clone, Debug, PartialEq)]
pub struct Attribute {
    /// Attribute name, such as `http.route`.
    pub key: String,
    /// Its value.
    pub value: Value,
}

impl Attribute {
    /// A string attribute, cut to [`MAX_ATTRIBUTE_VALUE_LENGTH`] bytes.
    #[must_use]
    pub fn string(key: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            value: Value::String(truncate(value.into(), MAX_ATTRIBUTE_VALUE_LENGTH)),
        }
    }

    /// An integer attribute.
    #[must_use]
    pub fn int(key: impl Into<String>, value: i64) -> Self {
        Self {
            key: key.into(),
            value: Value::Int(value),
        }
    }

    /// A float attribute.
    #[must_use]
    pub fn double(key: impl Into<String>, value: f64) -> Self {
        Self {
            key: key.into(),
            value: Value::Double(value),
        }
    }

    /// A boolean attribute.
    #[must_use]
    pub fn bool(key: impl Into<String>, value: bool) -> Self {
        Self {
            key: key.into(),
            value: Value::Bool(value),
        }
    }
}

/// Cuts `text` to at most `limit` bytes on a character boundary.
pub(crate) fn truncate(mut text: String, limit: usize) -> String {
    if text.len() > limit {
        let mut end = limit;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

/// The role of a span (OpenTelemetry `SpanKind`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SpanKind {
    /// In-process work.
    Internal = 1,
    /// Handling an incoming request.
    Server = 2,
    /// An outgoing request.
    Client = 3,
    /// Sending a message.
    Producer = 4,
    /// Processing a message.
    Consumer = 5,
}

/// Span status (OpenTelemetry `StatusCode`).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Status {
    /// Not set (the default).
    #[default]
    Unset = 0,
    /// Explicitly successful.
    Ok = 1,
    /// Failed; the description is an error type, never a message.
    Error = 2,
}

/// An ended span, as exporters receive it.
#[derive(Clone, Debug)]
pub struct SpanData {
    /// Instrumentation scope.
    pub scope: Arc<str>,
    /// Identity.
    pub context: SpanContext,
    /// Parent span, if any.
    pub parent_span_id: Option<SpanId>,
    /// Whether the parent came from another process.
    pub parent_remote: bool,
    /// Span name.
    pub name: String,
    /// Kind.
    pub kind: SpanKind,
    /// Start time.
    pub start: SystemTime,
    /// End time.
    pub end: SystemTime,
    /// Attributes, at most [`MAX_SPAN_ATTRIBUTES`].
    pub attributes: Vec<Attribute>,
    /// Attributes beyond the limit.
    pub dropped_attributes: u32,
    /// Status.
    pub status: Status,
    /// Status description (an error type).
    pub status_description: String,
}

/// Starts spans for one instrumentation scope.
#[derive(Clone)]
pub struct Tracer {
    telemetry: Telemetry,
    scope: Arc<str>,
}

impl fmt::Debug for Tracer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Tracer")
            .field("scope", &self.scope)
            .finish_non_exhaustive()
    }
}

impl Tracer {
    pub(crate) fn new(telemetry: Telemetry, scope: &str) -> Self {
        Self {
            telemetry,
            scope: Arc::from(scope),
        }
    }

    /// Starts a span. With a valid `parent` it joins that trace and follows
    /// its sampling decision; otherwise it starts a new trace, sampled with
    /// `FORGE_TELEMETRY_SAMPLE_RATIO`. A span is recorded only when it is
    /// sampled and a span exporter is configured; otherwise it still has
    /// identifiers for log correlation and propagation.
    #[must_use]
    pub fn start(
        &self,
        name: impl Into<String>,
        kind: SpanKind,
        parent: Option<&SpanContext>,
    ) -> Span {
        let (context, parent_span_id, parent_remote) =
            match parent.filter(|parent| parent.is_valid()) {
                Some(parent) => (
                    SpanContext {
                        trace_id: parent.trace_id,
                        span_id: new_span_id(),
                        flags: parent.flags,
                        trace_state: parent.trace_state.clone(),
                        remote: false,
                    },
                    Some(parent.span_id),
                    parent.remote,
                ),
                None => {
                    let trace_id = new_trace_id();
                    let flags = u8::from(self.telemetry.sample_root(trace_id));
                    (
                        SpanContext {
                            trace_id,
                            span_id: new_span_id(),
                            flags,
                            trace_state: String::new(),
                            remote: false,
                        },
                        None,
                        false,
                    )
                }
            };
        let recording = (context.is_sampled() && self.telemetry.records_spans()).then(|| {
            Mutex::new(Recording {
                name: name.into(),
                kind,
                start: SystemTime::now(),
                attributes: Vec::new(),
                dropped_attributes: 0,
                status: Status::Unset,
                status_description: String::new(),
            })
        });
        Span {
            tracer: self.clone(),
            context,
            parent_span_id,
            parent_remote,
            recording,
            ended: false,
        }
    }
}

struct Recording {
    name: String,
    kind: SpanKind,
    start: SystemTime,
    attributes: Vec<Attribute>,
    dropped_attributes: u32,
    status: Status,
    status_description: String,
}

/// A started span. It ends, and is queued for export when recording, on
/// [`Span::end`] or when dropped.
pub struct Span {
    tracer: Tracer,
    context: SpanContext,
    parent_span_id: Option<SpanId>,
    parent_remote: bool,
    recording: Option<Mutex<Recording>>,
    ended: bool,
}

impl fmt::Debug for Span {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Span")
            .field("context", &self.context)
            .field("recording", &self.recording.is_some())
            .finish_non_exhaustive()
    }
}

impl Span {
    /// The span's identity, to propagate or to parent child spans.
    #[must_use]
    pub fn context(&self) -> &SpanContext {
        &self.context
    }

    /// Whether attributes and status are kept for export.
    #[must_use]
    pub fn is_recording(&self) -> bool {
        self.recording.is_some()
    }

    fn with_recording(&self, apply: impl FnOnce(&mut Recording)) {
        if let Some(recording) = &self.recording {
            apply(&mut recording.lock().unwrap_or_else(PoisonError::into_inner));
        }
    }

    /// Renames the span.
    pub fn set_name(&self, name: impl Into<String>) {
        self.with_recording(|recording| recording.name = name.into());
    }

    /// Adds or replaces attributes; beyond [`MAX_SPAN_ATTRIBUTES`] they are
    /// counted as dropped.
    pub fn set_attributes(&self, attributes: impl IntoIterator<Item = Attribute>) {
        self.with_recording(|recording| {
            for attribute in attributes {
                if attribute.key.is_empty() {
                    continue;
                }
                if let Some(existing) = recording
                    .attributes
                    .iter_mut()
                    .find(|existing| existing.key == attribute.key)
                {
                    *existing = attribute;
                } else if recording.attributes.len() >= MAX_SPAN_ATTRIBUTES {
                    recording.dropped_attributes += 1;
                } else {
                    recording.attributes.push(attribute);
                }
            }
        });
    }

    /// Sets the status; `Ok` is final, as in OpenTelemetry.
    pub fn set_status(&self, status: Status, description: &str) {
        if status == Status::Unset {
            return;
        }
        self.with_recording(|recording| {
            if recording.status == Status::Ok {
                return;
            }
            recording.status = status;
            recording.status_description = if status == Status::Error {
                truncate(description.to_owned(), MAX_ATTRIBUTE_VALUE_LENGTH)
            } else {
                String::new()
            };
        });
    }

    /// Records a failure as `error.type` and an error status. `error_type`
    /// is a low-cardinality code (`timeout`, `500`), never an error message.
    pub fn record_error_type(&self, error_type: &str) {
        self.set_attributes([Attribute::string("error.type", error_type)]);
        self.set_status(Status::Error, error_type);
    }

    /// Ends the span now. Later calls (and the drop) do nothing.
    pub fn end(mut self) {
        self.finish();
    }

    fn finish(&mut self) {
        if std::mem::replace(&mut self.ended, true) {
            return;
        }
        let Some(recording) = self.recording.take() else {
            return;
        };
        let recording = recording
            .into_inner()
            .unwrap_or_else(PoisonError::into_inner);
        self.tracer.telemetry.export_span(SpanData {
            scope: Arc::clone(&self.tracer.scope),
            context: self.context.clone(),
            parent_span_id: self.parent_span_id,
            parent_remote: self.parent_remote,
            name: recording.name,
            kind: recording.kind,
            start: recording.start,
            end: SystemTime::now(),
            attributes: recording.attributes,
            dropped_attributes: recording.dropped_attributes,
            status: recording.status,
            status_description: recording.status_description,
        });
    }
}

impl Drop for Span {
    fn drop(&mut self) {
        self.finish();
    }
}

/// OpenTelemetry's `TraceIdRatioBased` threshold: a root span is sampled when
/// the trace identifier's last 8 bytes, shifted right by one, are below
/// `ratio * 2^63`.
pub(crate) fn ratio_threshold(ratio: f64) -> u64 {
    if ratio >= 1.0 {
        return 1 << 63;
    }
    // In range: 0 <= ratio < 1.
    (ratio * (1_u64 << 63) as f64) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_respects_character_boundaries() {
        assert_eq!(truncate("héllo".to_owned(), 2), "h");
        assert_eq!(truncate("hello".to_owned(), 10), "hello");
    }

    #[test]
    fn thresholds_follow_the_ratio() {
        assert_eq!(ratio_threshold(1.0), 1 << 63);
        assert_eq!(ratio_threshold(0.0), 0);
        assert_eq!(ratio_threshold(0.5), 1 << 62);
    }
}
