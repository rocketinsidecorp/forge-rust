//! Mail delivery instrumentation with Forge-Go's names: the
//! `forge.mail.send.duration` histogram (label `forge.mail.outcome`: `sent`
//! or the error code, such as `mail_unavailable`) and, inside a traced
//! operation, a `mail send` client span. Recipients, subjects, bodies,
//! headers and error messages are never recorded.

use std::time::Duration;

use crate::{
    SpanContext, Telemetry,
    metrics::{DURATION_BUCKETS, Histogram, MetricError},
    trace::{Span, SpanKind, Tracer},
};

/// `forge.mail.send.duration`.
pub const METRIC_SEND_DURATION: &str = "forge.mail.send.duration";

/// The outcome label of a successful delivery.
pub const OUTCOME_SENT: &str = "sent";

/// Mail delivery instruments; create once per sender.
#[derive(Clone, Debug)]
pub struct MailInstrumentation {
    tracer: Tracer,
    duration: Histogram,
}

impl Telemetry {
    /// Registers the mail delivery instruments.
    pub fn mail(&self) -> Result<MailInstrumentation, MetricError> {
        Ok(MailInstrumentation {
            tracer: self.tracer("forge/mail"),
            duration: self.metrics().histogram(
                METRIC_SEND_DURATION,
                "Duration of mail deliveries.",
                "s",
                DURATION_BUCKETS,
                &["forge.mail.outcome"],
            )?,
        })
    }
}

impl MailInstrumentation {
    /// Starts the `mail send` client span when `parent` is a valid span
    /// context, as Forge-Go does inside a traced operation; a delivery
    /// outside one (a background worker) starts no trace of its own.
    #[must_use]
    pub fn start(&self, parent: Option<&SpanContext>) -> Option<Span> {
        let parent = parent.filter(|parent| parent.is_valid())?;
        Some(
            self.tracer
                .start("mail send", SpanKind::Client, Some(parent)),
        )
    }

    /// Ends the span, if any, and records the duration. `outcome` is
    /// [`OUTCOME_SENT`] or an error code; any other outcome marks the span
    /// as an error.
    pub fn finish(&self, span: Option<Span>, outcome: &str, elapsed: Duration) {
        if let Some(span) = span {
            if outcome != OUTCOME_SENT && span.is_recording() {
                span.record_error_type(outcome);
            }
            span.end();
        }
        self.duration.record(elapsed.as_secs_f64(), &[outcome]);
    }
}
