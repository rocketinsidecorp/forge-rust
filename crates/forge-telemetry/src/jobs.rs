//! Background work instrumentation with Forge-Go's names: one consumer span
//! per job attempt or outbox delivery (each a new trace) and the
//! `forge.queue.job.duration` and `forge.outbox.dispatch.duration`
//! histograms. Payloads and error messages are never recorded.

use std::time::Duration;

use crate::{
    Telemetry,
    metrics::{DURATION_BUCKETS, Histogram, MetricError},
    trace::{Attribute, Span, SpanKind, Tracer},
};

/// `forge.queue.job.duration`.
pub const METRIC_JOB_DURATION: &str = "forge.queue.job.duration";
/// `forge.outbox.dispatch.duration`.
pub const METRIC_DISPATCH_DURATION: &str = "forge.outbox.dispatch.duration";
const MESSAGING_SYSTEM: &str = "forge.queue";

/// Job attempt instruments; create once per job runtime.
#[derive(Clone, Debug)]
pub struct JobInstrumentation {
    tracer: Tracer,
    duration: Histogram,
}

/// Outbox delivery instruments; create once per dispatcher.
#[derive(Clone, Debug)]
pub struct OutboxInstrumentation {
    tracer: Tracer,
    duration: Histogram,
}

impl Telemetry {
    /// Registers the job attempt instruments.
    pub fn jobs(&self) -> Result<JobInstrumentation, MetricError> {
        Ok(JobInstrumentation {
            tracer: self.tracer("forge/queue"),
            duration: self.metrics().histogram(
                METRIC_JOB_DURATION,
                "Duration of queue job attempts.",
                "s",
                DURATION_BUCKETS,
                &[
                    "messaging.destination.name",
                    "forge.job.kind",
                    "forge.job.outcome",
                ],
            )?,
        })
    }

    /// Registers the outbox delivery instruments.
    pub fn outbox(&self) -> Result<OutboxInstrumentation, MetricError> {
        Ok(OutboxInstrumentation {
            tracer: self.tracer("forge/outbox"),
            duration: self.metrics().histogram(
                METRIC_DISPATCH_DURATION,
                "Duration of outbox event delivery attempts.",
                "s",
                DURATION_BUCKETS,
                &["forge.event.type", "forge.outbox.outcome"],
            )?,
        })
    }
}

impl JobInstrumentation {
    /// Starts the span of one attempt: `process <queue>`, a new trace.
    #[must_use]
    pub fn start(&self, queue: &str, kind: &str, job_id: &str, attempt: u32) -> Span {
        let span = self
            .tracer
            .start(format!("process {queue}"), SpanKind::Consumer, None);
        if span.is_recording() {
            span.set_attributes([
                Attribute::string("messaging.system", MESSAGING_SYSTEM),
                Attribute::string("messaging.operation.type", "process"),
                Attribute::string("messaging.destination.name", queue),
                Attribute::string("messaging.message.id", job_id),
                Attribute::string("forge.job.kind", kind),
                Attribute::int("forge.job.attempt", i64::from(attempt)),
            ]);
        }
        span
    }

    /// Ends the attempt's span and records its duration. `outcome` is
    /// `succeeded`, `retry`, `dead`, `released`, `lease_lost` or
    /// `store_failed`; every outcome but `succeeded` and `released` marks
    /// the span as an error.
    pub fn finish(&self, span: Span, queue: &str, kind: &str, outcome: &str, elapsed: Duration) {
        if span.is_recording() {
            span.set_attributes([Attribute::string("forge.job.outcome", outcome)]);
            if !matches!(outcome, "succeeded" | "released") {
                span.record_error_type(outcome);
            }
        }
        span.end();
        self.duration
            .record(elapsed.as_secs_f64(), &[queue, kind, outcome]);
    }
}

impl OutboxInstrumentation {
    /// Starts the span of one delivery: `process outbox`, a new trace.
    #[must_use]
    pub fn start(&self, event_id: &str, event_type: &str, version: i64, attempt: u32) -> Span {
        let span = self
            .tracer
            .start("process outbox", SpanKind::Consumer, None);
        if span.is_recording() {
            span.set_attributes([
                Attribute::string("messaging.system", "forge.outbox"),
                Attribute::string("messaging.operation.type", "process"),
                Attribute::string("messaging.message.id", event_id),
                Attribute::string("forge.event.type", event_type),
                Attribute::int("forge.event.version", version),
                Attribute::int("forge.outbox.attempt", i64::from(attempt)),
            ]);
        }
        span
    }

    /// Ends the delivery's span and records its duration. `outcome` is
    /// Forge-Go's: `delivered`, `retry`, `dead_lettered`, `lease_lost` or
    /// `error` (the outcome could not be recorded).
    pub fn finish(&self, span: Span, event_type: &str, outcome: &str, elapsed: Duration) {
        if span.is_recording() {
            span.set_attributes([Attribute::string("forge.outbox.outcome", outcome)]);
            if outcome != "delivered" {
                span.record_error_type(outcome);
            }
        }
        span.end();
        self.duration
            .record(elapsed.as_secs_f64(), &[event_type, outcome]);
    }
}
