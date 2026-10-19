//! Use cases of the `events` resource: list, show, create, update and
//! delete with optimistic locking.
//!
//! Events belong to a tenant. Every use case takes the [`TenantContext`]
//! that the HTTP adapter derived from an authenticated session, an active
//! membership and an RBAC grant, and checks that the grant is for this
//! resource: changes need [`WRITE_PERMISSION`], reads need
//! [`READ_PERMISSION`] or [`WRITE_PERMISSION`].

use std::sync::Arc;

use forge::core::Version;
use forge::tenancy::TenantContext;

use crate::application::ports::event_repository::{EventPage, EventRepository, EventStoreError};
use crate::domain::entities::event::{Event, EventAttributes, EventId, EventValidationErrors};

/// Permission to read events.
pub const READ_PERMISSION: &str = "events:read";
/// Permission to create, change and delete events.
pub const WRITE_PERMISSION: &str = "events:write";
/// Events per index page.
pub const PAGE_SIZE: u16 = 25;

/// Why a event use case failed.
#[derive(Debug, thiserror::Error)]
pub enum EventError {
    /// The attributes break business rules.
    #[error("{0}")]
    Invalid(EventValidationErrors),
    /// No such event.
    #[error("event not found")]
    NotFound,
    /// The event changed since the caller read it.
    #[error("the event was changed by someone else")]
    Conflict,
    /// The tenant context was authorized for another permission.
    #[error("permission denied")]
    Forbidden,
    /// Storage failed.
    #[error("event storage failed")]
    Store(#[source] EventStoreError),
}

/// The event use cases over a [`EventRepository`].
#[derive(Clone)]
pub struct EventService {
    repository: Arc<dyn EventRepository>,
}

impl EventService {
    #[must_use]
    pub fn new(repository: Arc<dyn EventRepository>) -> Self {
        Self { repository }
    }

    /// One page of events, starting after `after`.
    pub async fn list(
        &self,
        context: &TenantContext,
        after: Option<EventId>,
    ) -> Result<EventPage, EventError> {
        authorize(context, &[READ_PERMISSION, WRITE_PERMISSION])?;
        self.repository
            .page(context, after, PAGE_SIZE)
            .await
            .map_err(EventError::Store)
    }

    /// One event.
    pub async fn find(&self, context: &TenantContext, id: EventId) -> Result<Event, EventError> {
        authorize(context, &[READ_PERMISSION, WRITE_PERMISSION])?;
        self.repository
            .find(context, id)
            .await
            .map_err(EventError::Store)?
            .ok_or(EventError::NotFound)
    }

    /// Validates and stores a new event.
    pub async fn create(
        &self,
        context: &TenantContext,
        attributes: EventAttributes,
    ) -> Result<Event, EventError> {
        authorize(context, &[WRITE_PERMISSION])?;
        let event = Event::new(attributes).map_err(EventError::Invalid)?;
        self.repository
            .insert(context, &event)
            .await
            .map_err(EventError::Store)?;
        Ok(event)
    }

    /// Replaces the attributes of the event read at version `expected`.
    pub async fn update(
        &self,
        context: &TenantContext,
        id: EventId,
        expected: Version,
        attributes: EventAttributes,
    ) -> Result<Event, EventError> {
        authorize(context, &[WRITE_PERMISSION])?;
        let event = Event::restore(id, expected, attributes).map_err(EventError::Invalid)?;
        match self.repository.update(context, &event).await {
            Ok(version) => Ok(event.with_version(version)),
            Err(EventStoreError::Conflict) => Err(self.conflict_or_missing(context, id).await),
            Err(error) => Err(EventError::Store(error)),
        }
    }

    /// Deletes the event read at version `expected`.
    pub async fn delete(
        &self,
        context: &TenantContext,
        id: EventId,
        expected: Version,
    ) -> Result<(), EventError> {
        authorize(context, &[WRITE_PERMISSION])?;
        match self.repository.delete(context, id, expected).await {
            Ok(()) => Ok(()),
            Err(EventStoreError::Conflict) => Err(self.conflict_or_missing(context, id).await),
            Err(error) => Err(EventError::Store(error)),
        }
    }

    /// A version mismatch is a conflict while the event exists, and
    /// "not found" once it is gone.
    async fn conflict_or_missing(&self, context: &TenantContext, id: EventId) -> EventError {
        match self.repository.find(context, id).await {
            Ok(Some(_)) => EventError::Conflict,
            Ok(None) => EventError::NotFound,
            Err(error) => EventError::Store(error),
        }
    }
}

/// Defense in depth: the context must carry a grant for one of `allowed`.
fn authorize(context: &TenantContext, allowed: &[&str]) -> Result<(), EventError> {
    let granted = context.authorization().permission().as_str();
    if allowed.contains(&granted) {
        Ok(())
    } else {
        Err(EventError::Forbidden)
    }
}
