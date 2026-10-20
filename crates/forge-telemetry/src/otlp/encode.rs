//! The OTLP JSON encoding (the protobuf JSON mapping): hex trace and span
//! identifiers, 64-bit integers as strings, enums as numbers, camelCase
//! field names. Mirrors Forge-Go's `otlp.go` field for field.

use std::{
    collections::BTreeMap,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Serialize, Serializer};

use crate::{
    SCOPE_NAME,
    logs::LogRecord,
    metrics::{MetricData, MetricKind},
    trace::{Attribute, SpanData, Value},
};

/// `AGGREGATION_TEMPORALITY_CUMULATIVE`.
const AGGREGATION_CUMULATIVE: u8 = 2;
/// Span flags: the low byte is the W3C flags; bit 8 says whether bit 9
/// (parent is remote) is known.
const SPAN_FLAGS_HAS_IS_REMOTE: u32 = 0x100;
const SPAN_FLAGS_IS_REMOTE: u32 = 0x200;

#[derive(Serialize)]
pub(crate) struct KeyValue {
    key: String,
    value: AnyValue,
}

#[derive(Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AnyValue {
    #[serde(skip_serializing_if = "Option::is_none")]
    string_value: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    int_value: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    double_value: Option<Double>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bool_value: Option<bool>,
}

/// A double that encodes non-finite values the way proto3 JSON does.
#[derive(Clone, Copy)]
pub(crate) struct Double(f64);

impl Serialize for Double {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let value = self.0;
        if value.is_nan() {
            serializer.serialize_str("NaN")
        } else if value == f64::INFINITY {
            serializer.serialize_str("Infinity")
        } else if value == f64::NEG_INFINITY {
            serializer.serialize_str("-Infinity")
        } else {
            serializer.serialize_f64(value)
        }
    }
}

#[derive(Serialize)]
struct Resource {
    attributes: Vec<KeyValue>,
}

#[derive(Serialize)]
struct Scope {
    name: String,
}

pub(crate) fn unix_nanos(time: SystemTime) -> String {
    time.duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos())
        .to_string()
}

pub(crate) fn attributes(attributes: &[Attribute]) -> Vec<KeyValue> {
    attributes
        .iter()
        .map(|attribute| KeyValue {
            key: attribute.key.clone(),
            value: any_value(&attribute.value),
        })
        .collect()
}

fn any_value(value: &Value) -> AnyValue {
    match value {
        Value::String(text) => AnyValue {
            string_value: Some(text.clone()),
            ..AnyValue::default()
        },
        Value::Int(number) => AnyValue {
            int_value: Some(number.to_string()),
            ..AnyValue::default()
        },
        Value::Double(number) => AnyValue {
            double_value: Some(Double(*number)),
            ..AnyValue::default()
        },
        Value::Bool(flag) => AnyValue {
            bool_value: Some(*flag),
            ..AnyValue::default()
        },
    }
}

fn resource(resource: &[Attribute]) -> Resource {
    Resource {
        attributes: attributes(resource),
    }
}

