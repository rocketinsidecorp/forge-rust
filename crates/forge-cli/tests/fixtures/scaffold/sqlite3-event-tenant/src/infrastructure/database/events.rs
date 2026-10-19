//! SQLite storage of events, implementing
//! [`EventRepository`]. SQL and SQLx stay in this module: the port and the
//! use cases see only domain and Forge types.
//!
//! Tenant isolation: every statement binds the tenant of the authorized
//! [`TenantContext`] in its `WHERE` clause (or `INSERT` values).
//!
//! Optimistic locking: `UPDATE` and `DELETE` match the expected `version`;
//! zero affected rows is [`EventStoreError::Conflict`].

use async_trait::async_trait;
use forge::core::{Timestamp, Version};
use forge::db::{DbError, SqliteDatabase};
use forge::tenancy::TenantContext;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;
use sqlx::types::chrono::{DateTime, Utc};

use crate::application::ports::event_repository::{EventPage, EventRepository, EventStoreError};
use crate::domain::entities::event::{Event, EventAttributes, EventId};

/// One row by id.
const FIND: &str = r#"SELECT "id", "version", "name", "notes", "starts_at", "ends_at", "capacity",
        "attendees", "public", "featured", "venue", "organizer"
    FROM "events"
    WHERE "tenant_id" = ? AND "id" = ?"#;

/// The first page, in id order.
const PAGE_FIRST: &str = r#"SELECT "id", "version", "name", "notes", "starts_at", "ends_at", "capacity",
        "attendees", "public", "featured", "venue", "organizer"
    FROM "events"
    WHERE "tenant_id" = ?
    ORDER BY "id"
    LIMIT ?"#;

/// The page after a cursor, in id order.
const PAGE_AFTER: &str = r#"SELECT "id", "version", "name", "notes", "starts_at", "ends_at", "capacity",
        "attendees", "public", "featured", "venue", "organizer"
    FROM "events"
    WHERE "tenant_id" = ? AND "id" > ?
    ORDER BY "id"
    LIMIT ?"#;

/// A new row at the initial version.
const INSERT: &str = r#"INSERT INTO "events" (
        "id", "tenant_id", "version", "name", "notes", "starts_at", "ends_at",
        "capacity", "attendees", "public", "featured", "venue", "organizer"
    )
    VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#;

/// Replaces the attributes if the version still matches.
const UPDATE: &str = r#"UPDATE "events"
    SET "name" = ?,
        "notes" = ?,
        "starts_at" = ?,
        "ends_at" = ?,
        "capacity" = ?,
        "attendees" = ?,
        "public" = ?,
        "featured" = ?,
        "venue" = ?,
        "organizer" = ?,
        "version" = "version" + 1,
        "updated_at" = CURRENT_TIMESTAMP
    WHERE "tenant_id" = ? AND "id" = ? AND "version" = ?"#;

/// Deletes the row if the version still matches.
const DELETE: &str = r#"DELETE FROM "events"
    WHERE "tenant_id" = ? AND "id" = ? AND "version" = ?"#;

/// [`EventRepository`] over the application's SQLite pool.
#[derive(Clone, Debug)]
pub struct SqliteEventRepository {
    database: SqliteDatabase,
}

impl SqliteEventRepository {
    #[must_use]
    pub fn new(database: SqliteDatabase) -> Self {
        Self { database }
    }
}

#[async_trait]
impl EventRepository for SqliteEventRepository {
    async fn find(
        &self,
        context: &TenantContext,
        id: EventId,
    ) -> Result<Option<Event>, EventStoreError> {
        let row = sqlx::query(FIND)
            .bind(context.tenant_id())
            .bind(id)
            .fetch_optional(self.database.pool())
            .await
            .map_err(unavailable)?;
        row.as_ref().map(restore).transpose()
    }

    async fn page(
        &self,
        context: &TenantContext,
        after: Option<EventId>,
        limit: u16,
    ) -> Result<EventPage, EventStoreError> {
        // One extra row tells whether a next page exists.
        let fetch = i64::from(limit) + 1;
        let rows = match after {
            None => {
                sqlx::query(PAGE_FIRST)
                    .bind(context.tenant_id())
                    .bind(fetch)
                    .fetch_all(self.database.pool())
                    .await
            }
            Some(after) => {
                sqlx::query(PAGE_AFTER)
                    .bind(context.tenant_id())
                    .bind(after)
                    .bind(fetch)
                    .fetch_all(self.database.pool())
                    .await
            }
        }
        .map_err(unavailable)?;
        let mut events = rows.iter().map(restore).collect::<Result<Vec<_>, _>>()?;
        let next = if events.len() > usize::from(limit) {
            events.truncate(usize::from(limit));
            events.last().map(Event::id)
        } else {
            None
        };
        Ok(EventPage { events, next })
    }

