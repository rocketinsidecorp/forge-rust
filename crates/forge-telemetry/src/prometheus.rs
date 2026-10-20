//! The Prometheus text exposition format 0.0.4, with Forge-Go's name
//! conversion: dots become underscores, the unit becomes a suffix (`s` adds
//! `_seconds`, `By` `_bytes`) and counters end in `_total`, so
//! `http.server.request.duration` is `http_server_request_duration_seconds`.

use std::fmt::Write as _;

use crate::metrics::{MetricData, MetricKind, Registry};

/// Media type of the text format.
pub const PROMETHEUS_CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// The Prometheus name of an OpenTelemetry metric name and unit.
#[must_use]
pub fn prometheus_name(name: &str, unit: &str, kind: MetricKind) -> String {
    let mut result = sanitize(name);
    let suffix = match unit {
        "s" => "seconds",
        "ms" => "milliseconds",
        "By" => "bytes",
        _ => "",
    };
    if !suffix.is_empty() && !result.ends_with(&format!("_{suffix}")) {
        result.push('_');
        result.push_str(suffix);
    }
    if kind == MetricKind::Counter && !result.ends_with("_total") {
        result.push_str("_total");
    }
    result
}

fn sanitize(name: &str) -> String {
    name.bytes()
        .enumerate()
        .map(|(index, byte)| {
            if byte.is_ascii_alphabetic()
                || (byte.is_ascii_digit() && index > 0)
                || byte == b'_'
                || byte == b':'
            {
                char::from(byte)
            } else {
                '_'
            }
        })
        .collect()
}

/// Renders every instrument of `registry` (observers run first).
#[must_use]
pub fn render(registry: &Registry) -> String {
    encode(&registry.collect())
}

/// Renders a metric snapshot.
#[must_use]
pub fn encode(metrics: &[MetricData]) -> String {
    let mut out = String::new();
    for metric in metrics {
        let name = prometheus_name(&metric.name, &metric.unit, metric.kind);
        let kind = match metric.kind {
            MetricKind::Counter => "counter",
            MetricKind::Gauge => "gauge",
            MetricKind::Histogram => "histogram",
        };
        if !metric.description.is_empty() {
            let _ = writeln!(out, "# HELP {name} {}", escape_help(&metric.description));
        }
        let _ = writeln!(out, "# TYPE {name} {kind}");
        for point in &metric.points {
            if metric.kind != MetricKind::Histogram {
                sample(&mut out, &name, &point.labels, None, point.value);
                continue;
            }
            let mut cumulative = 0_u64;
            for (index, count) in point.bucket_counts.iter().enumerate() {
                cumulative += count;
                let le = metric
                    .bounds
                    .get(index)
                    .map_or_else(|| "+Inf".to_owned(), |bound| format_float(*bound));
                sample(
                    &mut out,
                    &format!("{name}_bucket"),
                    &point.labels,
                    Some(&le),
                    cumulative as f64,
                );
            }
            sample(
                &mut out,
                &format!("{name}_sum"),
                &point.labels,
                None,
                point.sum,
            );
            sample(
                &mut out,
                &format!("{name}_count"),
                &point.labels,
                None,
                point.count as f64,
            );
        }
    }
    out
}

fn sample(out: &mut String, name: &str, labels: &[(String, String)], le: Option<&str>, value: f64) {
    out.push_str(name);
    if !labels.is_empty() || le.is_some() {
        out.push('{');
        for (index, (label, value)) in labels.iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            out.push_str(&sanitize(label));
            out.push_str("=\"");
            out.push_str(&escape_label(value));
            out.push('"');
        }
        if let Some(le) = le {
            if !labels.is_empty() {
                out.push(',');
            }
            out.push_str("le=\"");
            out.push_str(le);
            out.push('"');
        }
        out.push('}');
    }
    out.push(' ');
    out.push_str(&format_float(value));
    out.push('\n');
}

fn format_float(value: f64) -> String {
    if value.is_nan() {
        "NaN".to_owned()
    } else if value == f64::INFINITY {
        "+Inf".to_owned()
    } else if value == f64::NEG_INFINITY {
        "-Inf".to_owned()
    } else {
        value.to_string()
    }
}

fn escape_label(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

fn escape_help(value: &str) -> String {
    value.replace('\\', "\\\\").replace('\n', "\\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::DURATION_BUCKETS;

    #[test]
    fn names_follow_forge_go() {
        assert_eq!(
            prometheus_name("http.server.request.duration", "s", MetricKind::Histogram),
            "http_server_request_duration_seconds"
        );
        assert_eq!(
            prometheus_name("forge.db.pool.wait.duration", "s", MetricKind::Counter),
            "forge_db_pool_wait_duration_seconds_total"
        );
        assert_eq!(
            prometheus_name("process.memory.usage", "By", MetricKind::Gauge),
            "process_memory_usage_bytes"
        );
        assert_eq!(
            prometheus_name(
                "forge.telemetry.spans.dropped",
                "{span}",
                MetricKind::Counter
            ),
            "forge_telemetry_spans_dropped_total"
        );
        assert_eq!(prometheus_name("9lives", "", MetricKind::Gauge), "_lives");
    }

    #[test]
    fn text_format_has_types_buckets_and_escaped_labels() {
        let registry = Registry::default();
        let histogram = registry
            .histogram(
                "http.server.request.duration",
                "Duration of HTTP server requests.",
                "s",
                DURATION_BUCKETS,
                &["http.route"],
            )
            .expect("valid");
        histogram.record(0.2, &["/a\"b\\c\nd"]);
        let text = render(&registry);
        assert!(text.contains(
            "# HELP http_server_request_duration_seconds Duration of HTTP server requests.\n"
        ));
        assert!(text.contains("# TYPE http_server_request_duration_seconds histogram\n"));
        assert!(text.contains(
            "http_server_request_duration_seconds_bucket{http_route=\"/a\\\"b\\\\c\\nd\",le=\"0.25\"} 1\n"
        ));
        assert!(text.contains(
            "http_server_request_duration_seconds_bucket{http_route=\"/a\\\"b\\\\c\\nd\",le=\"0.1\"} 0\n"
        ));
        assert!(text.contains("le=\"+Inf\"} 1\n"));
        assert!(text.contains(
            "http_server_request_duration_seconds_count{http_route=\"/a\\\"b\\\\c\\nd\"} 1\n"
        ));
        assert!(
            text.contains("# TYPE forge_telemetry_metric_overflows_total counter")
                || !text.contains("overflows")
        );
    }
}
