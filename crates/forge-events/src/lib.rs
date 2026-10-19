//! Integration events, a transactional outbox and idempotent consumers for
//! Forge (ADR 0025).
//!
//! # Flow
//!
//! 1. A use case appends a versioned [`IntegrationEvent`] to the outbox **in
//!    the same transaction** as the state change it describes
//!    ([`postgres::append`], [`postgres::append_for_tenant`]). Nothing is
//!    published if the transaction rolls back.
//! 2. A [`Dispatcher`] claims committed events in id (time) order under a
//!    fenced lease and hands each to an [`EventPublisher`]. Failures are
//!    retried with backoff; an event that exhausts its attempts or fails
//!    permanently becomes **dead** until an operator retries it.
//! 3. Consumers make handling idempotent with the inbox
//!    ([`postgres::record_delivery`]): they record `(consumer, event id)` in
//!    the transaction that applies the event's effects and skip duplicates.
//!
//! Delivery is **at-least-once** (an event can be published again after a
//! crash between publishing and acknowledging); effects are exactly-once for
//! consumers that use the inbox. Forge does not promise global ordering:
//! events are dispatched in id order per batch, but retries and concurrent
//! dispatchers can reorder them.
//!
//! PostgreSQL only; other backends are refused with
//! [`forge_jobs::ensure_supported`].

#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]
#![deny(missing_docs)]

use std::{fmt, time::SystemTime};

use async_trait::async_trait;
use forge_core::Id;
use forge_tenancy::TenantId;
use serde::{Serialize, de::DeserializeOwned};
use thiserror::Error;

mod dispatcher;
#[cfg(test)]
mod pg_tests;
pub mod postgres;
mod subscribers;

pub use dispatcher::{Dispatcher, DispatcherSettings};
pub use subscribers::{EventSubscriber, Subscribers};

/// Largest serialized event payload, in bytes (256 KiB).
pub const MAX_PAYLOAD_BYTES: usize = 256 * 1024;
/// Longest event type or consumer name, in bytes.
pub const MAX_NAME: usize = 128;

/// Marker for event identifiers.
#[derive(Debug)]
pub enum EventMarker {}

/// UUIDv7 event identifier, generated in Rust when the event is appended. It
/// is the idempotency key consumers record.
pub type EventId = Id<EventMarker>;

/// A versioned integration event: an external contract, unlike in-process
/// domain events.
///
/// `TYPE` and `VERSION` identify the schema; publish a new version (or a new
/// type) instead of changing the meaning of an existing one. Payloads are
/// stored as JSON: never put secrets in them.
///
/// ```
/// use forge_events::IntegrationEvent;
/// use serde::{Deserialize, Serialize};
///
/// #[derive(Serialize, Deserialize)]
/// struct InvoicePaid {
///     invoice_id: uuid::Uuid,
///     amount_cents: i64,
/// }
///
/// impl IntegrationEvent for InvoicePaid {
///     const TYPE: &'static str = "billing.invoice_paid";
///     const VERSION: u16 = 1;
/// }
/// ```
pub trait IntegrationEvent: Serialize + DeserializeOwned + Send + Sync + 'static {
    /// Stable event type: lowercase letters, digits and `_ . : -`.
    const TYPE: &'static str;
    /// Schema version, at least 1.
    const VERSION: u16 = 1;
}

/// A committed event as delivered to publishers and consumers.
#[derive(Clone, Eq, PartialEq)]
pub struct EventEnvelope {
    id: EventId,
    event_type: String,
    version: u16,
    tenant_id: Option<TenantId>,
    occurred_at: SystemTime,
    attempt: u32,
    payload: String,
}

impl fmt::Debug for EventEnvelope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The payload is deliberately omitted.
        formatter
            .debug_struct("EventEnvelope")
            .field("id", &self.id)
            .field("event_type", &self.event_type)
            .field("version", &self.version)
            .field("tenant_id", &self.tenant_id)
            .field("attempt", &self.attempt)
            .finish_non_exhaustive()
    }
}

impl EventEnvelope {
    /// The event's identifier (the consumer idempotency key).
    #[must_use]
    pub const fn id(&self) -> EventId {
        self.id
    }

    /// The event type.
    #[must_use]
    pub fn event_type(&self) -> &str {
        &self.event_type
    }

    /// The schema version.
    #[must_use]
    pub const fn version(&self) -> u16 {
        self.version
    }

    /// The owning tenant, for tenant-scoped events.
    #[must_use]
    pub const fn tenant_id(&self) -> Option<TenantId> {
        self.tenant_id
    }

    /// When the event was appended (application clock).
    #[must_use]
    pub const fn occurred_at(&self) -> SystemTime {
        self.occurred_at
    }

    /// The delivery attempt, starting at 1.
    #[must_use]
    pub const fn attempt(&self) -> u32 {
        self.attempt
    }

    /// The JSON payload, for publishers that forward it verbatim.
    #[must_use]
    pub fn payload_json(&self) -> &str {
        &self.payload
    }

    /// Whether this envelope carries `E` (same type and version).
    #[must_use]
    pub fn is<E: IntegrationEvent>(&self) -> bool {
        self.event_type == E::TYPE && self.version == E::VERSION
    }

    /// Decodes the payload as `E`; fails when the type or version differ or
    /// the payload does not match `E`'s schema.
    pub fn decode<E: IntegrationEvent>(&self) -> Result<E, EventsError> {
        if !self.is::<E>() {
            return Err(EventsError::TypeMismatch);
        }
        serde_json::from_str(&self.payload).map_err(|_| EventsError::Decode)
    }
}

