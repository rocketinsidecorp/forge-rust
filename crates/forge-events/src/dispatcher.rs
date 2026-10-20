//! The outbox dispatcher: claims committed events under a fenced lease and
//! publishes them with retries, a timeout and panic isolation.

use std::{sync::Arc, time::Duration};

use forge_core::Cancellation;
use forge_jobs::Backoff;
use tokio::task::JoinSet;
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::{
    EventEnvelope, EventPublisher, EventsError, PublishError,
    postgres::{AfterFailure, PgOutboxStore},
};

/// Dispatcher timing and retry policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct DispatcherSettings {
    /// Events claimed per round trip (1 to 1000, default 50).
    pub batch: u16,
    /// Wait between claims while the outbox is empty, jittered (default 1 s).
    pub poll_interval: Duration,
    /// Deadline of one publication (default 10 s).
    pub publish_timeout: Duration,
    /// Publication attempts before an event is dead (default 20).
    pub max_attempts: u16,
    /// Delay between attempts (default 5 s doubling to one hour, jittered).
    pub backoff: Backoff,
}

impl Default for DispatcherSettings {
    fn default() -> Self {
        Self {
            batch: 50,
            poll_interval: Duration::from_secs(1),
            publish_timeout: Duration::from_secs(10),
            max_attempts: 20,
            backoff: Backoff::DEFAULT,
        }
    }
}

impl DispatcherSettings {
    /// Sets the batch size.
    #[must_use]
    pub const fn batch(mut self, batch: u16) -> Self {
        self.batch = batch;
        self
    }

    /// Sets the idle poll interval.
    #[must_use]
    pub const fn poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }

    /// Sets the publication deadline.
    #[must_use]
    pub const fn publish_timeout(mut self, timeout: Duration) -> Self {
        self.publish_timeout = timeout;
        self
    }

    /// Sets the attempt budget.
    #[must_use]
    pub const fn max_attempts(mut self, attempts: u16) -> Self {
        self.max_attempts = attempts;
        self
    }

    /// Sets the retry backoff.
    #[must_use]
    pub const fn backoff(mut self, backoff: Backoff) -> Self {
        self.backoff = backoff;
        self
    }

    fn validate(&self) -> Result<(), EventsError> {
        if !(1..=1000).contains(&self.batch) {
            return Err(EventsError::InvalidSetting(
                "batch must be between 1 and 1000",
            ));
        }
        if self.poll_interval < Duration::from_millis(10) {
            return Err(EventsError::InvalidSetting(
                "poll_interval must be at least 10 ms",
            ));
        }
        if self.publish_timeout < Duration::from_millis(10)
            || self.publish_timeout > Duration::from_secs(600)
        {
            return Err(EventsError::InvalidSetting(
                "publish_timeout must be between 10 ms and 10 minutes",
            ));
        }
        if self.max_attempts == 0 {
            return Err(EventsError::InvalidSetting(
                "max_attempts must be at least 1",
            ));
        }
        Ok(())
    }

    /// The claim lease: long enough to publish a whole batch sequentially.
    fn lease(&self) -> Duration {
        (self.publish_timeout * u32::from(self.batch) + Duration::from_secs(30))
            .min(Duration::from_secs(86_400))
    }
}

/// Publishes committed outbox events until shut down.
pub struct Dispatcher {
    store: PgOutboxStore,
    publisher: Arc<dyn EventPublisher>,
    settings: DispatcherSettings,
}

impl std::fmt::Debug for Dispatcher {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Dispatcher")
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

impl Dispatcher {
    /// A dispatcher publishing to `publisher` with default settings.
    #[must_use]
    pub fn new(store: PgOutboxStore, publisher: impl EventPublisher) -> Self {
        Self {
            store,
            publisher: Arc::new(publisher),
            settings: DispatcherSettings::default(),
        }
    }

    /// Replaces the settings.
    #[must_use]
    pub const fn settings(mut self, settings: DispatcherSettings) -> Self {
        self.settings = settings;
        self
    }

