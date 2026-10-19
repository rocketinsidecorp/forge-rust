//! PostgreSQL outbox and inbox.

use std::{
    borrow::Cow,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use forge_core::Id;
use forge_db::{DbTransaction, Migrator, PgDatabase};
use forge_tenancy::{TenantContext, TenantId};
use sqlx::{Postgres, Row, postgres::PgRow};
use uuid::Uuid;

use crate::{
    EventEnvelope, EventId, EventsError, IntegrationEvent, MAX_PAYLOAD_BYTES, validate_name,
};

/// Bookkeeping table of the framework events schema.
pub const MIGRATIONS_TABLE: &str = "_forge_events_migrations";

/// The embedded, reversible outbox/inbox schema. Apply it with the migration
/// role; grants go to the role named by `forge.runtime_role` (default
/// `app_runtime`), as for [`forge_jobs::postgres::migrator`].
#[must_use]
pub fn migrator() -> Migrator {
    use sqlx::{
        SqlSafeStr,
        migrate::{Migration, MigrationType},
    };

    let migration = |kind, sql: &'static str| {
        Migration::new(
            1,
            Cow::Borrowed("forge events"),
            kind,
            sql.into_sql_str(),
            false,
        )
    };
    let mut migrator = Migrator::with_migrations(vec![
        migration(
            MigrationType::ReversibleUp,
            include_str!("../migrations/postgres/1_forge_events.up.sql"),
        ),
        migration(
            MigrationType::ReversibleDown,
            include_str!("../migrations/postgres/1_forge_events.down.sql"),
        ),
    ]);
    migrator.table_name = Cow::Borrowed(MIGRATIONS_TABLE);
    migrator
}

/// Appends a global event to the outbox in `transaction`; it is published
/// only if the transaction commits.
pub async fn append<E: IntegrationEvent>(
    transaction: &mut DbTransaction<Postgres>,
    event: &E,
) -> Result<EventId, EventsError> {
    insert(transaction, event, None).await
}

/// Appends an event owned by the authorized tenant. The transaction must
/// carry the same tenant scope (ADR 0011), or row-level security rejects it.
pub async fn append_for_tenant<E: IntegrationEvent>(
    transaction: &mut DbTransaction<Postgres>,
    context: &TenantContext,
    event: &E,
) -> Result<EventId, EventsError> {
    insert(transaction, event, Some(context.tenant_id())).await
}

async fn insert<E: IntegrationEvent>(
    transaction: &mut DbTransaction<Postgres>,
    event: &E,
    tenant: Option<TenantId>,
) -> Result<EventId, EventsError> {
    validate_name("event type", E::TYPE)?;
    if E::VERSION == 0 {
        return Err(EventsError::InvalidVersion);
    }
    let payload = serde_json::to_string(event).map_err(EventsError::Serialize)?;
    if payload.len() > MAX_PAYLOAD_BYTES {
        return Err(EventsError::PayloadTooLarge {
            limit: MAX_PAYLOAD_BYTES,
        });
    }
    let id = EventId::new();
    sqlx::query(
        "INSERT INTO forge_outbox_events (id, tenant_id, event_type, version, payload, occurred_at)
         VALUES ($1, $2, $3, $4, $5::jsonb, timestamptz 'epoch' + $6::bigint * interval '1 microsecond')",
    )
    .bind(id.into_uuid())
    .bind(tenant.map(Id::into_uuid))
    .bind(E::TYPE)
    .bind(i32::from(E::VERSION))
    .bind(payload)
    .bind(micros_since_epoch(SystemTime::now()))
    .execute(transaction.connection())
    .await?;
    Ok(id)
}

/// Whether a consumer sees an event for the first time.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Delivery {
    /// First delivery: apply the effects in this transaction and commit.
    First,
    /// Already processed by this consumer: skip (and roll back or commit
    /// nothing).
    Duplicate,
}