    async fn insert(&self, context: &TenantContext, event: &Event) -> Result<(), EventStoreError> {
        let attributes = event.attributes();
        sqlx::query(INSERT)
            .bind(event.id())
            .bind(context.tenant_id())
            .bind(event.version())
            .bind(&attributes.name)
            .bind(&attributes.notes)
            .bind(to_datetime(attributes.starts_at)?)
            .bind(attributes.ends_at.map(to_datetime).transpose()?)
            .bind(attributes.capacity)
            .bind(attributes.attendees)
            .bind(attributes.public)
            .bind(attributes.featured)
            .bind(attributes.venue)
            .bind(attributes.organizer)
            .execute(self.database.pool())
            .await
            .map_err(unavailable)?;
        Ok(())
    }

    async fn update(
        &self,
        context: &TenantContext,
        event: &Event,
    ) -> Result<Version, EventStoreError> {
        let attributes = event.attributes();
        let next = event
            .version()
            .next()
            .map_err(|_| EventStoreError::Conflict)?;
        let result = sqlx::query(UPDATE)
            .bind(&attributes.name)
            .bind(&attributes.notes)
            .bind(to_datetime(attributes.starts_at)?)
            .bind(attributes.ends_at.map(to_datetime).transpose()?)
            .bind(attributes.capacity)
            .bind(attributes.attendees)
            .bind(attributes.public)
            .bind(attributes.featured)
            .bind(attributes.venue)
            .bind(attributes.organizer)
            .bind(context.tenant_id())
            .bind(event.id())
            .bind(event.version())
            .execute(self.database.pool())
            .await
            .map_err(unavailable)?;
        if result.rows_affected() == 1 {
            Ok(next)
        } else {
            Err(EventStoreError::Conflict)
        }
    }

    async fn delete(
        &self,
        context: &TenantContext,
        id: EventId,
        expected: Version,
    ) -> Result<(), EventStoreError> {
        let result = sqlx::query(DELETE)
            .bind(context.tenant_id())
            .bind(id)
            .bind(expected)
            .execute(self.database.pool())
            .await
            .map_err(unavailable)?;
        if result.rows_affected() == 1 {
            Ok(())
        } else {
            Err(EventStoreError::Conflict)
        }
    }
}

/// Rebuilds a event from a row; a value the domain rejects is `Corrupt`.
fn restore(row: &SqliteRow) -> Result<Event, EventStoreError> {
    let attributes = EventAttributes {
        name: row.try_get("name").map_err(corrupt)?,
        notes: row.try_get("notes").map_err(corrupt)?,
        starts_at: from_datetime(row.try_get("starts_at").map_err(corrupt)?)?,
        ends_at: row
            .try_get::<Option<DateTime<Utc>>, _>("ends_at")
            .map_err(corrupt)?
            .map(from_datetime)
            .transpose()?,
        capacity: row.try_get("capacity").map_err(corrupt)?,
        attendees: row.try_get("attendees").map_err(corrupt)?,
        public: row.try_get("public").map_err(corrupt)?,
        featured: row.try_get("featured").map_err(corrupt)?,
        venue: row.try_get("venue").map_err(corrupt)?,
        organizer: row.try_get("organizer").map_err(corrupt)?,
    };
    Event::restore(
        row.try_get("id").map_err(corrupt)?,
        row.try_get("version").map_err(corrupt)?,
        attributes,
    )
    .map_err(|_| EventStoreError::Corrupt)
}

fn to_datetime(value: Timestamp) -> Result<DateTime<Utc>, EventStoreError> {
    DateTime::from_timestamp_micros(value.unix_micros()).ok_or(EventStoreError::Corrupt)
}

fn from_datetime(value: DateTime<Utc>) -> Result<Timestamp, EventStoreError> {
    Timestamp::from_unix_micros(value.timestamp_micros()).map_err(|_| EventStoreError::Corrupt)
}

fn corrupt(_: sqlx::Error) -> EventStoreError {
    EventStoreError::Corrupt
}

/// Classifies the driver error (kept as the source for logs).
fn unavailable(error: sqlx::Error) -> EventStoreError {
    EventStoreError::Unavailable(Box::new(DbError::from(error)))
}
