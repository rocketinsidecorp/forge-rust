//! Conformance and pruning tests of the shared caches. SQLite always runs;
//! server backends run with `--include-ignored` and their test URLs
//! (`FORGE_TEST_DATABASE_URL`, `FORGE_TEST_MYSQL_URL` or
//! `FORGE_TEST_MARIADB_URL`).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{sync::Arc, time::Duration};

use forge_cache::{
    Cache, CacheError, MAX_PRUNE_ROUNDS, Options, Pruner, StoreOptions,
    testing::{Subject, run},
};
use forge_db::{Database, testing::TestDatabase};

const HOUR: Option<Duration> = Some(Duration::from_secs(3600));

macro_rules! store_tests {
    ($module:ident, $backend:ty, $cache:ty, $env:expr, $migrations:expr, $hash_probe:expr; $(#[$attr:meta])*) => {
        mod $module {
            use super::*;

            type Db = $backend;

            async fn fresh(options: StoreOptions) -> (Arc<$cache>, TestDatabase<Db>) {
                let test = TestDatabase::<Db>::create_from_env($env, &$migrations)
                    .await
                    .expect("provision the test database");
                let cache = <$cache>::new(test.database().clone(), options).expect("cache");
                (Arc::new(cache), test)
            }

            async fn rows(database: &Database<Db>) -> i64 {
                sqlx::query_scalar("SELECT COUNT(*) FROM forge_cache_entries")
                    .fetch_one(database.pool())
                    .await
                    .expect("count")
            }

            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            $(#[$attr])*
            async fn conforms_to_the_cache_contract() {
                run(|options| async move {
                    let (cache, test) = fresh(
                        StoreOptions::default().with_options(options).with_prune_interval(None),
                    )
                    .await;
                    Subject {
                        cache: cache.clone(),
                        pruner: Some(cache),
                        guard: Box::new(test),
                    }
                })
                .await;
            }

            #[tokio::test]
            $(#[$attr])*
            async fn keys_are_stored_only_as_hashes() {
                let (cache, test) = fresh(StoreOptions::default()).await;
                cache.set("session:alice@example.com", b"v", HOUR).await.expect("set");
                let probe: i64 = sqlx::query_scalar($hash_probe)
                    .fetch_one(test.database().pool())
                    .await
                    .expect("probe");
                assert_eq!(probe, 0, "a row contains the key");
                let digests: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM forge_cache_entries WHERE LENGTH(key_hash) = 32",
                )
                .fetch_one(test.database().pool())
                .await
                .expect("lengths");
                assert_eq!(digests, 1, "rows are addressed by a SHA-256 digest");
            }

            #[tokio::test]
            $(#[$attr])*
            async fn set_prunes_opportunistically() {
                let options = StoreOptions::default()
                    .with_options(Options::default().with_max_entries(3))
                    .with_prune_interval(Some(Duration::from_millis(1)))
                    .with_prune_batch(100);
                let (cache, _test) = fresh(options).await;
                let keys = ["a", "b", "c", "d", "e", "f"];
                for key in keys {
                    cache.set(key, b"v", HOUR).await.expect("set");
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                // Writes pruned inline: after one more explicit prune the bound holds.
                cache.prune().await.expect("prune");
                let mut kept = 0;
                for key in keys {
                    if cache.get(key).await.expect("get").is_some() {
                        kept += 1;
                    }
                }
                assert_eq!(kept, 3);
            }

            #[tokio::test]
            $(#[$attr])*
            async fn one_prune_is_bounded_work() {
                let options = StoreOptions::default()
                    .with_options(Options::default().with_max_entries(1))
                    .with_prune_interval(None)
                    .with_prune_batch(2);
                let (cache, _test) = fresh(options).await;
                for i in 0..60 {
                    cache.set(&format!("k{i:02}"), b"v", HOUR).await.expect("set");
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
                // One call is bounded work: MAX_PRUNE_ROUNDS batches of 2.
                let per_call = u64::from(MAX_PRUNE_ROUNDS) * 2;
                for expected in [per_call, 59 - per_call, 0] {
                    assert_eq!(cache.prune().await.expect("prune"), expected);
                }
            }

            #[tokio::test]
            $(#[$attr])*
            async fn sustained_writes_of_new_keys_stay_near_the_bound() {
                // The default interval is a minute: only the write counter can keep up.
                let options = StoreOptions::default()
                    .with_options(Options::default().with_max_entries(100))
                    .with_prune_batch(50);
                let (cache, test) = fresh(options).await;
                for i in 0..2000 {
                    cache.set(&format!("k{i}"), b"v", HOUR).await.expect("set");
                }
                let rows = rows(test.database()).await;
                assert!(
                    rows <= 100 + 2 * 50,
                    "{rows} rows after 2000 writes of new keys with max_entries=100"
                );
            }

            #[tokio::test]
            $(#[$attr])*
            async fn entries_are_shared_between_instances_and_migrations_roll_back() {
                let (first, test) = fresh(StoreOptions::default()).await;
                first.set("shared", b"v", HOUR).await.expect("set");
                let second = <$cache>::new(test.database().clone(), StoreOptions::default()).expect("cache");
                assert_eq!(second.get("shared").await.expect("get").as_deref(), Some(&b"v"[..]));
                assert_eq!(
                    test.database().rollback(&$migrations).await.expect("rollback"),
                    Some(20_261_019_000_100)
                );
                assert!(second.get("shared").await.is_err(), "the table is gone after rollback");
            }

            #[tokio::test]
            $(#[$attr])*
            async fn options_are_validated_and_errors_never_carry_keys() {
                let test = TestDatabase::<Db>::create_empty_from_env($env).await.expect("db");
                for bad in [
                    StoreOptions::default().with_prune_batch(0),
                    StoreOptions::default().with_options(Options::default().with_max_entries(0)),
                ] {
                    assert!(matches!(
                        <$cache>::new(test.database().clone(), bad),
                        Err(CacheError::Configuration(_))
                    ));
                }
                // No migration applied: the table is missing.
                let cache = <$cache>::new(test.database().clone(), StoreOptions::default()).expect("cache");
                let error = cache.get("secret-key-name").await.expect_err("no table");
                let text = format!("{error} {error:?}");
                assert!(matches!(error, CacheError::Store(_)));
                assert!(!text.contains("secret-key-name"), "{text}");
            }
        }
    };
}

store_tests!(
    sqlite, sqlx::Sqlite, forge_cache::sqlite::SqliteCache, "",
    forge_cache::sqlite::MIGRATIONS,
    "SELECT COUNT(*) FROM forge_cache_entries WHERE instr(key_hash, CAST('alice' AS BLOB)) > 0";
);
store_tests!(
    postgres, sqlx::Postgres, forge_cache::postgres::PostgresCache,
    "FORGE_TEST_DATABASE_URL", forge_cache::postgres::MIGRATIONS,
    "SELECT count(*) FROM forge_cache_entries WHERE position('alice'::bytea in key_hash) > 0";
    #[ignore = "needs FORGE_TEST_DATABASE_URL"]
);
store_tests!(
    mysql, sqlx::MySql, forge_cache::mysql::MySqlCache,
    "FORGE_TEST_MYSQL_URL", forge_cache::mysql::MIGRATIONS,
    "SELECT COUNT(*) FROM forge_cache_entries WHERE LOCATE('alice', key_hash) > 0";
    #[ignore = "needs FORGE_TEST_MYSQL_URL"]
);
store_tests!(
    mariadb, sqlx::MySql, forge_cache::mysql::MySqlCache,
    "FORGE_TEST_MARIADB_URL", forge_cache::mysql::MIGRATIONS,
    "SELECT COUNT(*) FROM forge_cache_entries WHERE LOCATE('alice', key_hash) > 0";
    #[ignore = "needs FORGE_TEST_MARIADB_URL"]
);
