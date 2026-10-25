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

#[derive(Serialize, Deserialize)]
struct Wide(Vec<u8>);

impl IntegrationEvent for Wide {
    const TYPE: &'static str = "test.wide";
}

#[tokio::test]
#[ignore = "needs FORGE_TEST_DATABASE_URL"]
async fn the_database_enforces_append_columns_and_payload_size() {
    let db = fresh().await;
    let forged = sqlx::query(
        "INSERT INTO forge_outbox_events (id, event_type, version, payload, occurred_at, state, published_at)
         VALUES ($1, 'test.wide', 1, '{}', now(), 'published', now())",
    )
    .bind(EventId::new().into_uuid())
    .execute(db.runtime().pool())
    .await;
    assert!(
        forged.is_err(),
        "delivery state is not insertable: {forged:?}"
    );
    let mut transaction = db.runtime().begin().await.expect("begin");
    let wide = append(&mut transaction, &Wide(vec![1; 120_000])).await;
    assert!(
        matches!(wide, Err(EventsError::PayloadTooLarge { .. })),
        "{wide:?}"
    );
    drop(transaction);
    db.cleanup().await.expect("cleanup");
}

#[tokio::test]
#[ignore = "needs FORGE_TEST_DATABASE_URL"]
async fn dispatcher_functions_refuse_tenant_scope_and_can_be_split_from_the_runtime_role() {
    let db = RlsTestDatabase::create_with_worker(&[migrator()])
        .await
        .expect(IGNORE);
    let mut transaction = db.runtime().begin().await.expect("begin");
    append(&mut transaction, &InvoicePaid { invoice: 1 })
        .await
        .expect("append");
    transaction.commit().await.expect("commit");
    let lease = Duration::from_secs(5);
    assert!(
        PgOutboxStore::new(db.runtime().clone())
            .claim(10, Uuid::now_v7(), lease, 3)
            .await
            .is_err(),
        "the request-serving role cannot dispatch"
    );
    let alpha = tenant_context();
    let mut scoped = begin_scoped(db.worker(), Some(&alpha)).await;
    let refused = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM forge_outbox_dead(10, NULL)")
        .fetch_one(scoped.connection())
        .await;
    assert!(refused.is_err(), "refused in tenant scope");
    drop(scoped);
    let claimed = PgOutboxStore::new(db.worker().clone())
        .claim(10, Uuid::now_v7(), lease, 3)
        .await
        .expect("worker claims");
    assert_eq!(claimed.len(), 1);
    db.cleanup().await.expect("cleanup");
}

/// Accepts every event except invoice 0, which it refuses for good.
struct Picky;

#[async_trait]
impl EventPublisher for Picky {
    async fn publish(&self, event: &EventEnvelope) -> Result<(), PublishError> {
        if event
            .decode::<InvoicePaid>()
            .is_ok_and(|paid| paid.invoice == 0)
        {
            return Err(PublishError::permanent("rejected by the broker"));
        }
        Ok(())
    }
}

