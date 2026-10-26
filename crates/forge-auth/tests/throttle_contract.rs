//! The `LoginThrottle` contract, run against the in-memory throttle (always)
//! and the PostgreSQL one (with `--include-ignored` and
//! `FORGE_TEST_DATABASE_URL`, on a database the test creates and drops):
//! the same limits, refusals, clearing and windows, and no lost update under
//! concurrent attempts. The PostgreSQL tests also check what only a shared
//! store can do: counters survive a restart and are shared by replicas, and
//! expired rows are pruned.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{
    net::{IpAddr, Ipv4Addr},
    sync::Arc,
    time::Duration,
};

use forge_auth::{
    LoginThrottle, MemoryLoginThrottle, PgLoginThrottle, ThrottleConfig, ThrottleDecision,
    ThrottleKey,
};
use forge_db::{sqlx, testing::TestDatabase};

/// The generated application's migration, the table's only definition.
const UP: &str = include_str!(
    "../../forge-cli/templates/app/migrations/00000000000000000005_forge_login_throttle.up.sql.tmpl"
);
const DOWN: &str = include_str!(
    "../../forge-cli/templates/app/migrations/00000000000000000005_forge_login_throttle.down.sql.tmpl"
);

const WINDOW: Duration = Duration::from_secs(60);

fn config(window: Duration) -> ThrottleConfig {
    ThrottleConfig {
        window,
        pair_limit: 3,
        account_limit: 5,
        source_limit: 8,
        max_entries: 20_000,
    }
}

type Make = dyn Fn(ThrottleConfig) -> Arc<dyn LoginThrottle>;

fn source(network: u8, host: u8) -> Option<IpAddr> {
    Some(IpAddr::V4(Ipv4Addr::new(10, 0, network, host)))
}

fn key(account: &str, network: u8, host: u8) -> ThrottleKey {
    ThrottleKey::new("sign_in", account, source(network, host))
}

async fn allowed(throttle: &dyn LoginThrottle, key: &ThrottleKey) -> bool {
    match throttle.attempt(key).await.expect("throttle decides") {
        ThrottleDecision::Allowed => true,
        ThrottleDecision::Throttled { retry_after } => {
            assert!(
                retry_after > Duration::ZERO && retry_after <= WINDOW,
                "{retry_after:?}"
            );
            false
        }
    }
}