/// Records that `consumer` processes `event_id` in `transaction`.
///
/// Call it first in the transaction that applies the event's effects: the
/// inbox row and the effects commit together, so a redelivered event yields
/// [`Delivery::Duplicate`] and is skipped. A concurrent duplicate delivery
/// waits on the row and then sees `Duplicate`.
///
/// ```no_run
/// # async fn apply(db: &forge_db::PgDatabase, event: &forge_events::EventEnvelope) -> Result<(), forge_events::EventsError> {
/// use forge_events::postgres::{Delivery, record_delivery};
///
/// let mut transaction = db.begin().await?;
/// if record_delivery(&mut transaction, "billing.ledger", event.id()).await? == Delivery::First {
///     // ... write the effects with `transaction` ...
///     transaction.commit().await?;
/// }
/// # Ok(())
/// # }
/// ```
pub async fn record_delivery(
    transaction: &mut DbTransaction<Postgres>,
    consumer: &str,
    event_id: EventId,
) -> Result<Delivery, EventsError> {
    validate_name("consumer", consumer)?;
    let inserted = sqlx::query(
        "INSERT INTO forge_inbox (consumer, event_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
    )
    .bind(consumer)
    .bind(event_id.into_uuid())
    .execute(transaction.connection())
    .await?
    .rows_affected();
    Ok(if inserted == 1 {
        Delivery::First
    } else {
        Delivery::Duplicate
    })
}

/// State after a failed publication was recorded.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AfterFailure {
    Retrying,
    Dead,
}

/// A dead event as shown to operators (no payload).
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct DeadEvent {
    /// The event.
    pub id: EventId,
    /// The owning tenant, if any.
    pub tenant_id: Option<TenantId>,
    /// Its type.
    pub event_type: String,
    /// Its schema version.
    pub version: u16,
    /// Publication attempts made.
    pub attempts: u32,
    /// The last recorded error.
    pub last_error: Option<String>,
}

/// Operator and dispatcher access to the outbox, over the runtime pool.
#[derive(Clone, Debug)]
pub struct PgOutboxStore {
    database: PgDatabase,
}

impl PgOutboxStore {
    /// Creates a store over the application's runtime pool.
    #[must_use]
    pub const fn new(database: PgDatabase) -> Self {
        Self { database }
    }

    /// Dead events, newest first, at most `limit` (1 to 500) older than
    /// `before`.
    pub async fn dead(
        &self,
        limit: u16,
        before: Option<EventId>,
    ) -> Result<Vec<DeadEvent>, EventsError> {
        let rows = sqlx::query(
            "SELECT id, tenant_id, event_type, version, attempts, last_error
             FROM forge_outbox_dead($1, $2)",
        )
        .bind(i32::from(limit.clamp(1, 500)))
        .bind(before.map(Id::into_uuid))
        .fetch_all(self.database.pool())
        .await?;
        rows.iter()
            .map(|row| {
                Ok(DeadEvent {
                    id: event_id(row.try_get("id")?)?,
                    tenant_id: tenant_id(row.try_get("tenant_id")?)?,
                    event_type: row.try_get("event_type")?,
                    version: u16::try_from(row.try_get::<i32, _>("version")?)
                        .map_err(|_| EventsError::Corrupt)?,
                    attempts: u32::try_from(row.try_get::<i32, _>("attempts")?)
                        .map_err(|_| EventsError::Corrupt)?,
                    last_error: row.try_get("last_error")?,
                })
            })
            .collect()
    }

    /// Requeues a dead event with a fresh attempt budget.
    pub async fn retry(&self, id: EventId) -> Result<bool, EventsError> {
        Ok(sqlx::query_scalar("SELECT forge_outbox_retry($1)")
            .bind(id.into_uuid())
            .fetch_one(self.database.pool())
            .await?)
    }