#[tokio::test]
#[ignore = "needs FORGE_TEST_DATABASE_URL"]
async fn deliveries_are_traced_and_measured_by_outcome() {
    // A subscriber that records spans, so the delivery's log span carries
    // the trace and span ids of its telemetry span.
    let _subscriber = tracing::subscriber::set_default(Warnings(Arc::new(AtomicUsize::new(0))));
    use forge_telemetry::{
        Options, SpanSink, Telemetry,
        testing::{Behavior, Receiver},
    };

    let receiver = Receiver::start(Behavior::Accept).await;
    let mut options = Options::new("worker");
    options.spans = Some(SpanSink::Otlp(receiver.client(
        http::HeaderMap::new(),
        false,
        Duration::from_secs(10),
    )));
    let telemetry = Telemetry::new(options).expect("telemetry");
    telemetry.start();

    let db = fresh().await;
    let store = PgOutboxStore::new(db.runtime().clone());
    let mut transaction = db.runtime().begin().await.expect("begin");
    let delivered = append(&mut transaction, &InvoicePaid { invoice: 7 })
        .await
        .expect("append");
    let refused = append(&mut transaction, &InvoicePaid { invoice: 0 })
        .await
        .expect("append");
    transaction.commit().await.expect("commit");

    let dispatcher = Dispatcher::new(store.clone(), Picky)
        .settings(fast())
        .instrument(telemetry.outbox().expect("instruments"));
    let (shutdown, handle) = spawn(dispatcher);
    wait_until("both events to settle", || async {
        state(&db, delivered).await.0 == "published" && state(&db, refused).await.0 == "dead"
    })
    .await;
    stop(shutdown, handle).await;
    telemetry.force_flush(Duration::from_secs(10)).await;

    let text = forge_telemetry::prometheus::render(telemetry.metrics());
    for outcome in ["delivered", "dead_lettered"] {
        let line = format!(
            "forge_outbox_dispatch_duration_seconds_count{{forge_event_type=\"billing.invoice_paid\",forge_outbox_outcome=\"{outcome}\"}} 1"
        );
        assert!(text.contains(&line), "{line}\n{text}");
    }
    let spans = receiver
        .received("/v1/traces")
        .iter()
        .map(|request| request.body.to_string())
        .collect::<String>();
    assert!(spans.contains("\"process outbox\""), "{spans}");
    assert!(spans.contains(&delivered.to_string()), "{spans}");
    assert!(!spans.contains("rejected by the broker"), "{spans}");
    db.cleanup().await.expect("cleanup");
}

#[tokio::test]
#[ignore = "needs FORGE_TEST_DATABASE_URL"]
async fn a_dispatcher_with_invalid_settings_refuses_to_run() {
    let db = fresh().await;
    let defaults = DispatcherSettings::default();
    for (settings, said) in [
        (defaults.batch(0), "batch"),
        (defaults.batch(1001), "batch"),
        (
            defaults.poll_interval(Duration::from_millis(1)),
            "poll_interval",
        ),
        (
            defaults.publish_timeout(Duration::from_millis(1)),
            "publish_timeout",
        ),
        (
            defaults.publish_timeout(Duration::from_secs(601)),
            "publish_timeout",
        ),
        (defaults.max_attempts(0), "max_attempts"),
    ] {
        let dispatcher =
            Dispatcher::new(PgOutboxStore::new(db.runtime().clone()), Picky).settings(settings);
        let error = dispatcher
            .run(Cancellation::new())
            .await
            .expect_err("invalid settings must not start");
        assert!(
            matches!(error, EventsError::InvalidSetting(text) if text.contains(said)),
            "{said}"
        );
    }
    assert!(
        format!(
            "{:?}",
            Dispatcher::new(PgOutboxStore::new(db.runtime().clone()), Picky)
        )
        .starts_with("Dispatcher { settings: DispatcherSettings")
    );
    db.cleanup().await.expect("cleanup");
}

/// Counts the warnings of the current thread.
struct Warnings(Arc<AtomicUsize>);

impl tracing::Subscriber for Warnings {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        if *event.metadata().level() == tracing::Level::WARN {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn enter(&self, _: &tracing::span::Id) {}