/// Every scenario uses its own accounts and sources, so they can share one
/// store.
async fn contract(make: &Make) {
    let throttle = make(config(WINDOW));
    let throttle = throttle.as_ref();

    // The pair limit, then a refusal with a retry inside the window.
    let pair = key("pair@example.com", 1, 1);
    for _ in 0..3 {
        assert!(allowed(throttle, &pair).await);
    }
    assert!(!allowed(throttle, &pair).await);

    // Refused attempts are not counted: other sources still have the
    // account's remaining budget (5 - 3), then the account is capped for
    // every source.
    assert!(allowed(throttle, &key("pair@example.com", 1, 2)).await);
    assert!(allowed(throttle, &key("pair@example.com", 1, 3)).await);
    assert!(!allowed(throttle, &key("pair@example.com", 1, 4)).await);

    // One source spraying accounts is capped by the source limit, and a
    // success does not reset the source.
    for n in 0..8 {
        let spray = key(&format!("spray{n}@example.com"), 2, 1);
        assert!(allowed(throttle, &spray).await, "attempt {n}");
        if n == 0 {
            throttle.success(&spray).await.expect("success");
        }
    }
    assert!(!allowed(throttle, &key("spray9@example.com", 2, 1)).await);
    assert!(allowed(throttle, &key("spray9@example.com", 2, 2)).await);

    // A success clears the pair and the account.
    let cleared = key("cleared@example.com", 3, 1);
    for _ in 0..3 {
        assert!(allowed(throttle, &cleared).await);
    }
    assert!(!allowed(throttle, &cleared).await);
    throttle.success(&cleared).await.expect("success");
    for _ in 0..3 {
        assert!(allowed(throttle, &cleared).await);
    }
    assert!(!allowed(throttle, &cleared).await);

    // Throttle names are independent, and a key without a source counts
    // only the account.
    let reset = ThrottleKey::new("password_reset", "pair@example.com", source(1, 1));
    assert!(allowed(throttle, &reset).await);
    let sourceless = ThrottleKey::new("sign_in", "nosource@example.com", None);
    for _ in 0..5 {
        assert!(allowed(throttle, &sourceless).await);
    }
    assert!(!allowed(throttle, &sourceless).await);

    // Concurrent first attempts of one key cannot exceed the limit.
    let racing = Arc::new(key("race@example.com", 4, 1));
    let shared: Arc<dyn LoginThrottle> = make(config(WINDOW));
    let mut tasks = Vec::new();
    for _ in 0..16 {
        let throttle = shared.clone();
        let racing = racing.clone();
        tasks.push(tokio::spawn(async move {
            matches!(
                throttle.attempt(&racing).await.expect("decides"),
                ThrottleDecision::Allowed
            )
        }));
    }
    let mut admitted = 0;
    for task in tasks {
        admitted += usize::from(task.await.expect("join"));
    }
    assert_eq!(admitted, 3, "exactly the pair limit is admitted");

    // A window ends: the counters start over.
    let short = make(config(Duration::from_millis(400)));
    // The refusal only holds inside the window (it opens at the first
    // attempt, after `started`): a store that took the whole window to answer
    // four attempts (a loaded CI runner) may rightly have opened a new one. It
    // is asserted on the first attempt, each on a fresh pair, whose attempts
    // finished inside the window; if none does, the test fails, so the
    // assertion cannot be skipped silently.
    let mut asserted = false;
    for attempt in 0..5_u8 {
        let expiring = key(&format!("expiring{attempt}@example.com"), 5 + attempt, 1);
        let started = std::time::Instant::now();
        for _ in 0..3 {
            assert!(allowed(short.as_ref(), &expiring).await);
        }
        let decision = short.attempt(&expiring).await.expect("decides");
        if started.elapsed() >= Duration::from_millis(400) {
            continue;
        }
        match decision {
            ThrottleDecision::Throttled { retry_after } => {
                assert!(retry_after <= Duration::from_millis(400), "{retry_after:?}");
            }
            ThrottleDecision::Allowed => panic!("the pair is exhausted"),
        }
        // real-clock: the store compares expiry with a real clock (database now() or the system clock) that the test cannot inject
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert!(matches!(
            short.attempt(&expiring).await.expect("decides"),
            ThrottleDecision::Allowed
        ));
        asserted = true;
        break;
    }
    assert!(
        asserted,
        "no attempt finished its four attempts inside the 400 ms window in 5 tries"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_memory_throttle_meets_the_contract() {
    contract(&|config| Arc::new(MemoryLoginThrottle::new(config).expect("valid config"))).await;
}

async fn database() -> TestDatabase<sqlx::Postgres> {
    let test = TestDatabase::<sqlx::Postgres>::create_empty()
        .await
        .expect("provision a PostgreSQL test database");
    sqlx::raw_sql(UP)
        .execute(test.database().pool())
        .await
        .expect("apply the throttle migration");
    test
}

async fn rows(pool: &sqlx::PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM forge_login_throttle")
        .fetch_one(pool)
        .await
        .expect("count")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs FORGE_TEST_DATABASE_URL"]
async fn the_postgres_throttle_meets_the_contract() {
    let test = database().await;
    let pool = test.database().pool().clone();
    contract(&move |config| {
        Arc::new(PgLoginThrottle::new(pool.clone(), config).expect("valid config"))
    })
    .await;
    test.cleanup().await.expect("cleanup");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs FORGE_TEST_DATABASE_URL"]
async fn counters_are_shared_by_replicas_and_survive_a_restart() {
    let test = database().await;
    let pool = test.database().pool().clone();
    let replica = |pool: &sqlx::PgPool| {
        PgLoginThrottle::new(pool.clone(), config(WINDOW)).expect("valid config")
    };
    let guess = key("replicas@example.com", 6, 1);
    // Two replicas: their attempts add up.
    let (first, second) = (replica(&pool), replica(&pool));
    assert!(allowed(&first, &guess).await);
    assert!(allowed(&second, &guess).await);
    assert!(allowed(&first, &guess).await);
    assert!(!allowed(&second, &guess).await);
    drop((first, second));

    // A restart: a new process with a new pool still refuses.
    let restarted_pool = sqlx::PgPool::connect(test.url().expose())
        .await
        .expect("new pool");
    let restarted = replica(&restarted_pool);
    assert!(!allowed(&restarted, &guess).await);
    // Nothing a refusal touched was stored: three counters, no empty rows.
    assert_eq!(rows(&restarted_pool).await, 3);
    restarted_pool.close().await;
    test.cleanup().await.expect("cleanup");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs FORGE_TEST_DATABASE_URL"]
async fn expired_counters_are_pruned_and_a_refusal_stores_nothing() {
    let test = database().await;
    let pool = test.database().pool().clone();
    let short = PgLoginThrottle::new(pool.clone(), config(Duration::from_millis(200)))
        .expect("valid config");
    // 4 keys of 3 counters each.
    for n in 0..4 {
        assert!(allowed(&short, &key(&format!("old{n}@example.com"), 7, n)).await);
    }
    assert_eq!(rows(&pool).await, 12);
    // real-clock: the store compares expiry with a real clock (database now() or the system clock) that the test cannot inject
    tokio::time::sleep(Duration::from_millis(400)).await;

    // An attempt prunes up to 8 expired counters besides creating its own 3.
    let long = PgLoginThrottle::new(pool.clone(), config(WINDOW)).expect("valid config");
    assert!(allowed(&long, &key("new@example.com", 8, 1)).await);
    assert_eq!(rows(&pool).await, 12 - 8 + 3);
    // The rest goes on request; live counters stay.
    assert_eq!(long.prune_expired(100).await.expect("prune"), 4);
    assert_eq!(rows(&pool).await, 3);
    assert_eq!(long.prune_expired(100).await.expect("prune"), 0);

    // A refused first attempt of a new key leaves no row behind: a blocked
    // source cannot fill the table with new accounts.
    let blocked_source = |n: u8| key(&format!("victim{n}@example.com"), 9, 1);
    for n in 0..8 {
        assert!(allowed(&long, &blocked_source(n)).await);
    }
    let before = rows(&pool).await;
    for n in 8..20 {
        assert!(!allowed(&long, &blocked_source(n)).await);
    }
    assert_eq!(rows(&pool).await, before);
    test.cleanup().await.expect("cleanup");
}

#[tokio::test]
#[ignore = "needs FORGE_TEST_DATABASE_URL"]
async fn the_migration_stores_digests_only_and_reverts() {
    // No database-generated identifiers (gen_random_uuid is UUIDv4): the
    // rows are keyed by Forge's digests.
    assert!(!UP.contains("gen_random_uuid") && !UP.contains("uuid"));
    let test = database().await;
    let pool = test.database().pool().clone();
    for refused in [
        "INSERT INTO forge_login_throttle VALUES (4, decode(repeat('ab', 32), 'hex'), 1, now())",
        "INSERT INTO forge_login_throttle VALUES (1, decode('ab', 'hex'), 1, now())",
        "INSERT INTO forge_login_throttle VALUES (1, decode(repeat('ab', 32), 'hex'), -1, now())",
    ] {
        assert!(
            sqlx::query(refused).execute(&pool).await.is_err(),
            "{refused}"
        );
    }
    sqlx::raw_sql(DOWN).execute(&pool).await.expect("down");
    let table: Option<String> =
        sqlx::query_scalar("SELECT to_regclass('public.forge_login_throttle')::text")
            .fetch_one(&pool)
            .await
            .expect("lookup");
    assert_eq!(table, None);
    // Without its table the throttle fails closed: callers refuse.
    let throttle = PgLoginThrottle::new(pool, config(WINDOW)).expect("valid config");
    assert!(
        throttle
            .attempt(&key("gone@example.com", 10, 1))
            .await
            .is_err()
    );
    test.cleanup().await.expect("cleanup");
}

#[tokio::test]
async fn configurations_the_table_cannot_hold_are_refused() {
    // A lazy pool never connects here.
    let pool = sqlx::PgPool::connect_lazy("postgres://u:p@127.0.0.1:1/db").expect("lazy pool");
    for refused in [
        ThrottleConfig {
            window: Duration::ZERO,
            ..config(WINDOW)
        },
        ThrottleConfig {
            window: Duration::from_secs(400 * 24 * 3600),
            ..config(WINDOW)
        },
        ThrottleConfig {
            pair_limit: u32::MAX,
            ..config(WINDOW)
        },
        ThrottleConfig {
            account_limit: 0,
            ..config(WINDOW)
        },
    ] {
        assert!(
            PgLoginThrottle::new(pool.clone(), refused).is_err(),
            "{refused:?}"
        );
    }
    let throttle = PgLoginThrottle::new(pool, config(WINDOW)).expect("valid");
    assert!(!format!("{throttle:?}").contains("u:p"));
    // Every replica shares its counters: production wiring does not warn.
    assert!(!throttle.is_process_local());
}
