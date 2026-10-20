//! Conformance and pruning tests of the shared limiters. SQLite always runs;
//! server backends run with `--include-ignored` and their test URLs
//! (`FORGE_TEST_DATABASE_URL`, `FORGE_TEST_MYSQL_URL` or
//! `FORGE_TEST_MARIADB_URL`).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{sync::Arc, time::Duration};

use forge_db::{Database, testing::TestDatabase};
use forge_ratelimit::{
    Limit, LimitError, Limiter, MAX_PRUNE_ROUNDS, Pruner, StoreOptions,
    testing::{Subject, run},
};

fn limit(n: u32, per: Duration) -> Limit {
    Limit::new(n, per).expect("valid limit")
}

macro_rules! store_tests {
    ($module:ident, $backend:ty, $limiter:ty, $env:expr, $migrations:expr; $(#[$attr:meta])*) => {
        mod $module {
            use super::*;

            type Db = $backend;

            async fn fresh(options: StoreOptions) -> (Arc<$limiter>, TestDatabase<Db>) {
                let test = TestDatabase::<Db>::create_from_env($env, &$migrations)
                    .await
                    .expect("provision the test database");
                let limiter = <$limiter>::new(test.database().clone(), options).expect("limiter");
                (Arc::new(limiter), test)
            }

            async fn rows(database: &Database<Db>) -> i64 {
                sqlx::query_scalar("SELECT COUNT(*) FROM forge_rate_limits")
                    .fetch_one(database.pool())
                    .await
                    .expect("count")
            }

            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            $(#[$attr])*
            async fn conforms_to_the_limiter_contract() {
                run(|| async {
                    let (limiter, test) =
                        fresh(StoreOptions::default().with_prune_interval(None)).await;
                    Subject {
                        limiter: limiter.clone(),
                        pruner: Some(limiter),
                        guard: Box::new(test),
                    }
                })
                .await;
            }

            #[tokio::test]
            $(#[$attr])*
            async fn allow_prunes_expired_buckets_opportunistically() {
                let options = StoreOptions::default()
                    .with_prune_interval(Some(Duration::from_millis(1)))
                    .with_prune_batch(1000);
                let (limiter, test) = fresh(options).await;
                let short = limit(1, Duration::from_millis(50));
                for key in ["a", "b", "c"] {
                    limiter.allow(key, short).await.expect("allow");
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
                limiter.allow("d", limit(1, Duration::from_secs(60))).await.expect("allow");
                // The prune ran inline after the hit above: nothing expired is left.
                assert_eq!(limiter.prune().await.expect("prune"), 0);
                assert_eq!(rows(test.database()).await, 1);
            }

            #[tokio::test]
            $(#[$attr])*
            async fn one_prune_is_bounded_work() {
                let options = StoreOptions::default()
                    .with_prune_interval(None)
                    .with_prune_batch(2);
                let (limiter, _test) = fresh(options).await;
                for i in 0..60 {
                    limiter
                        .allow(&format!("k{i:02}"), limit(1, Duration::from_millis(20)))
                        .await
                        .expect("allow");
                }
                tokio::time::sleep(Duration::from_millis(150)).await;
                let per_call = u64::from(MAX_PRUNE_ROUNDS) * 2;
                for expected in [per_call, 60 - per_call, 0] {
                    assert_eq!(limiter.prune().await.expect("prune"), expected);
                }
            }

            #[tokio::test]
            $(#[$attr])*
            async fn sustained_new_keys_do_not_pile_up_expired_buckets() {
                // The default interval is a minute: only the hit counter can keep up.
                let (limiter, test) = fresh(StoreOptions::default().with_prune_batch(50)).await;
                for i in 0..1500 {
                    limiter
                        .allow(&format!("k{i}"), limit(1, Duration::from_millis(1)))
                        .await
                        .expect("allow");
                    if i % 100 == 99 {
                        tokio::time::sleep(Duration::from_millis(3)).await;
                    }
                }
                let rows = rows(test.database()).await;
                assert!(
                    rows <= 400,
                    "{rows} buckets after 1500 hits of new, instantly expiring keys"
                );
            }

            #[tokio::test]
            $(#[$attr])*
            async fn buckets_survive_a_new_limiter_instance_and_migrations_roll_back() {
                let (first, test) = fresh(StoreOptions::default()).await;
                let l = limit(1, Duration::from_secs(60));
                assert!(first.allow("shared", l).await.expect("allow").allowed);
                let second = <$limiter>::new(test.database().clone(), StoreOptions::default()).expect("limiter");
                assert!(
                    !second.allow("shared", l).await.expect("allow").allowed,
                    "a second replica sees the first one's hit"
                );
                assert_eq!(test.database().rollback(&$migrations).await.expect("rollback"), Some(20_261_019_000_000));
                assert!(second.allow("x", l).await.is_err(), "the table is gone after rollback");
            }

            #[tokio::test]
            $(#[$attr])*
            async fn options_are_validated() {
                let test = TestDatabase::<Db>::create_empty_from_env($env).await.expect("db");
                for batch in [0, 100_001] {
                    let result = <$limiter>::new(
                        test.database().clone(),
                        StoreOptions::default().with_prune_batch(batch),
                    );
                    assert!(matches!(result, Err(LimitError::Configuration(_))));
                }
            }

            #[tokio::test]
            $(#[$attr])*
            async fn store_errors_never_carry_sql_or_keys() {
                let test = TestDatabase::<Db>::create_empty_from_env($env).await.expect("db");
                // No migration applied: the table is missing.
                let limiter = <$limiter>::new(test.database().clone(), StoreOptions::default()).expect("limiter");
                let secret = "k".repeat(64);
                let error = limiter.allow(&secret, limit(1, Duration::from_secs(1))).await.expect_err("no table");
                let text = format!("{error} {error:?}");
                assert!(matches!(error, LimitError::Store(_)));
                assert!(!text.contains(&secret), "{text}");
                assert!(!text.to_lowercase().contains("insert into"), "{text}");
            }
        }
    };
}

store_tests!(
    sqlite, sqlx::Sqlite, forge_ratelimit::sqlite::SqliteLimiter, "",
    forge_ratelimit::sqlite::MIGRATIONS;
);
store_tests!(
    postgres, sqlx::Postgres, forge_ratelimit::postgres::PostgresLimiter,
    "FORGE_TEST_DATABASE_URL", forge_ratelimit::postgres::MIGRATIONS;
    #[ignore = "needs FORGE_TEST_DATABASE_URL"]
);
store_tests!(
    mysql, sqlx::MySql, forge_ratelimit::mysql::MySqlLimiter,
    "FORGE_TEST_MYSQL_URL", forge_ratelimit::mysql::MIGRATIONS;
    #[ignore = "needs FORGE_TEST_MYSQL_URL"]
);
store_tests!(
    mariadb, sqlx::MySql, forge_ratelimit::mysql::MySqlLimiter,
    "FORGE_TEST_MARIADB_URL", forge_ratelimit::mysql::MIGRATIONS;
    #[ignore = "needs FORGE_TEST_MARIADB_URL"]
);
