//! Metric instruments with label names fixed at registration and a
//! per-instrument series cap (the port of Forge-Go's `metrics.go`).
//!
//! Each distinct combination of label values is one series. Once an
//! instrument holds [`DEFAULT_MAX_SERIES`] series, new combinations are folded
//! into one series whose labels are all [`OVERFLOW_LABEL_VALUE`] and
//! `forge.telemetry.metric.overflows` counts them, so a label fed by untrusted
//! input cannot exhaust memory or the metrics backend.

use std::{
    collections::{BTreeMap, HashMap, hash_map::RandomState},
    fmt,
    hash::{BuildHasher, Hasher},
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, Mutex, OnceLock, PoisonError, RwLock,
        atomic::{AtomicU64, Ordering},
    },
    time::SystemTime,
};

use thiserror::Error;

/// Most labels an instrument may declare.
pub const MAX_METRIC_LABELS: usize = 6;
/// Default series limit of each instrument.
pub const DEFAULT_MAX_SERIES: usize = 1000;
/// Label values are cut to this many bytes.
pub const MAX_LABEL_VALUE_LENGTH: usize = 128;
/// Value of every label of the overflow series.
pub const OVERFLOW_LABEL_VALUE: &str = "_overflow";
/// Counter of measurements folded into overflow series.
pub const METRIC_OVERFLOWS: &str = "forge.telemetry.metric.overflows";

/// Histogram bounds, in seconds, that OpenTelemetry semantic conventions
/// advise for request and operation durations.
pub const DURATION_BUCKETS: &[f64] = &[
    0.005, 0.01, 0.025, 0.05, 0.075, 0.1, 0.25, 0.5, 0.75, 1.0, 2.5, 5.0, 7.5, 10.0,
];

/// The type of an instrument.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MetricKind {
    /// A monotonic sum.
    Counter,
    /// A value that goes up and down.
    Gauge,
    /// A distribution in explicit buckets.
    Histogram,
}

/// An instrument could not be registered.
#[derive(Debug, Error, Eq, PartialEq)]
pub enum MetricError {
    /// The name does not match `[a-zA-Z][a-zA-Z0-9_./-]{0,254}`.
    #[error("invalid metric name {0:?}")]
    InvalidName(String),
    /// A label is invalid, repeated, `le`, or there are too many.
    #[error("metric {0} has an invalid, duplicate or excess label")]
    InvalidLabels(String),
    /// Histogram bounds must be finite and strictly ascending.
    #[error("histogram {0} bounds must be finite and strictly ascending")]
    InvalidBounds(String),
    /// The name is registered with another kind, unit, labels or bounds.
    #[error("metric {0} is already registered with a different definition")]
    Conflict(String),
}

type Observer = Arc<dyn Fn() + Send + Sync>;

/// Holds instruments; cheap to clone and safe to share.
#[derive(Clone)]
pub struct Registry {
    inner: Arc<RegistryInner>,
}

struct RegistryInner {
    max_series: usize,
    instruments: Mutex<BTreeMap<String, Arc<Instrument>>>,
    overflows: OnceLock<Counter>,
    /// Serializes observers, so a callback turning running totals into
    /// counter values never runs twice at once.
    collecting: Mutex<()>,
    observers: Mutex<BTreeMap<String, Observer>>,
}

impl fmt::Debug for Registry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let instruments = self
            .inner
            .instruments
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        formatter
            .debug_struct("Registry")
            .field("instruments", &instruments.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_SERIES)
    }
}

impl Registry {
    /// An empty registry; `max_series` bounds every instrument (0 means
    /// [`DEFAULT_MAX_SERIES`]).
    #[must_use]
    pub fn new(max_series: usize) -> Self {
        let registry = Self {
            inner: Arc::new(RegistryInner {
                max_series: if max_series == 0 {
                    DEFAULT_MAX_SERIES
                } else {
                    max_series
                },
                instruments: Mutex::new(BTreeMap::new()),
                overflows: OnceLock::new(),
                collecting: Mutex::new(()),
                observers: Mutex::new(BTreeMap::new()),
            }),
        };
        if let Ok(overflows) = registry.counter(
            METRIC_OVERFLOWS,
            "Measurements folded into an overflow series because an instrument reached its series limit.",
            "{measurement}",
            &[],
        ) {
            let _ = registry.inner.overflows.set(overflows);
        }
        registry
    }

