//! `LISTEN/NOTIFY` fanout between two brokers ("replicas") on one
//! PostgreSQL database. Run with `--include-ignored` and
//! `FORGE_TEST_DATABASE_URL` (each test gets a fresh database, so
//! notifications never cross tests).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use forge_broadcast::postgres::{PgBroker, PgOptions};
use forge_broadcast::{
    BroadcastError, Broker, Channel, MemoryBroker, MemoryOptions, Message, Subscription,
};
use forge_core::Cancellation;
use forge_db::sqlx;
use forge_db::testing::TestDatabase;
use forge_tenancy::TenantId;
use tokio::task::JoinHandle;

const WAIT: Duration = Duration::from_secs(10);

/// One replica: its broker and its running listener.
struct Replica {
    broker: PgBroker,
    shutdown: Cancellation,
    listener: JoinHandle<()>,
}

impl Replica {
    async fn start(test: &TestDatabase<sqlx::Postgres>) -> Self {
        let shutdown = Cancellation::new();
        let local = MemoryBroker::new(MemoryOptions::default(), &shutdown).expect("local broker");
        let broker =
            PgBroker::new(test.database().clone(), local, PgOptions::default()).expect("broker");
        let listening = broker.clone();
        let listener = tokio::spawn(async move { listening.listen().await });
        let replica = Self {
            broker,
            shutdown,
            listener,
        };
        replica.until_listening().await;
        replica
    }

