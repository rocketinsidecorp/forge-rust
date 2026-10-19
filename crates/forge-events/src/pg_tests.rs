//! PostgreSQL integration tests, run as the least-privilege runtime role.
//!
//! `#[ignore]`d unless requested: `FORGE_TEST_DATABASE_URL=postgres://...
//! cargo test -p forge-events -- --include-ignored`.

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use forge_auth::{Session, UnixTimestamp};
use forge_core::Cancellation;
use forge_db::PgDatabase;
use forge_jobs::{Backoff, testing::RlsTestDatabase};
use forge_security::{Permission, PrincipalId, RbacPolicy, Role};
use forge_tenancy::{Membership, TenantContext, TenantId};
use serde::{Deserialize, Serialize};
use sqlx::AssertSqlSafe;
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::{
    Dispatcher, DispatcherSettings, EventEnvelope, EventId, EventPublisher, EventSubscriber,
    EventsError, IntegrationEvent, PublishError, Subscribers,
    postgres::{Delivery, PgOutboxStore, append, append_for_tenant, migrator, record_delivery},
};

const IGNORE: &str = "needs FORGE_TEST_DATABASE_URL";

#[derive(Debug, Serialize, Deserialize, PartialEq)]
struct InvoicePaid {
    invoice: u32,
}

impl IntegrationEvent for InvoicePaid {
    const TYPE: &'static str = "billing.invoice_paid";
}

async fn fresh() -> RlsTestDatabase {
    let db = RlsTestDatabase::create(&[migrator()]).await.expect(IGNORE);
    // An application table written by the consumer under test.
    for statement in [
        "CREATE TABLE test_ledger (event_id uuid NOT NULL, invoice integer NOT NULL)".to_owned(),
        format!(
            "GRANT SELECT, INSERT ON test_ledger TO \"{}\"",
            db.runtime_role()
        ),
    ] {
        sqlx::query(AssertSqlSafe(statement))
            .execute(db.migrator().pool())
            .await
            .expect("ledger table");
    }
    db
}

fn fast() -> DispatcherSettings {
    DispatcherSettings::default()
        .poll_interval(Duration::from_millis(50))
        .publish_timeout(Duration::from_millis(500))
        .max_attempts(3)
        .backoff(Backoff::fixed(Duration::ZERO))
}

fn spawn(dispatcher: Dispatcher) -> (Cancellation, JoinHandle<Result<(), EventsError>>) {
    let shutdown = Cancellation::new();
    (shutdown.clone(), tokio::spawn(dispatcher.run(shutdown)))
}

async fn stop(shutdown: Cancellation, handle: JoinHandle<Result<(), EventsError>>) {
    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(30), handle)
        .await
        .expect("dispatcher stops")
        .expect("task")
        .expect("dispatcher result");
}