    /// Runs until `shutdown`: the publication in progress finishes (bounded
    /// by the publish timeout) and the rest of the claimed batch is released
    /// without consuming attempts.
    pub async fn run(self, shutdown: Cancellation) -> Result<(), EventsError> {
        self.settings.validate()?;
        forge_jobs::panics::redact_attempt_panics();
        info!(batch = self.settings.batch, "outbox dispatcher started");
        while !shutdown.is_cancelled() {
            let token = Uuid::now_v7();
            let claimed = match self
                .store
                .claim(
                    self.settings.batch,
                    token,
                    self.settings.lease(),
                    self.settings.max_attempts,
                )
                .await
            {
                Ok(claimed) => claimed,
                Err(error) => {
                    warn!(%error, "claiming outbox events failed");
                    Vec::new()
                }
            };
            let full = claimed.len() == usize::from(self.settings.batch);
            let mut events = claimed.into_iter();
            for event in events.by_ref() {
                self.dispatch(&event, token).await;
                if shutdown.is_cancelled() {
                    break;
                }
            }
            for event in events {
                if let Err(error) = self.store.release(event.id(), token).await {
                    warn!(event_id = %event.id(), %error, "could not release an outbox event; its lease will expire");
                }
            }
            if !full {
                let wait = Backoff::fixed(self.settings.poll_interval).delay(1);
                tokio::select! {
                    () = shutdown.cancelled() => break,
                    () = tokio::time::sleep(wait) => {}
                }
            }
        }
        info!("outbox dispatcher stopped");
        Ok(())
    }

    async fn dispatch(&self, event: &EventEnvelope, token: Uuid) {
        let publisher = Arc::clone(&self.publisher);
        let copy = event.clone();
        let timeout = self.settings.publish_timeout;
        // A task of its own so a panicking publisher fails one attempt
        // instead of the dispatcher; awaited before the next event.
        let mut task = JoinSet::new();
        task.spawn(forge_jobs::panics::redacted(async move {
            tokio::time::timeout(timeout, publisher.publish(&copy)).await
        }));
        let outcome = match task.join_next().await {
            Some(Ok(Ok(Ok(())))) => Ok(()),
            Some(Ok(Ok(Err(failure)))) => Err(failure),
            Some(Ok(Err(_elapsed))) => Err(PublishError::retry("publication exceeded its timeout")),
            Some(Err(_)) | None => Err(PublishError::retry("the publisher panicked")),
        };
        let fields = (event.id(), event.event_type(), event.attempt());
        match outcome {
            Ok(()) => match self.store.published(event.id(), token).await {
                Ok(true) => {
                    info!(event_id = %fields.0, event_type = fields.1, attempt = fields.2, "event published")
                }
                Ok(false) => {
                    warn!(event_id = %fields.0, event_type = fields.1, "event published after its lease was lost")
                }
                Err(error) => {
                    warn!(event_id = %fields.0, %error, "could not record publication; the event will be published again")
                }
            },
            Err(failure) => {
                let delay = if failure.is_permanent() {
                    Duration::ZERO
                } else {
                    self.settings.backoff.delay(event.attempt())
                };
                let message = sanitize(failure.message());
                match self
                    .store
                    .fail(
                        event.id(),
                        token,
                        &message,
                        delay,
                        self.settings.max_attempts,
                        failure.is_permanent(),
                    )
                    .await
                {
                    Ok(Some(AfterFailure::Retrying)) => {
                        warn!(event_id = %fields.0, event_type = fields.1, attempt = fields.2, error = %message, "publication failed; will retry")
                    }
                    Ok(Some(AfterFailure::Dead)) => {
                        error!(event_id = %fields.0, event_type = fields.1, attempt = fields.2, error = %message, "publication failed; event is dead")
                    }
                    Ok(None) => {
                        warn!(event_id = %fields.0, "publication failed after its lease was lost")
                    }
                    Err(error) => {
                        warn!(event_id = %fields.0, %error, "could not record the failed publication")
                    }
                }
            }
        }
    }
}

/// At most 1024 bytes, control characters replaced.
fn sanitize(message: &str) -> String {
    let mut out = String::new();
    for character in message.chars() {
        let character = if character.is_control() {
            ' '
        } else {
            character
        };
        if out.len() + character.len_utf8() > 1024 {
            break;
        }
        out.push(character);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_are_validated_and_leases_cover_a_batch() {
        assert!(DispatcherSettings::default().validate().is_ok());
        assert!(DispatcherSettings::default().batch(0).validate().is_err());
        assert!(
            DispatcherSettings::default()
                .max_attempts(0)
                .validate()
                .is_err()
        );
        assert!(
            DispatcherSettings::default()
                .publish_timeout(Duration::ZERO)
                .validate()
                .is_err()
        );
        let settings = DispatcherSettings::default()
            .batch(10)
            .publish_timeout(Duration::from_secs(2));
        assert_eq!(settings.lease(), Duration::from_secs(50));
        assert_eq!(sanitize("a\nb"), "a b");
        assert!(sanitize(&"x".repeat(5000)).len() <= 1024);
    }
}
