//! Storage port of the `events` resource, implemented in
//! `crate::infrastructure::database::events` and by in-memory fakes in
//! tests. It speaks only domain and Forge types: no SQL or driver types.

use async_trait::async_trait;
use forge::core::Version;
use forge::tenancy::TenantContext;

use crate::domain::entities::event::{Event, EventId};

/// One page of events in identifier order (UUIDv7, so creation order).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EventPage {
    pub events: Vec<Event>,
    /// The cursor of the next page, when there is one.
    pub next: Option<EventId>,
}

/// Why storage could not complete an operation. Messages never contain SQL,
/// row values or credentials.
#[derive(Debug, thiserror::Error)]
pub enum EventStoreError {
    /// The stored version differs from the expected one, or the row is gone.
    #[error("the event was changed or deleted concurrently")]
    Conflict,
    /// A stored row does not satisfy the domain rules.
    #[error("stored event data is invalid")]
    Corrupt,
    /// The database failed; the source is for logs only.
    #[error("event storage is unavailable")]
    Unavailable(#[source] Box<dyn std::error::Error + Send + Sync>),
}

/// Persistence of [`Event`] values with optimistic locking.
///
/// Every operation takes the authorized [`TenantContext`] and only ever sees
/// rows of its tenant.
#[async_trait]
pub trait EventRepository: Send + Sync {
    /// Loads one event.
    async fn find(
        &self,
        context: &TenantContext,
        id: EventId,
    ) -> Result<Option<Event>, EventStoreError>;

    /// Reads at most `limit` events whose identifier follows `after`.
    async fn page(
        &self,
        context: &TenantContext,
        after: Option<EventId>,
        limit: u16,
    ) -> Result<EventPage, EventStoreError>;

    /// Stores a new event.
    async fn insert(&self, context: &TenantContext, event: &Event) -> Result<(), EventStoreError>;

    /// Replaces the stored event if its version is still
    /// `event.version()` and returns the new version; otherwise fails
    /// with [`EventStoreError::Conflict`].
    async fn update(
        &self,
        context: &TenantContext,
        event: &Event,
    ) -> Result<Version, EventStoreError>;

    /// Deletes the event if its version is still `expected`; otherwise
    /// fails with [`EventStoreError::Conflict`].
    async fn delete(
        &self,
        context: &TenantContext,
        id: EventId,
        expected: Version,
    ) -> Result<(), EventStoreError>;
}