async fn wait_until<F, Fut>(what: &str, mut condition: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while !condition().await {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn state(db: &RlsTestDatabase, id: EventId) -> (String, i32) {
    sqlx::query_as("SELECT state, attempts FROM forge_outbox_events WHERE id = $1")
        .bind(id.into_uuid())
        .fetch_one(db.admin().pool())
        .await
        .expect("event row")
}

/// Applies each event to the ledger once, through the inbox.
struct Ledger {
    database: PgDatabase,
    handled: Arc<AtomicUsize>,
}

#[async_trait]
impl EventSubscriber for Ledger {
    async fn handle(&self, event: &EventEnvelope) -> Result<(), PublishError> {
        self.handled.fetch_add(1, Ordering::SeqCst);
        let paid: InvoicePaid = event.decode().map_err(PublishError::permanent)?;
        let mut transaction = self.database.begin().await?;
        if record_delivery(&mut transaction, "billing.ledger", event.id())
            .await
            .map_err(PublishError::retry)?
            == Delivery::First
        {
            sqlx::query("INSERT INTO test_ledger (event_id, invoice) VALUES ($1, $2)")
                .bind(event.id().into_uuid())
                .bind(i32::try_from(paid.invoice).expect("small"))
                .execute(transaction.connection())
                .await?;
            transaction.commit().await?;
        }
        Ok(())
    }
}

/// Fails the first delivery of every event, forcing a redelivery to all.
struct FailsOnce(Mutex<Vec<EventId>>);

#[async_trait]
impl EventSubscriber for FailsOnce {
    async fn handle(&self, event: &EventEnvelope) -> Result<(), PublishError> {
        let mut seen = self.0.lock().expect("lock");
        if seen.contains(&event.id()) {
            Ok(())
        } else {
            seen.push(event.id());
            Err(PublishError::retry("downstream unavailable"))
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs FORGE_TEST_DATABASE_URL"]
async fn committed_events_are_delivered_and_consumers_apply_them_once() {
    let db = fresh().await;
    let runtime = db.runtime();

    let mut rolled_back = runtime.begin().await.expect("begin");
    let lost = append(&mut rolled_back, &InvoicePaid { invoice: 1 })
        .await
        .expect("append");
    rolled_back.rollback().await.expect("rollback");

    let mut pending = runtime.begin().await.expect("begin");
    let kept = append(&mut pending, &InvoicePaid { invoice: 2 })
        .await
        .expect("append");
    // Nothing is published before commit.
    let store = PgOutboxStore::new(runtime.clone());
    let early = store
        .claim(10, Uuid::now_v7(), Duration::from_secs(5), 3)
        .await
        .expect("claim");
    assert!(early.is_empty());
    pending.commit().await.expect("commit");

    let handled = Arc::new(AtomicUsize::new(0));
    let mut subscribers = Subscribers::new();
    subscribers
        .subscribe::<InvoicePaid>(Ledger {
            database: runtime.clone(),
            handled: Arc::clone(&handled),
        })
        .expect("subscribe")
        .subscribe::<InvoicePaid>(FailsOnce(Mutex::new(Vec::new())))
        .expect("subscribe");
    let (shutdown, handle) = spawn(Dispatcher::new(store.clone(), subscribers).settings(fast()));
    wait_until("publication", || async {
        state(&db, kept).await.0 == "published"
    })
    .await;
    stop(shutdown, handle).await;

    assert_eq!(state(&db, kept).await, ("published".to_owned(), 2));
    assert_eq!(
        handled.load(Ordering::SeqCst),
        2,
        "redelivered to every subscriber"
    );
    let ledger: Vec<(Uuid, i32)> = sqlx::query_as("SELECT event_id, invoice FROM test_ledger")
        .fetch_all(db.admin().pool())
        .await
        .expect("ledger");
    assert_eq!(
        ledger,
        vec![(kept.into_uuid(), 2)],
        "effects applied exactly once"
    );
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM forge_outbox_events WHERE id = $1)")
            .bind(lost.into_uuid())
            .fetch_one(db.admin().pool())
            .await
            .expect("query");
    assert!(!exists, "rolled-back events are never published");
    db.cleanup().await.expect("cleanup");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs FORGE_TEST_DATABASE_URL"]
async fn concurrent_duplicate_deliveries_record_one_first_delivery() {
    let db = fresh().await;
    let event = EventId::new();
    let mut first = db.runtime().begin().await.expect("begin");
    assert_eq!(
        record_delivery(&mut first, "billing.ledger", event)
            .await
            .expect("first"),
        Delivery::First
    );
    // A concurrent duplicate blocks on the uncommitted row, then sees it.
    let runtime = db.runtime().clone();
    let second = tokio::spawn(async move {
        let mut transaction = runtime.begin().await.expect("begin");
        record_delivery(&mut transaction, "billing.ledger", event).await
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !second.is_finished(),
        "waits for the first delivery's transaction"
    );
    first.commit().await.expect("commit");
    assert_eq!(
        second.await.expect("task").expect("second"),
        Delivery::Duplicate
    );

    // Another consumer has its own inbox entry.
    let mut other = db.runtime().begin().await.expect("begin");
    assert_eq!(
        record_delivery(&mut other, "search.indexer", event)
            .await
            .expect("other"),
        Delivery::First
    );
    assert!(
        record_delivery(&mut other, "Bad Consumer", event)
            .await
            .is_err()
    );
    drop(other);
    db.cleanup().await.expect("cleanup");
}

struct Broken {
    panics: bool,
}

#[async_trait]
impl EventPublisher for Broken {
    async fn publish(&self, event: &EventEnvelope) -> Result<(), PublishError> {
        assert!(!self.panics, "publisher bug");
        if event
            .decode::<InvoicePaid>()
            .is_ok_and(|paid| paid.invoice == 0)
        {
            return Err(PublishError::permanent("rejected by the broker"));
        }
        Err(PublishError::retry("broker unavailable"))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs FORGE_TEST_DATABASE_URL"]
async fn failing_publications_retry_then_die_and_can_be_retried() {
    let db = fresh().await;
    let store = PgOutboxStore::new(db.runtime().clone());
    let mut transaction = db.runtime().begin().await.expect("begin");
    let permanent = append(&mut transaction, &InvoicePaid { invoice: 0 })
        .await
        .expect("append");
    let transient = append(&mut transaction, &InvoicePaid { invoice: 7 })
        .await
        .expect("append");
    transaction.commit().await.expect("commit");

    let (shutdown, handle) =
        spawn(Dispatcher::new(store.clone(), Broken { panics: false }).settings(fast()));
    wait_until("both events to die", || async {
        state(&db, permanent).await.0 == "dead" && state(&db, transient).await.0 == "dead"
    })
    .await;
    stop(shutdown, handle).await;
    assert_eq!(
        state(&db, permanent).await.1,
        1,
        "permanent failures are not retried"
    );
    assert_eq!(
        state(&db, transient).await.1,
        3,
        "transient failures use the attempt budget"
    );

    let dead = store.dead(10, None).await.expect("dead");
    assert_eq!(dead.len(), 2);
    assert_eq!(dead[0].id, transient, "newest first");
    assert_eq!(dead[0].last_error.as_deref(), Some("broker unavailable"));
    assert!(store.retry(transient).await.expect("retry"));
    assert_eq!(state(&db, transient).await, ("pending".to_owned(), 0));

    // A panicking publisher fails the attempt; the dispatcher survives.
    let (shutdown, handle) =
        spawn(Dispatcher::new(store.clone(), Broken { panics: true }).settings(fast()));
    wait_until("the panicking attempts", || async {
        state(&db, transient).await.0 == "dead"
    })
    .await;
    assert!(!handle.is_finished());
    stop(shutdown, handle).await;
    let error: Option<String> =
        sqlx::query_scalar("SELECT last_error FROM forge_outbox_events WHERE id = $1")
            .bind(transient.into_uuid())
            .fetch_one(db.admin().pool())
            .await
            .expect("error");
    assert_eq!(error.as_deref(), Some("the publisher panicked"));
    db.cleanup().await.expect("cleanup");
}

#[tokio::test]
#[ignore = "needs FORGE_TEST_DATABASE_URL"]
async fn stale_dispatchers_are_fenced_out() {
    let db = fresh().await;
    let store = PgOutboxStore::new(db.runtime().clone());
    let mut transaction = db.runtime().begin().await.expect("begin");
    let id = append(&mut transaction, &InvoicePaid { invoice: 3 })
        .await
        .expect("append");
    transaction.commit().await.expect("commit");

    let stale = Uuid::now_v7();
    let lease = Duration::from_secs(1);
    assert_eq!(
        store.claim(10, stale, lease, 5).await.expect("claim").len(),
        1
    );
    tokio::time::sleep(Duration::from_millis(1300)).await;
    let current = Uuid::now_v7();
    let reclaimed = store.claim(10, current, lease, 5).await.expect("reclaim");
    assert_eq!((reclaimed[0].id(), reclaimed[0].attempt()), (id, 2));
    assert!(!store.published(id, stale).await.expect("stale"));
    assert_eq!(
        store
            .fail(id, stale, "late", Duration::ZERO, 5, false)
            .await
            .expect("stale"),
        None
    );
    assert!(store.published(id, current).await.expect("current"));
    assert_eq!(
        store
            .prune(Duration::ZERO, Some(Duration::ZERO), 100)
            .await
            .expect("prune"),
        1
    );
    db.cleanup().await.expect("cleanup");
}

fn tenant_context() -> TenantContext {
    let principal = PrincipalId::new();
    let permission = Permission::new("billing:pay").expect("permission");
    let role = Role::new("member").expect("role");
    let membership = Membership::active(TenantId::new(), principal, vec![role.clone()]);
    let mut policy = RbacPolicy::new();
    policy.allow(role, permission.clone());
    let authenticated = Session::new(
        principal,
        UnixTimestamp::from_secs(1),
        UnixTimestamp::from_secs(9),
    )
    .expect("session")
    .authenticate(UnixTimestamp::from_secs(2))
    .expect("authenticated");
    TenantContext::authorize(authenticated, &membership, &policy, &permission).expect("context")
}

async fn begin_scoped(
    db: &PgDatabase,
    context: Option<&TenantContext>,
) -> forge_db::DbTransaction<sqlx::Postgres> {
    let mut transaction = db.begin().await.expect("begin");
    if let Some(context) = context {
        sqlx::query("SELECT set_config('forge.tenant_id', $1, true), set_config('forge.principal_id', $2, true)")
            .bind(context.tenant_id().to_string())
            .bind(context.principal_id().to_string())
            .execute(transaction.connection())
            .await
            .expect("scope");
    }
    transaction
}

#[tokio::test]
#[ignore = "needs FORGE_TEST_DATABASE_URL"]
async fn tenant_events_are_isolated_by_row_level_security() {
    let db = fresh().await;
    let runtime = db.runtime();
    let (alpha, beta) = (tenant_context(), tenant_context());

    let mut transaction = begin_scoped(runtime, Some(&alpha)).await;
    let alpha_event = append_for_tenant(&mut transaction, &alpha, &InvoicePaid { invoice: 1 })
        .await
        .expect("append");
    transaction.commit().await.expect("commit");
    for context in [None, Some(&beta)] {
        let mut transaction = begin_scoped(runtime, context).await;
        assert!(
            append_for_tenant(&mut transaction, &alpha, &InvoicePaid { invoice: 2 })
                .await
                .is_err()
        );
    }
    for (context, expected) in [
        (Some(&alpha), vec![alpha_event.into_uuid()]),
        (Some(&beta), vec![]),
        (None, vec![]),
    ] {
        let mut transaction = begin_scoped(runtime, context).await;
        let ids: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM forge_outbox_events")
            .fetch_all(transaction.connection())
            .await
            .expect("select");
        assert_eq!(ids, expected);
        transaction.rollback().await.expect("rollback");
    }
    // The dispatcher sees every tenant's committed events.
    let claimed = PgOutboxStore::new(runtime.clone())
        .claim(10, Uuid::now_v7(), Duration::from_secs(5), 3)
        .await
        .expect("claim");
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].tenant_id(), Some(alpha.tenant_id()));
    assert_eq!(
        claimed[0].decode::<InvoicePaid>().expect("decode"),
        InvoicePaid { invoice: 1 }
    );
    db.cleanup().await.expect("cleanup");
}