    /// The counter named `name`, created on first use. Registering an
    /// identical definition again returns the same series; a different one
    /// is [`MetricError::Conflict`].
    pub fn counter(
        &self,
        name: &str,
        description: &str,
        unit: &str,
        labels: &[&str],
    ) -> Result<Counter, MetricError> {
        self.register(
            name,
            description,
            unit,
            MetricKind::Counter,
            &[],
            labels,
            false,
        )
        .map(|instrument| Counter { instrument })
    }

    /// A counter of fractional values (seconds waited, CPU time); see
    /// [`Registry::counter`].
    pub fn float_counter(
        &self,
        name: &str,
        description: &str,
        unit: &str,
        labels: &[&str],
    ) -> Result<FloatCounter, MetricError> {
        self.register(
            name,
            description,
            unit,
            MetricKind::Counter,
            &[],
            labels,
            true,
        )
        .map(|instrument| FloatCounter { instrument })
    }

    /// A gauge; see [`Registry::counter`].
    pub fn gauge(
        &self,
        name: &str,
        description: &str,
        unit: &str,
        labels: &[&str],
    ) -> Result<Gauge, MetricError> {
        self.register(
            name,
            description,
            unit,
            MetricKind::Gauge,
            &[],
            labels,
            false,
        )
        .map(|instrument| Gauge { instrument })
    }

    /// A histogram with ascending bucket `bounds` ([`DURATION_BUCKETS`] for
    /// durations in seconds); see [`Registry::counter`].
    pub fn histogram(
        &self,
        name: &str,
        description: &str,
        unit: &str,
        bounds: &[f64],
        labels: &[&str],
    ) -> Result<Histogram, MetricError> {
        self.register(
            name,
            description,
            unit,
            MetricKind::Histogram,
            bounds,
            labels,
            false,
        )
        .map(|instrument| Histogram { instrument })
    }

    #[allow(clippy::too_many_arguments)]
    fn register(
        &self,
        name: &str,
        description: &str,
        unit: &str,
        kind: MetricKind,
        bounds: &[f64],
        labels: &[&str],
        fractional: bool,
    ) -> Result<Arc<Instrument>, MetricError> {
        if !valid_metric_name(name) {
            return Err(MetricError::InvalidName(name.to_owned()));
        }
        let labels_valid = labels.len() <= MAX_METRIC_LABELS
            && labels.iter().enumerate().all(|(index, label)| {
                valid_label_name(label) && *label != "le" && !labels[..index].contains(label)
            });
        if !labels_valid {
            return Err(MetricError::InvalidLabels(name.to_owned()));
        }
        if bounds.iter().any(|bound| !bound.is_finite())
            || bounds.windows(2).any(|pair| pair[1] <= pair[0])
        {
            return Err(MetricError::InvalidBounds(name.to_owned()));
        }
        let mut instruments = self
            .inner
            .instruments
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(existing) = instruments.get(name) {
            let same = existing.kind == kind
                && existing.fractional == fractional
                && existing.unit == unit
                && existing
                    .labels
                    .iter()
                    .map(String::as_str)
                    .eq(labels.iter().copied())
                && existing.bounds == bounds;
            return if same {
                Ok(Arc::clone(existing))
            } else {
                Err(MetricError::Conflict(name.to_owned()))
            };
        }
        let instrument = Arc::new(Instrument {
            registry: Arc::downgrade(&self.inner),
            name: name.to_owned(),
            description: description.to_owned(),
            unit: unit.to_owned(),
            kind,
            fractional,
            labels: labels.iter().map(|label| (*label).to_owned()).collect(),
            bounds: bounds.to_vec(),
            max_series: self.inner.max_series,
            hasher: RandomState::new(),
            series: RwLock::new(Series::default()),
            overflow: OnceLock::new(),
        });
        instruments.insert(name.to_owned(), Arc::clone(&instrument));
        Ok(instrument)
    }