// Traces.

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Traces {
    resource_spans: Vec<ResourceSpans>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ResourceSpans {
    resource: Resource,
    scope_spans: Vec<ScopeSpans>,
}

#[derive(Serialize)]
struct ScopeSpans {
    scope: Scope,
    spans: Vec<Span>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Span {
    trace_id: String,
    span_id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    trace_state: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    parent_span_id: String,
    flags: u32,
    name: String,
    kind: u8,
    start_time_unix_nano: String,
    end_time_unix_nano: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    attributes: Vec<KeyValue>,
    #[serde(skip_serializing_if = "is_zero")]
    dropped_attributes_count: u32,
    status: Status,
}

#[derive(Serialize)]
struct Status {
    #[serde(skip_serializing_if = "String::is_empty")]
    message: String,
    #[serde(skip_serializing_if = "is_zero_u8")]
    code: u8,
}

fn is_zero(value: &u32) -> bool {
    *value == 0
}

fn is_zero_u8(value: &u8) -> bool {
    *value == 0
}

/// `ExportTraceServiceRequest` as JSON.
pub(crate) fn traces(resource_attributes: &[Attribute], spans: &[SpanData]) -> Vec<u8> {
    let mut by_scope: BTreeMap<&str, Vec<Span>> = BTreeMap::new();
    for span in spans {
        let mut flags = u32::from(span.context.flags) | SPAN_FLAGS_HAS_IS_REMOTE;
        let parent_span_id = match span.parent_span_id {
            Some(parent) => {
                if span.parent_remote {
                    flags |= SPAN_FLAGS_IS_REMOTE;
                }
                parent.to_string()
            }
            None => String::new(),
        };
        by_scope.entry(&span.scope).or_default().push(Span {
            trace_id: span.context.trace_id.to_string(),
            span_id: span.context.span_id.to_string(),
            trace_state: span.context.trace_state.clone(),
            parent_span_id,
            flags,
            name: span.name.clone(),
            kind: span.kind as u8,
            start_time_unix_nano: unix_nanos(span.start),
            end_time_unix_nano: unix_nanos(span.end),
            attributes: attributes(&span.attributes),
            dropped_attributes_count: span.dropped_attributes,
            status: Status {
                message: span.status_description.clone(),
                code: span.status as u8,
            },
        });
    }
    let document = Traces {
        resource_spans: vec![ResourceSpans {
            resource: resource(resource_attributes),
            scope_spans: by_scope
                .into_iter()
                .map(|(scope, spans)| ScopeSpans {
                    scope: Scope {
                        name: scope.to_owned(),
                    },
                    spans,
                })
                .collect(),
        }],
    };
    serde_json::to_vec(&document).unwrap_or_default()
}

// Metrics.

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Metrics {
    resource_metrics: Vec<ResourceMetrics>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ResourceMetrics {
    resource: Resource,
    scope_metrics: Vec<ScopeMetrics>,
}

#[derive(Serialize)]
struct ScopeMetrics {
    scope: Scope,
    metrics: Vec<Metric>,
}

#[derive(Serialize)]
struct Metric {
    name: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    description: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    unit: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    sum: Option<Sum>,
    #[serde(skip_serializing_if = "Option::is_none")]
    gauge: Option<Gauge>,
    #[serde(skip_serializing_if = "Option::is_none")]
    histogram: Option<Histogram>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Sum {
    data_points: Vec<NumberPoint>,
    aggregation_temporality: u8,
    is_monotonic: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Gauge {
    data_points: Vec<NumberPoint>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Histogram {
    data_points: Vec<HistogramPoint>,
    aggregation_temporality: u8,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct NumberPoint {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    attributes: Vec<KeyValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    start_time_unix_nano: Option<String>,
    time_unix_nano: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    as_int: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    as_double: Option<Double>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct HistogramPoint {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    attributes: Vec<KeyValue>,
    start_time_unix_nano: String,
    time_unix_nano: String,
    count: String,
    sum: Double,
    bucket_counts: Vec<String>,
    explicit_bounds: Vec<f64>,
}

fn label_attributes(labels: &[(String, String)]) -> Vec<KeyValue> {
    labels
        .iter()
        .map(|(key, value)| KeyValue {
            key: key.clone(),
            value: AnyValue {
                string_value: Some(value.clone()),
                ..AnyValue::default()
            },
        })
        .collect()
}

/// `ExportMetricsServiceRequest` as JSON; temporality is cumulative.
pub(crate) fn metrics(resource_attributes: &[Attribute], metrics: &[MetricData]) -> Vec<u8> {
    let encoded = metrics
        .iter()
        .map(|metric| {
            let mut encoded = Metric {
                name: metric.name.clone(),
                description: metric.description.clone(),
                unit: metric.unit.clone(),
                sum: None,
                gauge: None,
                histogram: None,
            };
            match metric.kind {
                MetricKind::Counter => {
                    encoded.sum = Some(Sum {
                        data_points: metric
                            .points
                            .iter()
                            .map(|point| NumberPoint {
                                attributes: label_attributes(&point.labels),
                                start_time_unix_nano: Some(unix_nanos(point.start)),
                                time_unix_nano: unix_nanos(point.time),
                                as_int: (!metric.fractional)
                                    .then(|| (point.value as u64).to_string()),
                                as_double: metric.fractional.then_some(Double(point.value)),
                            })
                            .collect(),
                        aggregation_temporality: AGGREGATION_CUMULATIVE,
                        is_monotonic: true,
                    });
                }
                MetricKind::Gauge => {
                    encoded.gauge = Some(Gauge {
                        data_points: metric
                            .points
                            .iter()
                            .map(|point| NumberPoint {
                                attributes: label_attributes(&point.labels),
                                start_time_unix_nano: None,
                                time_unix_nano: unix_nanos(point.time),
                                as_int: None,
                                as_double: Some(Double(point.value)),
                            })
                            .collect(),
                    });
                }
                MetricKind::Histogram => {
                    encoded.histogram = Some(Histogram {
                        data_points: metric
                            .points
                            .iter()
                            .map(|point| HistogramPoint {
                                attributes: label_attributes(&point.labels),
                                start_time_unix_nano: unix_nanos(point.start),
                                time_unix_nano: unix_nanos(point.time),
                                count: point.count.to_string(),
                                sum: Double(point.sum),
                                bucket_counts: point
                                    .bucket_counts
                                    .iter()
                                    .map(ToString::to_string)
                                    .collect(),
                                explicit_bounds: metric.bounds.clone(),
                            })
                            .collect(),
                        aggregation_temporality: AGGREGATION_CUMULATIVE,
                    });
                }
            }
            encoded
        })
        .collect();
    let document = Metrics {
        resource_metrics: vec![ResourceMetrics {
            resource: resource(resource_attributes),
            scope_metrics: vec![ScopeMetrics {
                scope: Scope {
                    name: SCOPE_NAME.to_owned(),
                },
                metrics: encoded,
            }],
        }],
    };
    serde_json::to_vec(&document).unwrap_or_default()
}

// Logs.

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Logs {
    resource_logs: Vec<ResourceLogs>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ResourceLogs {
    resource: Resource,
    scope_logs: Vec<ScopeLogs>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ScopeLogs {
    scope: Scope,
    log_records: Vec<Log>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Log {
    time_unix_nano: String,
    observed_time_unix_nano: String,
    severity_number: u8,
    #[serde(skip_serializing_if = "str::is_empty")]
    severity_text: &'static str,
    body: AnyValue,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    attributes: Vec<KeyValue>,
    #[serde(skip_serializing_if = "is_zero")]
    dropped_attributes_count: u32,
    #[serde(skip_serializing_if = "is_zero")]
    flags: u32,
    #[serde(skip_serializing_if = "String::is_empty")]
    trace_id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    span_id: String,
}

/// `ExportLogsServiceRequest` as JSON.
pub(crate) fn logs(resource_attributes: &[Attribute], records: &[LogRecord]) -> Vec<u8> {
    let document = Logs {
        resource_logs: vec![ResourceLogs {
            resource: resource(resource_attributes),
            scope_logs: vec![ScopeLogs {
                scope: Scope {
                    name: SCOPE_NAME.to_owned(),
                },
                log_records: records
                    .iter()
                    .map(|record| Log {
                        time_unix_nano: unix_nanos(record.time),
                        observed_time_unix_nano: unix_nanos(record.observed_time),
                        severity_number: record.severity_number,
                        severity_text: record.severity_text,
                        body: AnyValue {
                            string_value: Some(record.body.clone()),
                            ..AnyValue::default()
                        },
                        attributes: attributes(&record.attributes),
                        dropped_attributes_count: record.dropped_attributes,
                        flags: u32::from(record.flags),
                        trace_id: record.trace_id.map(|id| id.to_string()).unwrap_or_default(),
                        span_id: record.span_id.map(|id| id.to_string()).unwrap_or_default(),
                    })
                    .collect(),
            }],
        }],
    };
    serde_json::to_vec(&document).unwrap_or_default()
}