/// An events failure. Messages never contain payloads or connection URLs.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum EventsError {
    /// An event type or consumer name does not follow the name grammar.
    #[error("invalid {kind} name {name:?}")]
    InvalidName {
        /// `event type` or `consumer`.
        kind: &'static str,
        /// The rejected name (truncated).
        name: String,
    },
    /// `VERSION` is 0.
    #[error("event versions start at 1")]
    InvalidVersion,
    /// A setting is out of its documented range.
    #[error("invalid events setting: {0}")]
    InvalidSetting(&'static str),
    /// The event could not be serialized.
    #[error("the event payload could not be serialized")]
    Serialize(#[source] serde_json::Error),
    /// The serialized event exceeds [`MAX_PAYLOAD_BYTES`].
    #[error("the event payload exceeds {limit} bytes")]
    PayloadTooLarge {
        /// The limit in bytes.
        limit: usize,
    },
    /// [`EventEnvelope::decode`] was asked for another type or version.
    #[error("the event has another type or version")]
    TypeMismatch,
    /// The payload does not match the requested type's schema.
    #[error("the event payload could not be decoded")]
    Decode,
    /// The database rejected or failed an operation.
    #[error("event store operation failed")]
    Database(#[from] forge_db::DbError),
    /// A persisted row violates the event contract.
    #[error("persisted event data is invalid")]
    Corrupt,
}

impl From<forge_db::sqlx::Error> for EventsError {
    fn from(error: forge_db::sqlx::Error) -> Self {
        Self::Database(forge_db::DbError::from(error))
    }
}

/// Why publishing (or consuming) an event failed.
///
/// Any `std::error::Error` converts into a retryable `PublishError` with `?`.
/// Messages are stored (truncated to 1024 bytes); keep payload data and
/// secrets out of them.
#[derive(Debug)]
pub struct PublishError {
    message: String,
    permanent: bool,
}

impl PublishError {
    /// A transient failure: retried after backoff.
    #[must_use]
    pub fn retry(message: impl fmt::Display) -> Self {
        Self {
            message: message.to_string(),
            permanent: false,
        }
    }

    /// A failure retrying cannot fix: the event becomes dead.
    #[must_use]
    pub fn permanent(message: impl fmt::Display) -> Self {
        Self {
            message: message.to_string(),
            permanent: true,
        }
    }

    /// The message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Whether the event goes straight to the dead state.
    #[must_use]
    pub const fn is_permanent(&self) -> bool {
        self.permanent
    }
}

impl<E: std::error::Error + Send + Sync + 'static> From<E> for PublishError {
    fn from(error: E) -> Self {
        let mut message = error.to_string();
        let mut source = error.source();
        while let Some(cause) = source {
            message.push_str(": ");
            message.push_str(&cause.to_string());
            source = cause.source();
        }
        Self::retry(message)
    }
}

impl fmt::Display for PublishError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

/// Delivers committed events somewhere: an in-process [`Subscribers`]
/// registry, a message broker, a webhook. Delivery is at-least-once, so the
/// receiving side must be idempotent (keyed by [`EventEnvelope::id`]).
#[async_trait]
pub trait EventPublisher: Send + Sync + 'static {
    /// Publishes one event.
    async fn publish(&self, event: &EventEnvelope) -> Result<(), PublishError>;
}

pub(crate) fn validate_name(kind: &'static str, value: &str) -> Result<(), EventsError> {
    if forge_jobs::is_valid_name(value, MAX_NAME) {
        Ok(())
    } else {
        Err(EventsError::InvalidName {
            kind,
            name: value.chars().take(MAX_NAME).collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::time::UNIX_EPOCH;

    use serde::Deserialize;

    use super::*;

    #[derive(Debug, Serialize, Deserialize, PartialEq)]
    struct Paid {
        cents: i64,
    }

    impl IntegrationEvent for Paid {
        const TYPE: &'static str = "billing.paid";
        const VERSION: u16 = 2;
    }

    fn envelope(event_type: &str, version: u16, payload: &str) -> EventEnvelope {
        EventEnvelope {
            id: EventId::new(),
            event_type: event_type.to_owned(),
            version,
            tenant_id: None,
            occurred_at: UNIX_EPOCH,
            attempt: 1,
            payload: payload.to_owned(),
        }
    }

    #[test]
    fn envelopes_decode_only_their_own_type_and_version() {
        let paid = envelope("billing.paid", 2, r#"{"cents":42}"#);
        assert!(paid.is::<Paid>());
        assert_eq!(paid.decode::<Paid>().expect("decode"), Paid { cents: 42 });
        assert!(matches!(
            envelope("billing.paid", 1, r#"{"cents":42}"#).decode::<Paid>(),
            Err(EventsError::TypeMismatch)
        ));
        assert!(matches!(
            envelope("billing.paid", 2, r#"{"secret":"x"}"#).decode::<Paid>(),
            Err(EventsError::Decode)
        ));
        let debug = format!("{paid:?}");
        assert!(
            debug.contains("billing.paid") && !debug.contains("42"),
            "{debug}"
        );
    }

    #[test]
    fn publish_errors_classify_and_convert() {
        assert!(PublishError::permanent("bad").is_permanent());
        let converted = PublishError::from(std::io::Error::other("broker down"));
        assert!(!converted.is_permanent());
        assert_eq!(converted.to_string(), "broker down");
        assert!(validate_name("consumer", "billing.ledger").is_ok());
        assert!(validate_name("consumer", "Billing").is_err());
    }
}