    /// Runs `observer` before every [`Registry::collect`] (each Prometheus
    /// scrape and OTLP push) to set gauges and counters from state that is
    /// cheaper to read on demand: connection pools, the process. Callbacks
    /// run one at a time in key order and must not call `observe` or
    /// `collect`. A key registered again replaces its callback; a panicking
    /// callback is skipped for that collection.
    pub fn observe(&self, key: &str, observer: impl Fn() + Send + Sync + 'static) {
        self.inner
            .observers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key.to_owned(), Arc::new(observer));
    }

    /// Removes the callback registered under `key`.
    pub fn unobserve(&self, key: &str) {
        self.inner
            .observers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(key);
    }

    /// A snapshot of every instrument with at least one series, sorted by
    /// name, series sorted by label values. Counters and histograms are
    /// cumulative since their series was created. Observers run first.
    #[must_use]
    pub fn collect(&self) -> Vec<MetricData> {
        {
            let _serialized = self
                .inner
                .collecting
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let observers: Vec<Observer> = self
                .inner
                .observers
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .values()
                .cloned()
                .collect();
            for observer in observers {
                let _ = catch_unwind(AssertUnwindSafe(|| observer()));
            }
        }
        let now = SystemTime::now();
        let instruments: Vec<Arc<Instrument>> = self
            .inner
            .instruments
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .cloned()
            .collect();
        instruments
            .iter()
            .filter_map(|instrument| instrument.snapshot(now))
            .collect()
    }

    /// The registered instrument names, sorted.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        self.inner
            .instruments
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .keys()
            .cloned()
            .collect()
    }

    /// The definition of a registered instrument: kind, unit and labels.
    #[must_use]
    pub fn definition(&self, name: &str) -> Option<(MetricKind, String, Vec<String>)> {
        self.inner
            .instruments
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(name)
            .map(|instrument| {
                (
                    instrument.kind,
                    instrument.unit.clone(),
                    instrument.labels.clone(),
                )
            })
    }
}

fn valid_metric_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 255
        && bytes[0].is_ascii_alphabetic()
        && bytes[1..]
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'/' | b'-'))
}

fn valid_label_name(label: &str) -> bool {
    let bytes = label.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 128
        && (bytes[0].is_ascii_alphabetic() || bytes[0] == b'_')
        && bytes[1..]
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.'))
}

struct Instrument {
    registry: std::sync::Weak<RegistryInner>,
    name: String,
    description: String,
    unit: String,
    kind: MetricKind,
    fractional: bool,
    labels: Vec<String>,
    bounds: Vec<f64>,
    max_series: usize,
    hasher: RandomState,
    series: RwLock<Series>,
    overflow: OnceLock<Arc<Point>>,
}

/// Series keyed by the hash of their (cut) label values, so a lookup does
/// not allocate; colliding series share a bucket.
#[derive(Default)]
struct Series {
    buckets: HashMap<u64, Vec<Arc<Point>>>,
    len: usize,
}

impl Series {
    fn find(&self, hash: u64, values: &[&str]) -> Option<Arc<Point>> {
        self.buckets.get(&hash)?.iter().find_map(|point| {
            point
                .labels
                .iter()
                .map(AsRef::as_ref)
                .eq(values.iter().map(|value| cut_label(value)))
                .then(|| Arc::clone(point))
        })
    }
}

struct Point {
    labels: Box<[Box<str>]>,
    start: SystemTime,
    /// A counter's count, or a gauge's or fractional counter's `f64` bits.
    value: AtomicU64,
    /// Histogram bucket counts, `bounds.len() + 1`.
    counts: Box<[AtomicU64]>,
    /// Histogram sum as `f64` bits.
    sum: AtomicU64,
}