    async fn until_listening(&self) {
        tokio::time::timeout(WAIT, async {
            while !self.broker.listening() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the listener must connect");
    }

    async fn stop(self) {
        self.shutdown.cancel();
        tokio::time::timeout(WAIT, self.listener)
            .await
            .expect("the listener stops when shutdown begins")
            .expect("no panic");
    }
}

async fn next(subscription: &mut Subscription) -> Message {
    tokio::time::timeout(WAIT, subscription.next())
        .await
        .expect("a message must arrive")
        .expect("the subscription is live")
}

async fn silent(subscription: &mut Subscription) {
    assert!(
        tokio::time::timeout(Duration::from_millis(300), subscription.next())
            .await
            .is_err(),
        "no further message may arrive"
    );
}

async fn fresh() -> TestDatabase<sqlx::Postgres> {
    TestDatabase::<sqlx::Postgres>::create_empty()
        .await
        .expect("FORGE_TEST_DATABASE_URL must name a PostgreSQL server")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs FORGE_TEST_DATABASE_URL"]
async fn messages_reach_every_replica_exactly_once() {
    let test = fresh().await;
    let one = Replica::start(&test).await;
    let two = Replica::start(&test).await;
    let channel = Channel::for_tenant(TenantId::new(), "posts").expect("valid");
    let elsewhere = Channel::for_tenant(TenantId::new(), "posts").expect("valid");
    let mut on_one = one.broker.subscribe(&channel).expect("subscribes");
    let mut on_two = two.broker.subscribe(&channel).expect("subscribes");
    let mut other_tenant = two.broker.subscribe(&elsewhere).expect("subscribes");

    one.broker
        .publish(&channel, Message::new("hello"))
        .await
        .expect("publishes");
    // A message several notifications long is reassembled.
    let large: String = "<p>0123456789</p>".repeat(1_500);
    two.broker
        .publish(&channel, Message::new(large.clone()))
        .await
        .expect("publishes");

    assert_eq!(next(&mut on_one).await.as_str(), "hello");
    assert_eq!(next(&mut on_two).await.as_str(), "hello");
    assert_eq!(next(&mut on_one).await.as_str(), large);
    assert_eq!(next(&mut on_two).await.as_str(), large);
    silent(&mut on_one).await;
    silent(&mut on_two).await;
    silent(&mut other_tenant).await;

    one.stop().await;
    two.stop().await;
    test.cleanup().await.expect("cleanup");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs FORGE_TEST_DATABASE_URL"]
async fn oversized_messages_are_refused_before_anything_is_sent() {
    let test = fresh().await;
    let one = Replica::start(&test).await;
    let channel = Channel::global("posts").expect("valid");
    let mut subscriber = one.broker.subscribe(&channel).expect("subscribes");

    let too_large =
        Message::new("x".repeat(forge_broadcast::postgres::DEFAULT_MAX_MESSAGE_BYTES + 1));
    assert!(matches!(
        one.broker.publish(&channel, too_large).await,
        Err(BroadcastError::MessageTooLarge)
    ));
    silent(&mut subscriber).await;

    one.stop().await;
    test.cleanup().await.expect("cleanup");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs FORGE_TEST_DATABASE_URL"]
async fn transactional_publishes_are_delivered_only_on_commit() {
    let test = fresh().await;
    let one = Replica::start(&test).await;
    let two = Replica::start(&test).await;
    let channel = Channel::global("posts").expect("valid");
    let mut on_one = one.broker.subscribe(&channel).expect("subscribes");
    let mut on_two = two.broker.subscribe(&channel).expect("subscribes");
    let pool = test.database().pool().clone();

    let mut rolled_back = pool.begin().await.expect("begin");
    one.broker
        .publish_in(&mut rolled_back, &channel, &Message::new("never"))
        .await
        .expect("queues");
    rolled_back.rollback().await.expect("rollback");

    let mut committed = pool.begin().await.expect("begin");
    one.broker
        .publish_in(&mut committed, &channel, &Message::new("committed"))
        .await
        .expect("queues");
    silent(&mut on_one).await;
    committed.commit().await.expect("commit");

    // Delivered everywhere, the publishing replica included, once.
    assert_eq!(next(&mut on_one).await.as_str(), "committed");
    assert_eq!(next(&mut on_two).await.as_str(), "committed");
    silent(&mut on_one).await;
    silent(&mut on_two).await;

    one.stop().await;
    two.stop().await;
    test.cleanup().await.expect("cleanup");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs FORGE_TEST_DATABASE_URL"]
async fn malformed_notifications_are_ignored_and_listening_continues() {
    let test = fresh().await;
    let one = Replica::start(&test).await;
    let channel = Channel::global("posts").expect("valid");
    let mut subscriber = one.broker.subscribe(&channel).expect("subscribes");
    let pool = test.database().pool().clone();

    for payload in [
        "garbage",
        "v1 o i 0 1 global/posts not-base64!",
        // A well-formed chunk for an invalid channel key.
        "v1 o i 0 1 global/Posts aGk=",
        "v1 o i 0 1 nobody/posts aGk=",
        // A first chunk whose continuation never comes.
        "v1 o j 0 2 global/posts aGk=",
    ] {
        sqlx::query("SELECT pg_notify('forge_broadcast', $1)")
            .bind(payload)
            .execute(&pool)
            .await
            .expect("notify");
    }
    // A foreign but well-formed message (another publisher with database
    // access) is delivered: the database role is the trust boundary.
    sqlx::query("SELECT pg_notify('forge_broadcast', 'v1 other id 0 1 global/posts aGk=')")
        .execute(&pool)
        .await
        .expect("notify");

    assert_eq!(next(&mut subscriber).await.as_str(), "hi");
    silent(&mut subscriber).await;
    assert!(one.broker.listening());

    one.stop().await;
    test.cleanup().await.expect("cleanup");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs FORGE_TEST_DATABASE_URL"]
async fn a_lost_listener_connection_is_re_established() {
    let test = fresh().await;
    let one = Replica::start(&test).await;
    let two = Replica::start(&test).await;
    let channel = Channel::global("posts").expect("valid");
    let mut on_one = one.broker.subscribe(&channel).expect("subscribes");
    let pool = test.database().pool().clone();

    let terminated: Vec<bool> = sqlx::query_scalar(
        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
         WHERE datname = current_database() AND query LIKE 'LISTEN%'",
    )
    .fetch_all(&pool)
    .await
    .expect("terminate listeners");
    assert!(!terminated.is_empty(), "the listeners were found");

    // Messages sent while disconnected may be lost; keep publishing until
    // the reconnected listener hears one.
    let heard = tokio::time::timeout(WAIT * 3, async {
        loop {
            two.broker
                .publish(&channel, Message::new("after"))
                .await
                .expect("publishes");
            if let Ok(Ok(message)) =
                tokio::time::timeout(Duration::from_millis(250), on_one.next()).await
            {
                return message;
            }
        }
    })
    .await
    .expect("the listener must reconnect");
    assert_eq!(heard.as_str(), "after");

    one.stop().await;
    two.stop().await;
    test.cleanup().await.expect("cleanup");
}

#[test]
fn invalid_options_are_refused_without_a_database() {
    let shutdown = Cancellation::new();
    let local = MemoryBroker::new(MemoryOptions::default(), &shutdown).expect("local");
    let url = forge_config::SecretString::new("postgres://user:secret@127.0.0.1:1/none");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let _guard = runtime.enter();
    let database = forge_db::PgDatabase::connect_lazy(&url).expect("lazy pool");
    for channel in ["", "Forge", "a-b", "x\"; LISTEN y; --"] {
        let options = PgOptions {
            notify_channel: channel.to_owned(),
            ..PgOptions::default()
        };
        assert!(
            matches!(
                PgBroker::new(database.clone(), local.clone(), options),
                Err(BroadcastError::Configuration(_))
            ),
            "{channel:?}"
        );
    }
}
