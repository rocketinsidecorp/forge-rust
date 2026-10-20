//! Statistics and the output formats shared with Forge-Go's load generator
//! (docs/benchmarks.md, "Output format"): a markdown table and a JSON
//! document with the same keys.

use std::fmt::Write as _;
use std::time::Duration;

/// The outcome of one measured window of one scenario.
#[derive(Debug, Default)]
pub struct Sample {
    pub ok: usize,
    pub errors: usize,
    pub elapsed: Duration,
    /// Latencies of the successful requests, sorted.
    pub latencies: Vec<Duration>,
}

/// The figures of one run, or the median of each figure over runs.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Stats {
    pub requests_per_second: f64,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
}

/// One scenario over all its runs.
#[derive(Debug)]
pub struct Outcome {
    pub scenario: String,
    pub runs: Vec<Stats>,
    pub median: Stats,
    pub errors: usize,
}

/// What was configured, for the header line and the JSON document.
#[derive(Clone, Copy, Debug)]
pub struct Settings {
    pub concurrency: usize,
    pub duration: Duration,
    pub warmup: Duration,
    pub runs: usize,
    pub rows: usize,
}

/// The nearest-rank percentile `p` (0 to 100) of `sorted`.
#[must_use]
pub fn percentile(sorted: &[Duration], p: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let rank = (p / 100.0 * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

/// The figures of a sample.
#[must_use]
pub fn stats_of(sample: &Sample) -> Stats {
    Stats {
        requests_per_second: sample.ok as f64 / sample.elapsed.as_secs_f64(),
        p50_ms: millis(percentile(&sample.latencies, 50.0)),
        p95_ms: millis(percentile(&sample.latencies, 95.0)),
        p99_ms: millis(percentile(&sample.latencies, 99.0)),
    }
}

/// The median of `values` (the mean of the middle two when there is an even
/// number of them).
#[must_use]
pub fn median(values: &[f64]) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let n = sorted.len();
    match n {
        0 => 0.0,
        _ if n % 2 == 1 => sorted[n / 2],
        _ => f64::midpoint(sorted[n / 2 - 1], sorted[n / 2]),
    }
}

/// The median of each figure over `runs`.
#[must_use]
pub fn median_of(runs: &[Stats]) -> Stats {
    let column = |pick: fn(&Stats) -> f64| median(&runs.iter().map(pick).collect::<Vec<_>>());
    Stats {
        requests_per_second: column(|s| s.requests_per_second),
        p50_ms: column(|s| s.p50_ms),
        p95_ms: column(|s| s.p95_ms),
        p99_ms: column(|s| s.p99_ms),
    }
}

/// A duration the way Go prints it (`10s`, `500ms`, `1m30s`), so the header
/// line reads the same in both frameworks.
#[must_use]
pub fn go_duration(duration: Duration) -> String {
    let nanos = duration.as_nanos();
    if nanos == 0 {
        return "0s".to_owned();
    }
    let trim = |value: f64| {
        let text = format!("{value:.9}");
        text.trim_end_matches('0').trim_end_matches('.').to_owned()
    };
    if nanos < 1_000 {
        return format!("{nanos}ns");
    }
    if nanos < 1_000_000 {
        return format!("{}µs", trim(nanos as f64 / 1e3));
    }
    if nanos < 1_000_000_000 {
        return format!("{}ms", trim(nanos as f64 / 1e6));
    }
    let seconds = duration.as_secs_f64();
    let hours = (seconds / 3600.0).floor();
    let minutes = ((seconds - hours * 3600.0) / 60.0).floor();
    let rest = seconds - hours * 3600.0 - minutes * 60.0;
    let mut out = String::new();
    if hours > 0.0 {
        let _ = write!(out, "{hours}h");
    }
    if hours > 0.0 || minutes > 0.0 {
        let _ = write!(out, "{minutes}m");
    }
    let _ = write!(out, "{}s", trim(rest));
    out
}

/// Two decimals under 10 ms, one above.
fn ms(value: f64) -> String {
    if value < 10.0 {
        format!("{value:.2}")
    } else {
        format!("{value:.1}")
    }
}

/// The markdown report.
#[must_use]
pub fn markdown(settings: &Settings, outcomes: &[Outcome]) -> String {
    let mut out = format!(
        "concurrency {}, {} per run, {} warm-up, {} runs, index of {} rows; figures are medians over the runs\n\n",
        settings.concurrency,
        go_duration(settings.duration),
        go_duration(settings.warmup),
        settings.runs,
        settings.rows
    );
    out.push_str(
        "| Scenario | Requests/s | Requests/s (min-max) | p50 ms | p95 ms | p99 ms | Errors |\n",
    );
    out.push_str("| --- | ---: | ---: | ---: | ---: | ---: | ---: |\n");
    for outcome in outcomes {
        let rates = outcome.runs.iter().map(|run| run.requests_per_second);
        let low = rates.clone().fold(f64::INFINITY, f64::min);
        let high = rates.fold(f64::NEG_INFINITY, f64::max);
        let _ = writeln!(
            out,
            "| `{}` | {:.0} | {low:.0}-{high:.0} | {} | {} | {} | {} |",
            outcome.scenario,
            outcome.median.requests_per_second,
            ms(outcome.median.p50_ms),
            ms(outcome.median.p95_ms),
            ms(outcome.median.p99_ms),
            outcome.errors,
        );
    }
    out
}

/// A float the way Go's JSON encoder writes it for ordinary magnitudes: the
/// shortest text that reads back as the same number, and `0` for zero.
fn number(value: f64) -> String {
    if value.is_finite() {
        format!("{value}")
    } else {
        "0".to_owned()
    }
}

fn stats_json(out: &mut String, stats: &Stats, indent: &str) {
    let _ = write!(
        out,
        "{{\n{indent}  \"requests_per_second\": {},\n{indent}  \"p50_ms\": {},\n{indent}  \"p95_ms\": {},\n{indent}  \"p99_ms\": {}\n{indent}}}",
        number(stats.requests_per_second),
        number(stats.p50_ms),
        number(stats.p95_ms),
        number(stats.p99_ms),
    );
}

/// The JSON report: Forge-Go's keys in its order (the top-level keys sorted,
/// as Go writes a map) and indentation of two spaces.
#[must_use]
pub fn json(settings: &Settings, outcomes: &[Outcome]) -> String {
    let mut out = String::from("{\n");
    let _ = writeln!(out, "  \"concurrency\": {},", settings.concurrency);
    let _ = writeln!(
        out,
        "  \"duration_seconds\": {},",
        number(settings.duration.as_secs_f64())
    );
    let _ = writeln!(out, "  \"index_rows\": {},", settings.rows);
    out.push_str("  \"results\": [\n");
    for (position, outcome) in outcomes.iter().enumerate() {
        out.push_str("    {\n");
        let _ = writeln!(out, "      \"scenario\": \"{}\",", outcome.scenario);
        out.push_str("      \"runs\": [\n");
        for (index, run) in outcome.runs.iter().enumerate() {
            out.push_str("        ");
            stats_json(&mut out, run, "        ");
            out.push_str(if index + 1 < outcome.runs.len() {
                ",\n"
            } else {
                "\n"
            });
        }
        out.push_str("      ],\n      \"median\": ");
        stats_json(&mut out, &outcome.median, "      ");
        let _ = write!(out, ",\n      \"errors\": {}\n    }}", outcome.errors);
        out.push_str(if position + 1 < outcomes.len() {
            ",\n"
        } else {
            "\n"
        });
    }
    out.push_str("  ],\n");
    let _ = writeln!(out, "  \"runs\": {},", settings.runs);
    let _ = writeln!(
        out,
        "  \"warmup_seconds\": {}",
        number(settings.warmup.as_secs_f64())
    );
    out.push_str("}\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms_list(values: &[u64]) -> Vec<Duration> {
        values.iter().map(|v| Duration::from_millis(*v)).collect()
    }

    #[test]
    fn percentiles_are_nearest_rank() {
        let sorted = ms_list(&(1..=100).collect::<Vec<_>>());
        assert_eq!(percentile(&sorted, 50.0), Duration::from_millis(50));
        assert_eq!(percentile(&sorted, 95.0), Duration::from_millis(95));
        assert_eq!(percentile(&sorted, 99.0), Duration::from_millis(99));
        assert_eq!(percentile(&sorted, 100.0), Duration::from_millis(100));
        assert_eq!(percentile(&ms_list(&[7]), 99.0), Duration::from_millis(7));
        assert_eq!(percentile(&[], 50.0), Duration::ZERO);
    }

    #[test]
    fn medians_of_odd_and_even_counts() {
        assert!((median(&[3.0, 1.0, 2.0]) - 2.0).abs() < f64::EPSILON);
        assert!((median(&[4.0, 1.0, 2.0, 3.0]) - 2.5).abs() < f64::EPSILON);
        assert!(median(&[]).abs() < f64::EPSILON);
    }

    #[test]
    fn durations_print_like_go() {
        assert_eq!(go_duration(Duration::from_secs(10)), "10s");
        assert_eq!(go_duration(Duration::from_secs(2)), "2s");
        assert_eq!(go_duration(Duration::from_millis(500)), "500ms");
        assert_eq!(go_duration(Duration::from_millis(1500)), "1.5s");
        assert_eq!(go_duration(Duration::from_secs(90)), "1m30s");
        assert_eq!(go_duration(Duration::from_secs(60)), "1m0s");
        assert_eq!(go_duration(Duration::from_secs(3600)), "1h0m0s");
        assert_eq!(go_duration(Duration::ZERO), "0s");
    }

    fn outcomes() -> Vec<Outcome> {
        let run = |rate| Stats {
            requests_per_second: rate,
            p50_ms: 0.671,
            p95_ms: 12.34,
            p99_ms: 5.0,
        };
        let runs = vec![run(17663.0), run(17869.4), run(22101.0)];
        vec![Outcome {
            scenario: "health".to_owned(),
            median: median_of(&runs),
            runs,
            errors: 0,
        }]
    }

    const SETTINGS: Settings = Settings {
        concurrency: 16,
        duration: Duration::from_secs(10),
        warmup: Duration::from_secs(2),
        runs: 3,
        rows: 50,
    };

    #[test]
    fn markdown_has_the_shared_columns_and_header() {
        assert_eq!(
            markdown(&SETTINGS, &outcomes()),
            "concurrency 16, 10s per run, 2s warm-up, 3 runs, index of 50 rows; figures are medians over the runs\n\n\
             | Scenario | Requests/s | Requests/s (min-max) | p50 ms | p95 ms | p99 ms | Errors |\n\
             | --- | ---: | ---: | ---: | ---: | ---: | ---: |\n\
             | `health` | 17869 | 17663-22101 | 0.67 | 12.3 | 5.00 | 0 |\n"
        );
    }

    #[test]
    fn json_has_the_shared_keys_in_order() {
        let text = json(&SETTINGS, &outcomes());
        let keys: Vec<&str> = text
            .lines()
            .filter_map(|line| line.trim().strip_prefix('"')?.split('"').next())
            .collect();
        assert_eq!(
            keys,
            [
                "concurrency",
                "duration_seconds",
                "index_rows",
                "results",
                "scenario",
                "runs",
                "requests_per_second",
                "p50_ms",
                "p95_ms",
                "p99_ms",
                "requests_per_second",
                "p50_ms",
                "p95_ms",
                "p99_ms",
                "requests_per_second",
                "p50_ms",
                "p95_ms",
                "p99_ms",
                "median",
                "requests_per_second",
                "p50_ms",
                "p95_ms",
                "p99_ms",
                "errors",
                "runs",
                "warmup_seconds"
            ]
        );
        assert!(text.contains("  \"duration_seconds\": 10,\n"), "{text}");
        assert!(text.contains("\"requests_per_second\": 17869.4"), "{text}");
        assert!(text.ends_with("  \"warmup_seconds\": 2\n}\n"), "{text}");
    }
}