fn cut_label(value: &str) -> &str {
    if value.len() <= MAX_LABEL_VALUE_LENGTH {
        return value;
    }
    let mut end = MAX_LABEL_VALUE_LENGTH;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

impl Instrument {
    fn new_point(&self, labels: Box<[Box<str>]>) -> Arc<Point> {
        let buckets = if self.kind == MetricKind::Histogram {
            self.bounds.len() + 1
        } else {
            0
        };
        Arc::new(Point {
            labels,
            start: SystemTime::now(),
            value: AtomicU64::new(0),
            counts: (0..buckets).map(|_| AtomicU64::new(0)).collect(),
            sum: AtomicU64::new(0),
        })
    }

    /// The series of `values`, created while below the series limit. Looking
    /// up an existing series does not allocate.
    fn point(&self, values: &[&str]) -> Arc<Point> {
        if values.len() != self.labels.len() {
            // A programming error; counted as overflow so it stays visible
            // without panicking in a request path.
            return self.overflow_point();
        }
        let mut hasher = self.hasher.build_hasher();
        for value in values {
            hasher.write(cut_label(value).as_bytes());
            hasher.write_u8(0xff);
        }
        let hash = hasher.finish();
        if let Some(point) = self
            .series
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .find(hash, values)
        {
            return point;
        }
        let mut series = self.series.write().unwrap_or_else(PoisonError::into_inner);
        if let Some(point) = series.find(hash, values) {
            return point;
        }
        if series.len >= self.max_series {
            drop(series);
            return self.overflow_point();
        }
        let point = self.new_point(
            values
                .iter()
                .map(|value| Box::from(cut_label(value)))
                .collect(),
        );
        series
            .buckets
            .entry(hash)
            .or_default()
            .push(Arc::clone(&point));
        series.len += 1;
        point
    }

    fn overflow_point(&self) -> Arc<Point> {
        if self.name != METRIC_OVERFLOWS
            && let Some(registry) = self.registry.upgrade()
            && let Some(overflows) = registry.overflows.get()
        {
            overflows.add(1, &[]);
        }
        Arc::clone(self.overflow.get_or_init(|| {
            self.new_point(
                self.labels
                    .iter()
                    .map(|_| Box::from(OVERFLOW_LABEL_VALUE))
                    .collect(),
            )
        }))
    }

    fn snapshot(&self, now: SystemTime) -> Option<MetricData> {
        let mut points: Vec<Arc<Point>> = self
            .series
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .buckets
            .values()
            .flatten()
            .cloned()
            .collect();
        if let Some(overflow) = self.overflow.get() {
            points.push(Arc::clone(overflow));
        }
        if points.is_empty() {
            return None;
        }
        points.sort_by(|a, b| a.labels.cmp(&b.labels));
        let points = points
            .iter()
            .map(|point| {
                let labels = self
                    .labels
                    .iter()
                    .zip(point.labels.iter())
                    .map(|(name, value)| (name.clone(), value.to_string()))
                    .collect();
                let mut data = PointData {
                    labels,
                    start: point.start,
                    time: now,
                    value: 0.0,
                    count: 0,
                    sum: 0.0,
                    bucket_counts: Vec::new(),
                };
                match self.kind {
                    MetricKind::Counter if !self.fractional => {
                        data.value = point.value.load(Ordering::Relaxed) as f64;
                    }
                    MetricKind::Counter | MetricKind::Gauge => {
                        data.value = f64::from_bits(point.value.load(Ordering::Relaxed));
                    }
                    MetricKind::Histogram => {
                        data.bucket_counts = point
                            .counts
                            .iter()
                            .map(|count| count.load(Ordering::Relaxed))
                            .collect();
                        data.count = data.bucket_counts.iter().sum();
                        data.sum = f64::from_bits(point.sum.load(Ordering::Relaxed));
                    }
                }
                data
            })
            .collect();
        Some(MetricData {
            name: self.name.clone(),
            description: self.description.clone(),
            unit: self.unit.clone(),
            kind: self.kind,
            fractional: self.fractional,
            bounds: self.bounds.clone(),
            points,
        })
    }
}

fn add_float(target: &AtomicU64, delta: f64) {
    let _ = target.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |bits| {
        Some((f64::from_bits(bits) + delta).to_bits())
    });
}

/// A monotonic count, such as requests served.
#[derive(Clone)]
pub struct Counter {
    instrument: Arc<Instrument>,
}

/// A monotonic sum of fractional values, such as seconds waited.
#[derive(Clone)]
pub struct FloatCounter {
    instrument: Arc<Instrument>,
}

/// A value that goes up and down, such as requests in flight.
#[derive(Clone)]
pub struct Gauge {
    instrument: Arc<Instrument>,
}

/// A distribution, such as request durations.
#[derive(Clone)]
pub struct Histogram {
    instrument: Arc<Instrument>,
}

macro_rules! debug_instrument {
    ($($type:ident),*) => {$(
        impl fmt::Debug for $type {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.debug_struct(stringify!($type)).field("name", &self.instrument.name).finish()
            }
        }
    )*};
}
debug_instrument!(Counter, FloatCounter, Gauge, Histogram);

impl Counter {
    /// Adds `n` to the series of `values` (the label values in declaration
    /// order).
    pub fn add(&self, n: u64, values: &[&str]) {
        if n == 0 {
            return;
        }
        self.instrument
            .point(values)
            .value
            .fetch_add(n, Ordering::Relaxed);
    }

