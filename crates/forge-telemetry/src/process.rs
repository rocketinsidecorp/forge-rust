//! Process and Tokio runtime metrics, read when metrics are collected (a
//! scrape or an OTLP push), never on a timer of their own.
//!
//! Forge-Go records `go.*` runtime metrics; Rust has no garbage collector or
//! goroutines, so Forge-Rust records the OpenTelemetry process conventions
//! it can read cheaply on Linux (`/proc/self`) and the stable Tokio runtime
//! counters: the live task count is the goroutine count's counterpart (a
//! count that only grows is a task leak).

use std::sync::Arc;

use crate::metrics::{MetricError, Registry};

/// Resident memory (`VmRSS`), bytes.
pub const METRIC_MEMORY_USAGE: &str = "process.memory.usage";
/// Virtual memory (`VmSize`), bytes.
pub const METRIC_MEMORY_VIRTUAL: &str = "process.memory.virtual";
/// User plus system CPU time, seconds.
pub const METRIC_CPU_TIME: &str = "process.cpu.time";
/// Operating-system threads.
pub const METRIC_THREAD_COUNT: &str = "process.thread.count";
/// Open file descriptors.
pub const METRIC_FILE_DESCRIPTORS: &str = "process.unix.file_descriptor.count";
/// Tokio worker threads.
pub const METRIC_TOKIO_WORKERS: &str = "tokio.worker.count";
/// Tasks alive in the Tokio runtime.
pub const METRIC_TOKIO_TASKS: &str = "tokio.task.count";
/// Tasks waiting in the runtime's global queue.
pub const METRIC_TOKIO_GLOBAL_QUEUE: &str = "tokio.global_queue.depth";

/// Registers the process and runtime instruments and their observer.
pub(crate) fn register(registry: &Registry) -> Result<(), MetricError> {
    let workers = registry.gauge(
        METRIC_TOKIO_WORKERS,
        "Worker threads of the Tokio runtime.",
        "{thread}",
        &[],
    )?;
    let tasks = registry.gauge(
        METRIC_TOKIO_TASKS,
        "Tasks alive in the Tokio runtime.",
        "{task}",
        &[],
    )?;
    let queue = registry.gauge(
        METRIC_TOKIO_GLOBAL_QUEUE,
        "Tasks waiting in the Tokio runtime's global queue.",
        "{task}",
        &[],
    )?;
    let linux = linux::Instruments::register(registry)?;
    let linux = Arc::new(linux);
    registry.observe("forge.process", move || {
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let metrics = handle.metrics();
            workers.set(metrics.num_workers() as f64, &[]);
            tasks.set(metrics.num_alive_tasks() as f64, &[]);
            queue.set(metrics.global_queue_depth() as f64, &[]);
        }
        linux.observe();
    });
    Ok(())
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use crate::metrics::{FloatCounter, Gauge};

    /// `USER_HZ`: the unit of `/proc/<pid>/stat` CPU times, 100 on every
    /// Linux architecture Rust supports for servers.
    const CLOCK_TICKS_PER_SECOND: f64 = 100.0;

    pub(super) struct Instruments {
        rss: Gauge,
        virtual_memory: Gauge,
        cpu: FloatCounter,
        threads: Gauge,
        descriptors: Gauge,
    }

    impl Instruments {
        pub(super) fn register(registry: &Registry) -> Result<Self, MetricError> {
            Ok(Self {
                rss: registry.gauge(
                    METRIC_MEMORY_USAGE,
                    "Resident memory of the process.",
                    "By",
                    &[],
                )?,
                virtual_memory: registry.gauge(
                    METRIC_MEMORY_VIRTUAL,
                    "Virtual memory of the process.",
                    "By",
                    &[],
                )?,
                cpu: registry.float_counter(
                    METRIC_CPU_TIME,
                    "CPU time used by the process, user and system.",
                    "s",
                    &[],
                )?,
                threads: registry.gauge(
                    METRIC_THREAD_COUNT,
                    "Operating-system threads of the process.",
                    "{thread}",
                    &[],
                )?,
                descriptors: registry.gauge(
                    METRIC_FILE_DESCRIPTORS,
                    "Open file descriptors of the process.",
                    "{file_descriptor}",
                    &[],
                )?,
            })
        }

        pub(super) fn observe(&self) {
            if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
                for line in status.lines() {
                    let (key, value) = line.split_once(':').unwrap_or_default();
                    let number = || {
                        value
                            .split_whitespace()
                            .next()
                            .and_then(|text| text.parse::<f64>().ok())
                    };
                    match key {
                        "VmRSS" => {
                            if let Some(kib) = number() {
                                self.rss.set(kib * 1024.0, &[]);
                            }
                        }
                        "VmSize" => {
                            if let Some(kib) = number() {
                                self.virtual_memory.set(kib * 1024.0, &[]);
                            }
                        }
                        "Threads" => {
                            if let Some(count) = number() {
                                self.threads.set(count, &[]);
                            }
                        }
                        _ => {}
                    }
                }
            }
            if let Ok(stat) = std::fs::read_to_string("/proc/self/stat")
                && let Some((_, after_name)) = stat.rsplit_once(')')
            {
                // Fields after the command name start at field 3 (state);
                // utime and stime are fields 14 and 15.
                let fields: Vec<&str> = after_name.split_whitespace().collect();
                if let (Some(user), Some(system)) = (
                    fields.get(11).and_then(|text| text.parse::<f64>().ok()),
                    fields.get(12).and_then(|text| text.parse::<f64>().ok()),
                ) {
                    self.cpu
                        .observe((user + system) / CLOCK_TICKS_PER_SECOND, &[]);
                }
            }
            if let Ok(entries) = std::fs::read_dir("/proc/self/fd") {
                // The directory handle itself is one of the entries.
                let count = entries.count().saturating_sub(1);
                self.descriptors.set(count as f64, &[]);
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod linux {
    use super::*;

    /// Process metrics are read from `/proc`; other systems record none.
    pub(super) struct Instruments;

    impl Instruments {
        pub(super) fn register(_registry: &Registry) -> Result<Self, MetricError> {
            Ok(Self)
        }

        pub(super) fn observe(&self) {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn process_and_runtime_metrics_are_read_on_collection() {
        let registry = Registry::default();
        register(&registry).expect("registers");
        let collected = registry.collect();
        let value = |name: &str| {
            collected
                .iter()
                .find(|metric| metric.name == name)
                .map(|metric| metric.points[0].value)
        };
        assert!(value(METRIC_TOKIO_WORKERS).is_some());
        assert!(value(METRIC_TOKIO_TASKS).is_some());
        if cfg!(target_os = "linux") {
            assert!(value(METRIC_MEMORY_USAGE).is_some_and(|bytes| bytes > 0.0));
            assert!(value(METRIC_THREAD_COUNT).is_some_and(|count| count >= 1.0));
            assert!(value(METRIC_FILE_DESCRIPTORS).is_some_and(|count| count >= 1.0));
            assert!(value(METRIC_CPU_TIME).is_some());
        }
    }
}