    fn exit(&self, _: &tracing::span::Id) {}
}

#[tokio::test]
#[ignore = "needs FORGE_TEST_DATABASE_URL"]
async fn a_failing_claim_is_logged_and_retried_until_shutdown() {
    let db = fresh().await;
    let store = PgOutboxStore::new(db.runtime().clone());
    // Every claim now fails: the pool is closed.
    db.runtime().close().await;
    let warnings = Arc::new(AtomicUsize::new(0));
    let _guard = tracing::subscriber::set_default(Warnings(Arc::clone(&warnings)));

    let (shutdown, handle) = spawn(Dispatcher::new(store, Picky).settings(fast()));
    tokio::time::timeout(Duration::from_secs(30), async {
        while warnings.load(Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the failed claims are logged and retried");
    stop(shutdown, handle).await;
    db.cleanup().await.expect("cleanup");
}

/// Never answers.
struct Silent;

#[async_trait]
impl EventPublisher for Silent {
    async fn publish(&self, _: &EventEnvelope) -> Result<(), PublishError> {
        std::future::pending().await
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs FORGE_TEST_DATABASE_URL"]
async fn a_publication_that_exceeds_its_timeout_fails_the_attempt() {
    let db = fresh().await;
    let store = PgOutboxStore::new(db.runtime().clone());
    let mut transaction = db.runtime().begin().await.expect("begin");
    let id = append(&mut transaction, &InvoicePaid { invoice: 1 })
        .await
        .expect("append");
    transaction.commit().await.expect("commit");

    let settings = fast().publish_timeout(Duration::from_millis(20));
    let (shutdown, handle) = spawn(Dispatcher::new(store, Silent).settings(settings));
    wait_until("the event to die", || async {
        state(&db, id).await.0 == "dead"
    })
    .await;
    stop(shutdown, handle).await;

    assert_eq!(state(&db, id).await.1, 3, "every attempt timed out");
    let error: Option<String> =
        sqlx::query_scalar("SELECT last_error FROM forge_outbox_events WHERE id = $1")
            .bind(id.into_uuid())
            .fetch_one(db.admin().pool())
            .await
            .expect("error");
    assert_eq!(error.as_deref(), Some("publication exceeded its timeout"));
    db.cleanup().await.expect("cleanup");
}

/// Delivers the first event and then begins shutdown.
struct StopsAfterOne(Cancellation);

#[async_trait]
impl EventPublisher for StopsAfterOne {
    async fn publish(&self, _: &EventEnvelope) -> Result<(), PublishError> {
        self.0.cancel();
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs FORGE_TEST_DATABASE_URL"]
async fn shutdown_releases_the_rest_of_a_claimed_batch_without_using_attempts() {
    let db = fresh().await;
    let store = PgOutboxStore::new(db.runtime().clone());
    let mut transaction = db.runtime().begin().await.expect("begin");
    let mut ids = Vec::new();
    for invoice in 1..=3 {
        ids.push(
            append(&mut transaction, &InvoicePaid { invoice })
                .await
                .expect("append"),
        );
    }
    transaction.commit().await.expect("commit");

    let shutdown = Cancellation::new();
    let dispatcher =
        Dispatcher::new(store, StopsAfterOne(shutdown.clone())).settings(fast().batch(3));
    tokio::time::timeout(Duration::from_secs(30), dispatcher.run(shutdown))
        .await
        .expect("the dispatcher stops")
        .expect("clean shutdown");

    // Ids are time-ordered: the first claimed event was delivered, the other
    // two went back to the outbox as if never claimed.
    assert_eq!(state(&db, ids[0]).await, ("published".to_owned(), 1));
    for id in &ids[1..] {
        assert_eq!(state(&db, *id).await, ("pending".to_owned(), 0));
    }
    db.cleanup().await.expect("cleanup");
}

/// Cannot be written as JSON.
struct Unwritable;

impl Serialize for Unwritable {
    fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
        Err(serde::ser::Error::custom("cannot be written"))
    }
}

impl<'de> Deserialize<'de> for Unwritable {
    fn deserialize<D: serde::Deserializer<'de>>(_: D) -> Result<Self, D::Error> {
        Ok(Self)
    }
}

impl IntegrationEvent for Unwritable {
    const TYPE: &'static str = "test.unwritable";
}

#[derive(Serialize, Deserialize)]
struct Unversioned;

impl IntegrationEvent for Unversioned {
    const TYPE: &'static str = "test.unversioned";
    const VERSION: u16 = 0;
}

#[derive(Serialize, Deserialize)]
struct Misnamed;

impl IntegrationEvent for Misnamed {
    const TYPE: &'static str = "Not A Name";
}

#[tokio::test]
#[ignore = "needs FORGE_TEST_DATABASE_URL"]
async fn an_event_that_breaks_the_contract_is_refused_before_anything_is_written() {
    let db = fresh().await;
    let mut transaction = db.runtime().begin().await.expect("begin");

    assert!(matches!(
        append(&mut transaction, &Misnamed).await,
        Err(EventsError::InvalidName {
            kind: "event type",
            ..
        })
    ));
    assert!(matches!(
        append(&mut transaction, &Unversioned).await,
        Err(EventsError::InvalidVersion)
    ));
    assert!(matches!(
        append(&mut transaction, &Unwritable).await,
        Err(EventsError::Serialize(_))
    ));
    // Over the limit in the application, before the database sees it.
    let wide = Wide(vec![1; 300_000]);
    assert!(matches!(
        append(&mut transaction, &wide).await,
        Err(EventsError::PayloadTooLarge { limit }) if limit == crate::MAX_PAYLOAD_BYTES
    ));
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM forge_outbox_events")
        .fetch_one(transaction.connection())
        .await
        .expect("count");
    assert_eq!(rows, 0);
    transaction.rollback().await.expect("rollback");

    // A consumer name follows the same grammar as an event type.
    let mut transaction = db.runtime().begin().await.expect("begin");
    for consumer in ["", "Billing Ledger", &"c".repeat(crate::MAX_NAME + 1)] {
        assert!(
            matches!(
                record_delivery(&mut transaction, consumer, EventId::new()).await,
                Err(EventsError::InvalidName {
                    kind: "consumer",
                    ..
                })
            ),
            "{consumer:?}"
        );
    }
    transaction.rollback().await.expect("rollback");
    db.cleanup().await.expect("cleanup");
}

#[tokio::test]
#[ignore = "needs FORGE_TEST_DATABASE_URL"]
async fn dead_events_are_listed_newest_first_a_page_at_a_time() {
    let db = fresh().await;
    let store = PgOutboxStore::new(db.runtime().clone());
    let (older, newer) = (EventId::new(), EventId::new());
    for id in [older, newer] {
        sqlx::query(
            "INSERT INTO forge_outbox_events
                (id, event_type, version, payload, occurred_at, state, attempts, last_error)
             VALUES ($1, 'test.dead', 1, '{}', now(), 'dead', 3, 'gave up')",
        )
        .bind(id.into_uuid())
        .execute(db.admin().pool())
        .await
        .expect("a dead event");
    }

    let first_page = store.dead(1, None).await.expect("first page");
    assert_eq!(
        first_page.iter().map(|event| event.id).collect::<Vec<_>>(),
        [newer]
    );
    assert_eq!(first_page[0].event_type, "test.dead");
    assert_eq!((first_page[0].version, first_page[0].attempts), (1, 3));
    assert_eq!(first_page[0].last_error.as_deref(), Some("gave up"));
    assert_eq!(first_page[0].tenant_id, None);
    let second_page = store.dead(10, Some(newer)).await.expect("second page");
    assert_eq!(
        second_page.iter().map(|event| event.id).collect::<Vec<_>>(),
        [older]
    );
    assert!(
        store
            .dead(10, Some(older))
            .await
            .expect("last page")
            .is_empty()
    );
    assert_eq!(
        store
            .dead(0, None)
            .await
            .expect("limit is at least one")
            .len(),
        1
    );
    assert!(
        !store.retry(EventId::new()).await.expect("unknown id"),
        "only an existing dead event is requeued"
    );
    db.cleanup().await.expect("cleanup");
}

/// Takes the lease away while it publishes, as another dispatcher would
/// after the lease expired, then answers `outcome`.
struct LeaseTaken {
    admin: PgDatabase,
    outcome: fn() -> Result<(), PublishError>,
    published: Arc<AtomicUsize>,
}

#[async_trait]
impl EventPublisher for LeaseTaken {
    async fn publish(&self, event: &EventEnvelope) -> Result<(), PublishError> {
        sqlx::query("UPDATE forge_outbox_events SET lease_token = $1 WHERE id = $2")
            .bind(Uuid::now_v7())
            .bind(event.id().into_uuid())
            .execute(self.admin.pool())
            .await?;
        let outcome = (self.outcome)();
        self.published.fetch_add(1, Ordering::SeqCst);
        outcome
    }
}

#[tokio::test]
#[ignore = "needs FORGE_TEST_DATABASE_URL"]
async fn a_publication_whose_lease_was_lost_changes_nothing_either_way() {
    for outcome in [(|| Ok(())) as fn() -> Result<(), PublishError>, || {
        Err(PublishError::retry("broker unavailable"))
    }] {
        let db = fresh().await;
        let store = PgOutboxStore::new(db.runtime().clone());
        let mut transaction = db.runtime().begin().await.expect("begin");
        let id = append(&mut transaction, &InvoicePaid { invoice: 1 })
            .await
            .expect("append");
        transaction.commit().await.expect("commit");
        let published = Arc::new(AtomicUsize::new(0));
        let publisher = LeaseTaken {
            admin: db.admin().clone(),
            outcome,
            published: Arc::clone(&published),
        };
        let warnings = Arc::new(AtomicUsize::new(0));
        let _guard = tracing::subscriber::set_default(Warnings(Arc::clone(&warnings)));

        let (shutdown, handle) = spawn(Dispatcher::new(store, publisher).settings(fast()));
        tokio::time::timeout(Duration::from_secs(30), async {
            while published.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the event is published");
        // Stopping waits for the delivery in progress to be recorded.
        stop(shutdown, handle).await;

        assert_eq!(
            warnings.load(Ordering::SeqCst),
            1,
            "the lost lease is reported once"
        );
        assert_eq!(
            state(&db, id).await,
            ("publishing".to_owned(), 1),
            "neither a publication nor a failure is recorded for a lease that is not ours"
        );
        db.cleanup().await.expect("cleanup");
    }
}

/// Closes the pool the dispatcher records with, so recording fails, and
/// optionally begins shutdown.
struct PoolClosed {
    runtime: PgDatabase,
    shutdown: Option<Cancellation>,
    outcome: fn() -> Result<(), PublishError>,
}

#[async_trait]
impl EventPublisher for PoolClosed {
    async fn publish(&self, _: &EventEnvelope) -> Result<(), PublishError> {
        self.runtime.close().await;
        if let Some(shutdown) = &self.shutdown {
            shutdown.cancel();
        }
        (self.outcome)()
    }
}

#[tokio::test]
#[ignore = "needs FORGE_TEST_DATABASE_URL"]
async fn a_dispatcher_that_cannot_record_a_result_logs_it_and_stops_cleanly() {
    // (outcome, whether the batch is cut short by a shutdown, warnings expected at least)
    type Case = (fn() -> Result<(), PublishError>, bool, usize);
    let cases: [Case; 3] = [
        (|| Ok(()), false, 1),
        (|| Err(PublishError::retry("broker unavailable")), false, 1),
        // Three events claimed, the first delivered and not recorded, the
        // others could not be released: each failure is its own warning.
        (|| Ok(()), true, 3),
    ];
    for (outcome, cut_short, at_least) in cases {
        let db = fresh().await;
        let store = PgOutboxStore::new(db.runtime().clone());
        let mut transaction = db.runtime().begin().await.expect("begin");
        for invoice in 1..=3 {
            append(&mut transaction, &InvoicePaid { invoice })
                .await
                .expect("append");
        }
        transaction.commit().await.expect("commit");
        let shutdown = Cancellation::new();
        let publisher = PoolClosed {
            runtime: db.runtime().clone(),
            shutdown: cut_short.then(|| shutdown.clone()),
            outcome,
        };
        let warnings = Arc::new(AtomicUsize::new(0));
        let _guard = tracing::subscriber::set_default(Warnings(Arc::clone(&warnings)));
        let dispatcher = Dispatcher::new(store, publisher).settings(fast().batch(3));

        if cut_short {
            tokio::time::timeout(Duration::from_secs(30), dispatcher.run(shutdown))
                .await
                .expect("the dispatcher stops")
                .expect("clean shutdown");
        } else {
            let handle = tokio::spawn(dispatcher.run(shutdown.clone()));
            tokio::time::timeout(Duration::from_secs(30), async {
                while warnings.load(Ordering::SeqCst) < at_least {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("the failure to record is logged");
            stop(shutdown, handle).await;
        }
        assert!(warnings.load(Ordering::SeqCst) >= at_least);
        db.cleanup().await.expect("cleanup");
    }
}