    /// Records `total`, a running count kept elsewhere (a pool's waits). The
    /// counter never decreases: a lower total is ignored, and a first total
    /// of zero creates the series so it is exported from the start.
    pub fn observe(&self, total: u64, values: &[&str]) {
        self.instrument
            .point(values)
            .value
            .fetch_max(total, Ordering::Relaxed);
    }
}

impl FloatCounter {
    /// Adds `delta`; negative and non-finite values are ignored.
    pub fn add(&self, delta: f64, values: &[&str]) {
        if delta > 0.0 && delta.is_finite() {
            add_float(&self.instrument.point(values).value, delta);
        }
    }

    /// Records `total`, a running sum kept elsewhere, like
    /// [`Counter::observe`]. Negative and non-finite totals are ignored.
    pub fn observe(&self, total: f64, values: &[&str]) {
        if !(total >= 0.0 && total.is_finite()) {
            return;
        }
        let point = self.instrument.point(values);
        let _ = point
            .value
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |bits| {
                (total > f64::from_bits(bits)).then_some(total.to_bits())
            });
    }
}

impl Gauge {
    /// Sets the series of `values`; NaN is ignored.
    pub fn set(&self, value: f64, values: &[&str]) {
        if !value.is_nan() {
            self.instrument
                .point(values)
                .value
                .store(value.to_bits(), Ordering::Relaxed);
        }
    }

    /// Changes the series of `values` by `delta`; NaN is ignored.
    pub fn add(&self, delta: f64, values: &[&str]) {
        if !delta.is_nan() {
            add_float(&self.instrument.point(values).value, delta);
        }
    }
}

impl Histogram {
    /// Records `value` in the series of `values`; NaN is ignored. A value
    /// equal to a bound counts in that bound's bucket (`le`).
    pub fn record(&self, value: f64, values: &[&str]) {
        if value.is_nan() {
            return;
        }
        let point = self.instrument.point(values);
        let index = self
            .instrument
            .bounds
            .partition_point(|bound| *bound < value);
        point.counts[index].fetch_add(1, Ordering::Relaxed);
        add_float(&point.sum, value);
    }
}

/// A snapshot of one instrument.
#[derive(Clone, Debug)]
pub struct MetricData {
    /// OpenTelemetry name, such as `http.server.request.duration`.
    pub name: String,
    /// Help text.
    pub description: String,
    /// UCUM unit (`s`, `By`, `{request}`).
    pub unit: String,
    /// Instrument kind.
    pub kind: MetricKind,
    /// A counter whose values are not whole numbers.
    pub fractional: bool,
    /// Histogram bucket upper bounds.
    pub bounds: Vec<f64>,
    /// One entry per series.
    pub points: Vec<PointData>,
}

/// A snapshot of one series. Counters and gauges set `value`; histograms set
/// `count`, `sum` and `bucket_counts` (`bounds.len() + 1`, the last for
/// values above every bound).
#[derive(Clone, Debug)]
pub struct PointData {
    /// Label names and values, in declaration order.
    pub labels: Vec<(String, String)>,
    /// When the series was created.
    pub start: SystemTime,
    /// When the snapshot was taken.
    pub time: SystemTime,
    /// Counter or gauge value.
    pub value: f64,
    /// Histogram count.
    pub count: u64,
    /// Histogram sum.
    pub sum: f64,
    /// Histogram bucket counts (not cumulative).
    pub bucket_counts: Vec<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instruments_record_and_snapshot() {
        let registry = Registry::default();
        let counter = registry
            .counter("requests", "Requests.", "{request}", &["method"])
            .expect("valid");
        counter.add(2, &["GET"]);
        counter.add(1, &["POST"]);
        counter.add(1, &["GET"]);
        let gauge = registry
            .gauge("active", "", "{request}", &[])
            .expect("valid");
        gauge.add(3.0, &[]);
        gauge.add(-1.0, &[]);
        let histogram = registry
            .histogram("latency", "", "s", &[0.1, 1.0], &[])
            .expect("valid");
        for value in [0.05, 0.1, 0.5, 2.0] {
            histogram.record(value, &[]);
        }

        let collected = registry.collect();
        let names: Vec<&str> = collected
            .iter()
            .map(|metric| metric.name.as_str())
            .collect();
        assert_eq!(names, ["active", "latency", "requests"]);
        assert_eq!(collected[0].points[0].value, 2.0);
        assert_eq!(collected[1].points[0].bucket_counts, [2, 1, 1]);
        assert_eq!(collected[1].points[0].count, 4);
        assert!((collected[1].points[0].sum - 2.65).abs() < 1e-9);
        let requests = &collected[2];
        assert_eq!(requests.points[0].labels[0].1, "GET");
        assert_eq!(requests.points[0].value, 3.0);
        assert_eq!(requests.points[1].value, 1.0);
    }

