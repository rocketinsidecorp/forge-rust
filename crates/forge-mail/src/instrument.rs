//! `forge.mail.send.duration` and the `mail send` span.

use std::{sync::Arc, time::Instant};

use async_trait::async_trait;
use forge_telemetry::{
    SpanContext, Telemetry,
    mail::{MailInstrumentation, OUTCOME_SENT},
};

use crate::{MailError, Message, Sender};

/// A [`Sender`] that measures every send in `forge.mail.send.duration`
/// (label `forge.mail.outcome`: `sent` or the error code, such as
/// `mail_unavailable`) and, through [`Instrumented::send_in`] inside a
/// traced operation, records a `mail send` client span. Recipients,
/// subjects, bodies, headers and error messages are never recorded.
#[derive(Clone, Debug)]
pub struct Instrumented<S> {
    inner: S,
    instruments: MailInstrumentation,
}

impl<S: Sender> Instrumented<S> {
    /// Wraps `inner`, registering the instruments in `telemetry`.
    pub fn new(inner: S, telemetry: &Telemetry) -> Result<Self, MailError> {
        let instruments = telemetry
            .mail()
            .map_err(|error| MailError::Configuration(format!("mail metrics: {error}")))?;
        Ok(Self { inner, instruments })
    }

    /// Sends inside the trace of `parent` (for example a request's
    /// `trace_context()`), with a `mail send` child span.
    pub async fn send_in(
        &self,
        parent: Option<&SpanContext>,
        message: &Message,
    ) -> Result<(), MailError> {
        let started = Instant::now();
        let span = self.instruments.start(parent);
        let result = self.inner.send(message).await;
        let outcome = match &result {
            Ok(()) => OUTCOME_SENT,
            Err(error) => error.code(),
        };
        self.instruments.finish(span, outcome, started.elapsed());
        result
    }
}

#[async_trait]
impl<S: Sender> Sender for Instrumented<S> {
    async fn send(&self, message: &Message) -> Result<(), MailError> {
        self.send_in(None, message).await
    }
}

/// Wraps `sender` in [`Instrumented`] when telemetry is enabled, and returns
/// it unchanged otherwise.
pub fn instrument(
    sender: Arc<dyn Sender>,
    telemetry: Option<&Telemetry>,
) -> Result<Arc<dyn Sender>, MailError> {
    match telemetry {
        Some(telemetry) => Ok(Arc::new(Instrumented::new(sender, telemetry)?)),
        None => Ok(sender),
    }
}