    /// Deletes up to `limit` published events older than `published_after`
    /// and, with `inbox_after`, inbox entries older than it. Returns the rows
    /// deleted.
    pub async fn prune(
        &self,
        published_after: Duration,
        inbox_after: Option<Duration>,
        limit: u32,
    ) -> Result<u64, EventsError> {
        let deleted: i64 = sqlx::query_scalar("SELECT forge_outbox_prune($1, $2, $3)")
            .bind(millis(published_after))
            .bind(inbox_after.map(millis))
            .bind(i32::try_from(limit.clamp(1, 10_000)).unwrap_or(1))
            .fetch_one(self.database.pool())
            .await?;
        Ok(u64::try_from(deleted).unwrap_or(0))
    }

    pub(crate) async fn claim(
        &self,
        limit: u16,
        token: Uuid,
        lease: Duration,
        max_attempts: u16,
    ) -> Result<Vec<EventEnvelope>, EventsError> {
        let rows = sqlx::query(
            "SELECT id, tenant_id, event_type, version, payload, occurred_at_us, attempts
             FROM forge_outbox_claim($1, $2, $3, $4)",
        )
        .bind(i32::from(limit.clamp(1, 1000)))
        .bind(token)
        .bind(millis(lease))
        .bind(i32::from(max_attempts))
        .fetch_all(self.database.pool())
        .await?;
        rows.iter().map(envelope).collect()
    }

    pub(crate) async fn published(&self, id: EventId, token: Uuid) -> Result<bool, EventsError> {
        Ok(sqlx::query_scalar("SELECT forge_outbox_published($1, $2)")
            .bind(id.into_uuid())
            .bind(token)
            .fetch_one(self.database.pool())
            .await?)
    }

    pub(crate) async fn fail(
        &self,
        id: EventId,
        token: Uuid,
        error: &str,
        retry_in: Duration,
        max_attempts: u16,
        permanent: bool,
    ) -> Result<Option<AfterFailure>, EventsError> {
        let state: Option<String> =
            sqlx::query_scalar("SELECT forge_outbox_fail($1, $2, $3, $4, $5, $6)")
                .bind(id.into_uuid())
                .bind(token)
                .bind(error)
                .bind(millis(retry_in))
                .bind(i32::from(max_attempts))
                .bind(permanent)
                .fetch_one(self.database.pool())
                .await?;
        match state.as_deref() {
            None => Ok(None),
            Some("pending") => Ok(Some(AfterFailure::Retrying)),
            Some("dead") => Ok(Some(AfterFailure::Dead)),
            Some(_) => Err(EventsError::Corrupt),
        }
    }

    pub(crate) async fn release(&self, id: EventId, token: Uuid) -> Result<bool, EventsError> {
        Ok(sqlx::query_scalar("SELECT forge_outbox_release($1, $2)")
            .bind(id.into_uuid())
            .bind(token)
            .fetch_one(self.database.pool())
            .await?)
    }
}

fn envelope(row: &PgRow) -> Result<EventEnvelope, EventsError> {
    Ok(EventEnvelope {
        id: event_id(row.try_get("id")?)?,
        event_type: row.try_get("event_type")?,
        version: u16::try_from(row.try_get::<i32, _>("version")?)
            .map_err(|_| EventsError::Corrupt)?,
        tenant_id: tenant_id(row.try_get("tenant_id")?)?,
        occurred_at: UNIX_EPOCH
            + Duration::from_micros(
                u64::try_from(row.try_get::<i64, _>("occurred_at_us")?).unwrap_or(0),
            ),
        attempt: u32::try_from(row.try_get::<i32, _>("attempts")?)
            .map_err(|_| EventsError::Corrupt)?,
        payload: row.try_get("payload")?,
    })
}

fn event_id(value: Uuid) -> Result<EventId, EventsError> {
    Id::from_uuid(value).map_err(|_| EventsError::Corrupt)
}

fn tenant_id(value: Option<Uuid>) -> Result<Option<TenantId>, EventsError> {
    value
        .map(|tenant| Id::from_uuid(tenant).map_err(|_| EventsError::Corrupt))
        .transpose()
}

fn millis(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

fn micros_since_epoch(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_micros()).ok())
        .unwrap_or(0)
}