    #[test]
    fn definitions_are_validated_and_shared() {
        let registry = Registry::default();
        let first = registry.counter("a.b", "", "1", &["x"]).expect("valid");
        let second = registry
            .counter("a.b", "", "1", &["x"])
            .expect("same definition");
        first.add(1, &["v"]);
        second.add(1, &["v"]);
        assert_eq!(
            registry
                .collect()
                .iter()
                .find(|m| m.name == "a.b")
                .map(|m| m.points[0].value),
            Some(2.0)
        );
        assert_eq!(
            registry
                .gauge("a.b", "", "1", &["x"])
                .expect_err("conflict"),
            MetricError::Conflict("a.b".to_owned())
        );
        assert!(registry.counter("1bad", "", "", &[]).is_err());
        assert!(registry.counter("ok", "", "", &["le"]).is_err());
        assert!(registry.counter("ok", "", "", &["a", "a"]).is_err());
        assert!(
            registry
                .counter("ok", "", "", &["a", "b", "c", "d", "e", "f", "g"])
                .is_err()
        );
        assert!(registry.histogram("h", "", "s", &[1.0, 1.0], &[]).is_err());
        assert!(registry.histogram("h", "", "s", &[f64::NAN], &[]).is_err());
    }

    #[test]
    fn series_beyond_the_limit_fold_into_overflow() {
        let registry = Registry::new(2);
        let counter = registry.counter("c", "", "", &["k"]).expect("valid");
        for value in ["a", "b", "c", "d"] {
            counter.add(1, &[value]);
        }
        counter.add(1, &["only", "too-many-values"]);
        let collected = registry.collect();
        let series = collected.iter().find(|m| m.name == "c").expect("collected");
        assert_eq!(series.points.len(), 3);
        let overflow = series
            .points
            .iter()
            .find(|point| point.labels[0].1 == OVERFLOW_LABEL_VALUE)
            .expect("overflow series");
        assert_eq!(overflow.value, 3.0);
        let overflows = collected
            .iter()
            .find(|m| m.name == METRIC_OVERFLOWS)
            .expect("overflow counter");
        assert_eq!(overflows.points[0].value, 3.0);
    }

    #[test]
    fn label_values_are_cut() {
        let registry = Registry::default();
        let counter = registry.counter("c", "", "", &["k"]).expect("valid");
        counter.add(1, &["é".repeat(100).as_str()]);
        let collected = registry.collect();
        let value = &collected.iter().find(|m| m.name == "c").expect("c").points[0].labels[0].1;
        assert!(value.len() <= MAX_LABEL_VALUE_LENGTH);
    }

    #[test]
    fn observers_run_before_collection_and_counters_stay_monotonic() {
        let registry = Registry::default();
        let counter = registry.counter("total", "", "", &[]).expect("valid");
        let waited = registry
            .float_counter("waited", "", "s", &[])
            .expect("valid");
        let totals = Arc::new(Mutex::new((5_u64, 1.5_f64)));
        let shared = Arc::clone(&totals);
        let (c, w) = (counter.clone(), waited.clone());
        registry.observe("source", move || {
            let (count, seconds) = *shared.lock().expect("lock");
            c.observe(count, &[]);
            w.observe(seconds, &[]);
        });
        registry.observe("panics", || panic!("ignored"));
        let value = |name: &str| {
            registry
                .collect()
                .into_iter()
                .find(|m| m.name == name)
                .map(|m| m.points[0].value)
        };
        assert_eq!(value("total"), Some(5.0));
        *totals.lock().expect("lock") = (3, 0.5);
        assert_eq!(value("total"), Some(5.0), "never decreases");
        assert_eq!(value("waited"), Some(1.5));
        registry.unobserve("source");
        *totals.lock().expect("lock") = (9, 9.0);
        assert_eq!(value("total"), Some(5.0));
    }

    #[test]
    fn zero_observation_creates_the_series() {
        let registry = Registry::default();
        let counter = registry.counter("waits", "", "", &["pool"]).expect("valid");
        counter.observe(0, &["default"]);
        assert!(registry.collect().iter().any(|m| m.name == "waits"));
    }
}
