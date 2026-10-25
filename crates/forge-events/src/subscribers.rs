//! In-process fan-out of committed events to subscribers.

use std::{collections::HashMap, fmt, sync::Arc};

use async_trait::async_trait;

use crate::{
    EventEnvelope, EventPublisher, EventsError, IntegrationEvent, PublishError, validate_name,
};

/// Handles committed events of the types it is subscribed to.
///
/// Delivery is at-least-once: when any subscriber of an event fails, the
/// event is published again to **every** subscriber. Make handling
/// idempotent with [`record_delivery`](crate::postgres::record_delivery).
#[async_trait]
pub trait EventSubscriber: Send + Sync + 'static {
    /// Handles one event.
    async fn handle(&self, event: &EventEnvelope) -> Result<(), PublishError>;
}

/// An [`EventPublisher`] that routes each event to the subscribers of its
/// type, in subscription order. Events without subscribers are published
/// (and dropped) successfully.
///
/// ```
/// use forge_events::{EventEnvelope, EventSubscriber, IntegrationEvent, PublishError, Subscribers};
/// use serde::{Deserialize, Serialize};
///
/// #[derive(Serialize, Deserialize)]
/// struct InvoicePaid { invoice_id: uuid::Uuid }
///
/// impl IntegrationEvent for InvoicePaid {
///     const TYPE: &'static str = "billing.invoice_paid";
/// }
///
/// struct Ledger;
///
/// #[async_trait::async_trait]
/// impl EventSubscriber for Ledger {
///     async fn handle(&self, event: &EventEnvelope) -> Result<(), PublishError> {
///         let _paid: InvoicePaid = event.decode().map_err(PublishError::permanent)?;
///         Ok(())
///     }
/// }
///
/// let mut subscribers = Subscribers::new();
/// subscribers.subscribe::<InvoicePaid>(Ledger)?;
/// # Ok::<(), forge_events::EventsError>(())
/// ```
#[derive(Default)]
pub struct Subscribers {
    routes: HashMap<String, Vec<Arc<dyn EventSubscriber>>>,
}

impl fmt::Debug for Subscribers {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut types: Vec<(&String, usize)> = self
            .routes
            .iter()
            .map(|(event_type, subscribers)| (event_type, subscribers.len()))
            .collect();
        types.sort();
        formatter
            .debug_struct("Subscribers")
            .field("routes", &types)
            .finish()
    }
}

impl Subscribers {
    /// No subscriptions.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Subscribes to events of type `E` (any version: check
    /// [`EventEnvelope::is`] or [`EventEnvelope::decode`]).
    pub fn subscribe<E: IntegrationEvent>(
        &mut self,
        subscriber: impl EventSubscriber,
    ) -> Result<&mut Self, EventsError> {
        self.subscribe_type(E::TYPE, subscriber)
    }

    /// Subscribes to events whose type is `event_type`.
    pub fn subscribe_type(
        &mut self,
        event_type: &str,
        subscriber: impl EventSubscriber,
    ) -> Result<&mut Self, EventsError> {
        validate_name("event type", event_type)?;
        self.routes
            .entry(event_type.to_owned())
            .or_default()
            .push(Arc::new(subscriber));
        Ok(self)
    }

    /// Whether nothing is subscribed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.routes.is_empty()
    }
}

#[async_trait]
impl EventPublisher for Subscribers {
    async fn publish(&self, event: &EventEnvelope) -> Result<(), PublishError> {
        let Some(subscribers) = self.routes.get(event.event_type()) else {
            return Ok(());
        };
        let mut first_failure = None;
        for subscriber in subscribers {
            if let Err(failure) = subscriber.handle(event).await {
                first_failure.get_or_insert(failure);
            }
        }
        first_failure.map_or(Ok(()), Err)
    }
}

#[cfg(test)]
mod subscribers_tests;
