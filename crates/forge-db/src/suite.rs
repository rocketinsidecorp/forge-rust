//! Cross-backend contract suite.
//!
//! The same assertions run against SQLite (always), PostgreSQL
//! (`FORGE_TEST_DATABASE_URL`), MySQL (`FORGE_TEST_MYSQL_URL`) and MariaDB
//! (`FORGE_TEST_MARIADB_URL`); server backends are `#[ignore]`d unless
//! requested with `--include-ignored`.

use std::time::Duration;

use forge_config::{DatabaseScheme, SecretString};
use forge_core::{Id, Version};
use sqlx::{
    AssertSqlSafe, Row, SqlSafeStr,
    migrate::{MigrateError, Migration, MigrationType, Migrator},
};
use uuid::Uuid;

use crate::{
    Backend, Database, DbError, FRAMEWORK_MIGRATIONS, PoolSettings, Transaction,
    TransactionManager, combine_migrations, expect_rows, testing::TestDatabase,
};

enum Owner {}

/// Rewrites `?` placeholders to `$n` for PostgreSQL.
fn q<B: Backend>(sql: &str) -> AssertSqlSafe<String> {
    if <B as Backend>::SCHEME != DatabaseScheme::Postgres {
        return AssertSqlSafe(sql.to_owned());
    }
    let mut out = String::with_capacity(sql.len() + 4);
    let mut n = 0;
    for ch in sql.chars() {
        if ch == '?' {
            n += 1;
            out.push('$');
            out.push_str(&n.to_string());
        } else {
            out.push(ch);
        }
    }
    AssertSqlSafe(out)
}

static POSTGRES_MIGRATIONS: Migrator = sqlx::migrate!("tests/migrations/postgres");
static MYSQL_MIGRATIONS: Migrator = sqlx::migrate!("tests/migrations/mysql");
static SQLITE_MIGRATIONS: Migrator = sqlx::migrate!("tests/migrations/sqlite");

/// Stands in for the migrations Forge crates ship (`forge-db` cannot depend
/// on them): the registered versions and descriptions, each creating a
/// `framework_<version>` table with portable SQL. Their versions are higher
/// than the application's, as in a generated application (`1..3`).
fn framework_set() -> Migrator {
    Migrator::with_migrations(
        FRAMEWORK_MIGRATIONS
            .iter()
            .flat_map(|migration| {
                let table = framework_table(migration.version);
                [
                    (
                        MigrationType::ReversibleUp,
                        format!("CREATE TABLE {table} (id INTEGER PRIMARY KEY)"),
                    ),
                    (MigrationType::ReversibleDown, format!("DROP TABLE {table}")),
                ]
                .map(|(kind, sql)| {
                    Migration::new(
                        migration.version,
                        migration.description.into(),
                        kind,
                        AssertSqlSafe(sql).into_sql_str(),
                        false,
                    )
                })
            })
            .collect(),
    )
}

fn framework_table(version: i64) -> String {
    format!("framework_{version}")
}

fn framework_versions() -> Vec<i64> {
    FRAMEWORK_MIGRATIONS.iter().map(|m| m.version).collect()
}

/// Tests every backend must pass.
///
/// `$names` says whether the backend reports constraint names for unique and
/// foreign-key violations (SQLite does not).
macro_rules! common_suite {
    ($module:ident, $db:ty, $env:expr, $migrations:expr, $names:expr; $(#[$attr:meta])*) => {
        mod $module {
            use super::*;

            type Db = $db;

            async fn fresh() -> TestDatabase<Db> {
                TestDatabase::<Db>::create_from_env($env, &$migrations)
                    .await
                    .expect("provision the test database")
            }

            async fn table_exists(db: &Database<Db>, table: &str) -> bool {
                sqlx::query(AssertSqlSafe(format!("SELECT COUNT(*) FROM {table}")))
                    .execute(db.pool())
                    .await
                    .is_ok()
            }

            async fn framework_tables_exist(db: &Database<Db>) -> bool {
                for version in framework_versions() {
                    if !table_exists(db, &framework_table(version)).await {
                        return false;
                    }
                }
                true
            }

            async fn insert_owner(db: &Database<Db>, email: &str) -> Id<Owner> {
                let id = Id::<Owner>::new();
                sqlx::query(q::<Db>("INSERT INTO owners (id, email, version) VALUES (?, ?, ?)"))
                    .bind(id)
                    .bind(email)
                    .bind(Version::INITIAL)
                    .execute(db.pool())
                    .await
                    .expect("insert owner");
                id
            }

            #[tokio::test]
            $(#[$attr])*
            async fn migrations_apply_in_order_and_roll_back_one_at_a_time() {
                let test = TestDatabase::<Db>::create_empty_from_env($env).await.expect("provision");
                let db = test.database();

                assert_eq!(db.migrate(&$migrations).await.expect("migrate"), vec![1, 2]);
                assert!(db.migrate(&$migrations).await.expect("migrate again").is_empty());
                insert_owner(db, "a@example.test").await;

                assert_eq!(db.rollback(&$migrations).await.expect("rollback"), Some(2));
                assert_eq!(db.migrate(&$migrations).await.expect("re-migrate"), vec![2]);
                assert_eq!(db.rollback(&$migrations).await.expect("rollback"), Some(2));
                assert_eq!(db.rollback(&$migrations).await.expect("rollback"), Some(1));
                assert_eq!(db.rollback(&$migrations).await.expect("nothing left"), None);
                test.cleanup().await.expect("cleanup");
            }

            /// Regression (rocketinsidecorp/forge-rust#10): the framework
            /// versions are higher, yet the application rollback reverts the
            /// application's newest migration and never a framework one.
            #[tokio::test]
            $(#[$attr])*
            async fn application_rollback_never_reverts_framework_migrations() {
                let test = TestDatabase::<Db>::create_empty_from_env($env).await.expect("provision");
                let db = test.database();
                let combined = combine_migrations(&[&$migrations, &framework_set()]).expect("combine");

                let mut all = vec![1, 2];
                all.extend(framework_versions());
                assert_eq!(db.migrate(&combined).await.expect("migrate"), all);

                assert_eq!(db.rollback(&combined).await.expect("rollback"), Some(2));
                assert!(!table_exists(db, "notes").await);
                assert!(framework_tables_exist(db).await);
                assert_eq!(db.rollback(&combined).await.expect("rollback"), Some(1));
                assert!(!table_exists(db, "owners").await);

                // Only framework migrations remain: refused, nothing reverted.
                let newest = FRAMEWORK_MIGRATIONS.iter().max_by_key(|m| m.version).expect("listed");
                let error = db.rollback(&combined).await.expect_err("refused");
                assert!(
                    matches!(
                        error,
                        DbError::FrameworkMigrationOnly { version, owner }
                            if version == newest.version && owner == newest.owner
                    ),
                    "{error:?}"
                );
                assert!(error.to_string().contains("not reverted by the application rollback"));
                assert!(framework_tables_exist(db).await);

                // The explicit framework rollback is the only way to revert them,
                // and it never touches application migrations.
                assert_eq!(db.migrate(&combined).await.expect("re-migrate"), vec![1, 2]);
                let mut framework = framework_versions();
                framework.sort_unstable();
                for version in framework.into_iter().rev() {
                    assert_eq!(
                        db.rollback_framework(&combined).await.expect("framework rollback"),
                        Some(version)
                    );
                    assert!(!table_exists(db, &framework_table(version)).await);
                }
                assert_eq!(db.rollback_framework(&combined).await.expect("none left"), None);
                assert!(table_exists(db, "notes").await);
                assert_eq!(db.rollback(&combined).await.expect("rollback"), Some(2));
                test.cleanup().await.expect("cleanup");
            }

            /// Turning a framework feature off leaves its versions applied:
            /// they are tolerated, while a version nobody owns fails closed.
            #[tokio::test]
            $(#[$attr])*
            async fn disabled_framework_migrations_are_tolerated_and_unknown_versions_refused() {
                let test = TestDatabase::<Db>::create_empty_from_env($env).await.expect("provision");
                let db = test.database();
                let combined = combine_migrations(&[&$migrations, &framework_set()]).expect("combine");
                db.migrate(&combined).await.expect("migrate");

                // The application's own migrator, without the framework sets.
                assert!(db.migrate(&$migrations).await.expect("migrate").is_empty());
                assert_eq!(db.rollback(&$migrations).await.expect("rollback"), Some(2));
                assert_eq!(db.migrate(&$migrations).await.expect("re-migrate"), vec![2]);
                assert_eq!(db.rollback_framework(&$migrations).await.expect("no set"), None);
                assert!(framework_tables_exist(db).await);

                // Re-enabling the features finds them applied and unchanged.
                assert!(db.migrate(&combined).await.expect("re-enabled").is_empty());

                // Version 2 belongs to nobody this build knows: refused.
                let stale = Migrator::with_migrations(
                    $migrations.iter().filter(|m| m.version == 1).cloned().collect(),
                );
                for result in [
                    db.migrate(&stale).await.map(|_| None),
                    db.rollback(&stale).await,
                    db.rollback_framework(&stale).await,
                ] {
                    assert!(
                        matches!(result, Err(DbError::Migration(MigrateError::VersionMissing(2)))),
                        "{result:?}"
                    );
                }
                assert!(table_exists(db, "notes").await);
                test.cleanup().await.expect("cleanup");
            }

            #[tokio::test]
            $(#[$attr])*
            async fn ids_and_versions_round_trip_and_are_validated_on_decode() {
                let test = fresh().await;
                let db = test.database();
                let id = insert_owner(db, "a@example.test").await;

                let (read_id, read_version): (Id<Owner>, Version) =
                    sqlx::query_as(q::<Db>("SELECT id, version FROM owners WHERE id = ?"))
                        .bind(id)
                        .fetch_one(db.pool())
                        .await
                        .expect("typed read");
                assert_eq!(read_id, id);
                assert_eq!(read_version, Version::INITIAL);

                // Storage format: native uuid on PostgreSQL, 16 raw bytes elsewhere.
                if <Db as Backend>::SCHEME == DatabaseScheme::Postgres {
                    let column: String = sqlx::query_scalar("SELECT pg_typeof(id)::text FROM owners")
                        .fetch_one(db.pool())
                        .await
                        .expect("column type");
                    assert_eq!(column, "uuid");
                } else {
                    let raw: Vec<u8> = sqlx::query_scalar("SELECT id FROM owners")
                        .fetch_one(db.pool())
                        .await
                        .expect("raw bytes");
                    assert_eq!(raw.as_slice(), id.as_uuid().as_bytes());
                }

                // A v4 UUID and a zero version can be stored but never decoded.
                let v4 = Uuid::from_u128(0x1234_5678_1234_4234_8234_1234_5678_1234);
                sqlx::query(q::<Db>("INSERT INTO owners (id, email, version) VALUES (?, ?, ?)"))
                    .bind(v4)
                    .bind("v4@example.test")
                    .bind(0_i64)
                    .execute(db.pool())
                    .await
                    .expect("raw insert");
                let wrong_id = sqlx::query_scalar::<_, Id<Owner>>(q::<Db>("SELECT id FROM owners WHERE email = ?"))
                    .bind("v4@example.test")
                    .fetch_one(db.pool())
                    .await;
                assert!(matches!(wrong_id, Err(sqlx::Error::ColumnDecode { .. })), "{wrong_id:?}");
                let wrong_version = sqlx::query_scalar::<_, Version>(q::<Db>("SELECT version FROM owners WHERE email = ?"))
                    .bind("v4@example.test")
                    .fetch_one(db.pool())
                    .await;
                assert!(matches!(wrong_version, Err(sqlx::Error::ColumnDecode { .. })), "{wrong_version:?}");

                // Ids sort like the time-ordered UUIDs they wrap.
                let later = insert_owner(db, "later@example.test").await;
                assert!(later > id);
                test.cleanup().await.expect("cleanup");
            }

            #[tokio::test]
            $(#[$attr])*
            async fn constraint_violations_are_classified() {
                let test = fresh().await;
                let db = test.database();
                let owner = insert_owner(db, "a@example.test").await;
                let names: bool = $names;

                let duplicate = sqlx::query(q::<Db>("INSERT INTO owners (id, email, version) VALUES (?, ?, ?)"))
                    .bind(Id::<Owner>::new())
                    .bind("a@example.test")
                    .bind(Version::INITIAL)
                    .execute(db.pool())
                    .await
                    .map_err(DbError::from)
                    .expect_err("duplicate email");
                assert!(matches!(duplicate, DbError::UniqueViolation { .. }), "{duplicate:?}");
                assert_eq!(duplicate.constraint(), names.then_some("owners_email_key"));
                assert!(!duplicate.to_string().contains("a@example.test"));

                let orphan = sqlx::query(q::<Db>("INSERT INTO notes (id, owner_id, title) VALUES (?, ?, ?)"))
                    .bind(Id::<Owner>::new())
                    .bind(Id::<Owner>::new())
                    .bind("title")
                    .execute(db.pool())
                    .await
                    .map_err(DbError::from)
                    .expect_err("missing owner");
                assert!(matches!(orphan, DbError::ForeignKeyViolation { .. }), "{orphan:?}");
                assert_eq!(orphan.constraint(), names.then_some("notes_owner_fk"));

                let blank = sqlx::query(q::<Db>("INSERT INTO notes (id, owner_id, title) VALUES (?, ?, ?)"))
                    .bind(Id::<Owner>::new())
                    .bind(owner)
                    .bind("")
                    .execute(db.pool())
                    .await
                    .map_err(DbError::from)
                    .expect_err("blank title");
                assert!(matches!(blank, DbError::CheckViolation { .. }), "{blank:?}");
                assert_eq!(blank.constraint(), Some("title_not_blank"));

                let null = sqlx::query(q::<Db>("INSERT INTO notes (id, owner_id, title) VALUES (?, ?, NULL)"))
                    .bind(Id::<Owner>::new())
                    .bind(owner)
                    .execute(db.pool())
                    .await
                    .map_err(DbError::from)
                    .expect_err("null title");
                assert!(matches!(null, DbError::NotNullViolation), "{null:?}");

                for error in [&duplicate, &orphan, &blank, &null] {
                    assert!(!error.is_retryable(), "{error:?}");
                }
                test.cleanup().await.expect("cleanup");
            }

            #[tokio::test]
            $(#[$attr])*
            async fn transactions_commit_roll_back_and_roll_back_on_drop() {
                let test = fresh().await;
                let db = test.database();
                let count = || async {
                    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM owners")
                        .fetch_one(db.pool())
                        .await
                        .expect("count")
                };
                let insert = |email: &'static str| async move {
                    let mut tx = db.begin().await.expect("begin");
                    sqlx::query(q::<Db>("INSERT INTO owners (id, email, version) VALUES (?, ?, ?)"))
                        .bind(Id::<Owner>::new())
                        .bind(email)
                        .bind(Version::INITIAL)
                        .execute(tx.connection())
                        .await
                        .expect("insert");
                    tx
                };

                insert("commit@example.test").await.commit().await.expect("commit");
                assert_eq!(count().await, 1);
                insert("rollback@example.test").await.rollback().await.expect("rollback");
                assert_eq!(count().await, 1);
                drop(insert("dropped@example.test").await);
                assert_eq!(count().await, 1);

                // The contract traits are implemented by the same types.
                let tx = <Database<Db> as TransactionManager>::begin(db).await.expect("begin");
                Transaction::commit(tx).await.expect("commit through the contract");
                test.cleanup().await.expect("cleanup");
            }

            #[tokio::test]
            $(#[$attr])*
            async fn optimistic_updates_report_conflicts_through_expect_rows() {
                let test = fresh().await;
                let db = test.database();
                let id = insert_owner(db, "a@example.test").await;
                let update = |expected: Version, next: Version| {
                    sqlx::query(q::<Db>("UPDATE owners SET version = ? WHERE id = ? AND version = ?"))
                        .bind(next)
                        .bind(id)
                        .bind(expected)
                        .execute(db.pool())
                };

                let next = Version::INITIAL.next().expect("next");
                expect_rows(update(Version::INITIAL, next).await.expect("update"), 1).expect("one row");
                let stale = expect_rows(update(Version::INITIAL, next).await.expect("update"), 1);
                assert!(matches!(stale, Err(DbError::Conflict)), "{stale:?}");
                test.cleanup().await.expect("cleanup");
            }

            #[tokio::test]
            $(#[$attr])*
            async fn ping_works_and_a_foreign_url_scheme_is_refused_without_echoing_it() {
                let test = fresh().await;
                test.database().ping().await.expect("ping");
                assert_eq!(test.database().scheme(), <Db as Backend>::SCHEME);

                let foreign = match <Db as Backend>::SCHEME {
                    DatabaseScheme::Postgres => "mysql://user:hunter2@db.example.test/app",
                    DatabaseScheme::MySql | DatabaseScheme::Sqlite => {
                        "postgres://user:hunter2@db.example.test/app"
                    }
                };
                let error = Database::<Db>::connect(&SecretString::new(foreign))
                    .await
                    .expect_err("scheme mismatch");
                assert!(matches!(error, DbError::Configuration { .. }), "{error:?}");
                let rendered = format!("{error} {error:?}");
                assert!(rendered.contains(<Db as Backend>::SCHEME.accepted_schemes()), "{rendered}");
                assert!(!rendered.contains("hunter2") && !rendered.contains("db.example.test"));
                test.cleanup().await.expect("cleanup");
            }

            #[tokio::test]
            $(#[$attr])*
            async fn pool_exhaustion_is_a_retryable_timeout() {
                let test = fresh().await;
                let settings = PoolSettings::default()
                    .with_max_connections(1)
                    .with_acquire_timeout(Duration::from_millis(1500));
                let db = Database::<Db>::connect_with(test.url(), settings).await.expect("connect");
                let held = db.pool().acquire().await.expect("hold the only connection");

                let error = db.ping().await.expect_err("no connection available");
                assert!(matches!(error, DbError::PoolTimeout), "{error:?}");
                assert!(error.is_retryable());
                drop(held);
                db.ping().await.expect("works again once released");
                test.cleanup().await.expect("cleanup");
            }

            #[tokio::test]
            $(#[$attr])*
            async fn debug_output_never_contains_the_connection_url() {
                let test = fresh().await;
                let url = test.url().expose().to_owned();
                let rendered = format!("{test:?} {:?} {:?}", test.database(), test.url());
                assert!(!rendered.contains(&url), "{rendered}");
                if let Some(password) = url.split('@').next().and_then(|auth| auth.rsplit(':').next()) {
                    if url.contains('@') {
                        assert!(!rendered.contains(password), "{rendered}");
                    }
                }
                test.cleanup().await.expect("cleanup");
            }

            #[tokio::test]
            $(#[$attr])*
            async fn test_databases_are_removed_even_when_the_test_body_panics() {
                let (sender, receiver) = std::sync::mpsc::channel();
                let outcome = std::thread::spawn(move || {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("runtime");
                    runtime.block_on(async move {
                        let test = fresh().await;
                        sender.send(test.url().expose().to_owned()).expect("send url");
                        panic!("the test body fails after provisioning");
                    })
                })
                .join();
                assert!(outcome.is_err(), "the panic must propagate");
                let url = receiver.recv().expect("url of the provisioned database");

                assert!(!database_exists::<Db>(&url).await, "database survived the panic");
            }

            #[tokio::test]
            $(#[$attr])*
            async fn cleanup_removes_the_database() {
                let test = fresh().await;
                let url = test.url().expose().to_owned();
                assert!(database_exists::<Db>(&url).await);
                test.cleanup().await.expect("cleanup");
                assert!(!database_exists::<Db>(&url).await);
            }
        }
    };
}

/// Whether the database addressed by `url` still exists.
async fn database_exists<B: Backend>(url: &str) -> bool {
    if let Some(location) = url.strip_prefix("sqlite://") {
        let path = location.split('?').next().unwrap_or(location);
        return std::path::Path::new(path).exists();
    }
    Database::<B>::connect(&SecretString::new(url))
        .await
        .is_ok()
}

common_suite!(sqlite, sqlx::Sqlite, "", SQLITE_MIGRATIONS, false;);
common_suite!(
    postgres, sqlx::Postgres, "FORGE_TEST_DATABASE_URL", POSTGRES_MIGRATIONS, true;
    #[ignore = "needs FORGE_TEST_DATABASE_URL"]
);
common_suite!(
    mysql, sqlx::MySql, "FORGE_TEST_MYSQL_URL", MYSQL_MIGRATIONS, true;
    #[ignore = "needs FORGE_TEST_MYSQL_URL"]
);
common_suite!(
    mariadb, sqlx::MySql, "FORGE_TEST_MARIADB_URL", MYSQL_MIGRATIONS, true;
    #[ignore = "needs FORGE_TEST_MARIADB_URL"]
);

/// An unreachable server is a retryable `Unavailable`, never a panic or a hang.
#[tokio::test]
async fn unreachable_servers_are_unavailable_and_retryable() {
    for url in [
        "postgres://user:hunter2@127.0.0.1:1/app",
        "mysql://user:hunter2@127.0.0.1:1/app",
        "mariadb://user:hunter2@127.0.0.1:1/app",
    ] {
        let url = SecretString::new(url);
        let error = if url.expose().starts_with("postgres") {
            Database::<sqlx::Postgres>::connect(&url)
                .await
                .expect_err("refused")
        } else {
            Database::<sqlx::MySql>::connect(&url)
                .await
                .expect_err("refused")
        };
        assert!(matches!(error, DbError::Unavailable(_)), "{error:?}");
        assert!(error.is_retryable());
        assert!(!error.to_string().contains("hunter2"));
    }
}

/// A lazy pool is created without a reachable database.
#[tokio::test]
async fn lazy_pools_connect_on_first_use() {
    let url = SecretString::new("postgres://user:hunter2@127.0.0.1:1/app");
    let db = Database::<sqlx::Postgres>::connect_lazy(&url).expect("lazy pool");
    let error = db.ping().await.expect_err("nothing listens on port 1");
    assert!(error.is_retryable(), "{error:?}");
    assert!(!format!("{db:?}").contains("hunter2"));
}

mod sqlite_specific {
    use super::*;
    use sqlx::Sqlite;

    #[tokio::test]
    async fn connections_enable_foreign_keys_wal_and_a_busy_timeout() {
        let test = TestDatabase::<Sqlite>::create(&SQLITE_MIGRATIONS)
            .await
            .expect("provision");
        let pool = test.database().pool();
        let foreign_keys: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
            .fetch_one(pool)
            .await
            .expect("pragma");
        let journal: String = sqlx::query_scalar("PRAGMA journal_mode")
            .fetch_one(pool)
            .await
            .expect("pragma");
        let busy: i64 = sqlx::query_scalar("PRAGMA busy_timeout")
            .fetch_one(pool)
            .await
            .expect("pragma");
        assert_eq!((foreign_keys, journal.as_str(), busy), (1, "wal", 5000));
        test.cleanup().await.expect("cleanup");
    }

    #[tokio::test]
    async fn files_are_created_only_with_mode_rwc() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("app.db");

        let plain = SecretString::new(format!("sqlite://{}", path.display()));
        assert!(
            Database::<Sqlite>::connect(&plain).await.is_err(),
            "must not create the file"
        );
        assert!(!path.exists());

        let creating = SecretString::new(format!("sqlite://{}?mode=rwc", path.display()));
        Database::<Sqlite>::connect(&creating)
            .await
            .expect("rwc creates the file")
            .close()
            .await;
        assert!(path.exists());
        Database::<Sqlite>::connect(&plain)
            .await
            .expect("now it exists")
            .close()
            .await;
    }

    #[tokio::test]
    async fn memory_databases_share_one_pinned_connection() {
        let db = Database::<Sqlite>::connect(&SecretString::new("sqlite::memory:"))
            .await
            .expect("memory database");
        sqlx::query("CREATE TABLE t (id INTEGER)")
            .execute(db.pool())
            .await
            .expect("create");
        sqlx::query("INSERT INTO t VALUES (1)")
            .execute(db.pool())
            .await
            .expect("insert");
        let rows = sqlx::query("SELECT id FROM t")
            .fetch_all(db.pool())
            .await
            .expect("select");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get::<i64, _>(0), 1);
        assert_eq!(db.pool().options().get_max_connections(), 1);
    }

    #[tokio::test]
    async fn a_second_writer_gets_a_retryable_busy_error() {
        let test = TestDatabase::<Sqlite>::create(&SQLITE_MIGRATIONS)
            .await
            .expect("provision");
        let db = test.database();
        let mut holder = db.begin().await.expect("begin");
        sqlx::query("INSERT INTO owners (id, email, version) VALUES (?, 'a', 1)")
            .bind(Id::<Owner>::new())
            .execute(holder.connection())
            .await
            .expect("take the write lock");

        let mut waiter = db.begin().await.expect("begin");
        sqlx::query("PRAGMA busy_timeout = 100")
            .execute(waiter.connection())
            .await
            .expect("shorten the wait");
        let error = sqlx::query("INSERT INTO owners (id, email, version) VALUES (?, 'b', 1)")
            .bind(Id::<Owner>::new())
            .execute(waiter.connection())
            .await
            .map_err(DbError::from)
            .expect_err("database is busy");
        assert!(matches!(error, DbError::SerializationFailure), "{error:?}");
        assert!(error.is_retryable());
        drop(waiter);
        holder.commit().await.expect("commit");
        test.cleanup().await.expect("cleanup");
    }
}

mod postgres_specific {
    use super::*;
    use sqlx::Postgres;

    const IGNORE: &str = "needs FORGE_TEST_DATABASE_URL";

    #[tokio::test]
    #[ignore = "needs FORGE_TEST_DATABASE_URL"]
    async fn lost_updates_and_deadlocks_are_serialization_failures() {
        let test = TestDatabase::<Postgres>::create(&POSTGRES_MIGRATIONS)
            .await
            .expect(IGNORE);
        let db = test.database();
        let first = Id::<Owner>::new();
        let second = Id::<Owner>::new();
        for (id, email) in [(first, "a"), (second, "b")] {
            sqlx::query("INSERT INTO owners (id, email, version) VALUES ($1, $2, 1)")
                .bind(id)
                .bind(email)
                .execute(db.pool())
                .await
                .expect("insert");
        }

        // 40001: a repeatable-read transaction updating a row changed since its snapshot.
        let mut snapshot = db.begin().await.expect("begin");
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
            .execute(snapshot.connection())
            .await
            .expect("isolation");
        sqlx::query("SELECT version FROM owners WHERE id = $1")
            .bind(first)
            .fetch_one(snapshot.connection())
            .await
            .expect("take the snapshot");
        sqlx::query("UPDATE owners SET version = 2 WHERE id = $1")
            .bind(first)
            .execute(db.pool())
            .await
            .expect("concurrent update");
        let lost = sqlx::query("UPDATE owners SET version = 3 WHERE id = $1")
            .bind(first)
            .execute(snapshot.connection())
            .await
            .map_err(DbError::from)
            .expect_err("lost update");
        assert!(matches!(lost, DbError::SerializationFailure), "{lost:?}");
        drop(snapshot);

        // 40P01: two transactions locking the rows in opposite order.
        let (a, b) = deadlock(db, first, second).await;
        let failures = [a, b].into_iter().filter(|r| r.is_err()).count();
        assert_eq!(failures, 1, "exactly one deadlock victim");
        test.cleanup().await.expect("cleanup");
    }

    async fn cross(tx: &mut crate::DbTransaction<Postgres>, id: Id<Owner>) -> Result<(), DbError> {
        sqlx::query("UPDATE owners SET version = version + 1 WHERE id = $1")
            .bind(id)
            .execute(tx.connection())
            .await
            .map(|_| ())
            .map_err(DbError::from)
    }

    async fn deadlock(
        db: &Database<Postgres>,
        first: Id<Owner>,
        second: Id<Owner>,
    ) -> (Result<(), DbError>, Result<(), DbError>) {
        let touch = |id: Id<Owner>| async move {
            let mut tx = db.begin().await?;
            sqlx::query("UPDATE owners SET version = version + 1 WHERE id = $1")
                .bind(id)
                .execute(tx.connection())
                .await?;
            Ok::<_, DbError>(tx)
        };
        let mut tx_a = touch(first).await.expect("lock first");
        let mut tx_b = touch(second).await.expect("lock second");
        let (result_a, result_b) = tokio::join!(
            async {
                let result = cross(&mut tx_a, second).await;
                drop(tx_a);
                result
            },
            async {
                tokio::time::sleep(Duration::from_millis(300)).await;
                let result = cross(&mut tx_b, first).await;
                drop(tx_b);
                result
            }
        );
        for error in [&result_a, &result_b]
            .into_iter()
            .filter_map(|r| r.as_ref().err())
        {
            assert!(matches!(error, DbError::SerializationFailure), "{error:?}");
        }
        (result_a, result_b)
    }

    #[tokio::test]
    #[ignore = "needs FORGE_TEST_DATABASE_URL"]
    async fn advisory_transaction_locks_are_exclusive_and_released_with_the_transaction() {
        let test = TestDatabase::<Postgres>::create(&POSTGRES_MIGRATIONS)
            .await
            .expect(IGNORE);
        let db = test.database();
        let key = crate::advisory_key("forge-db:test");

        let mut holder = db.begin().await.expect("begin");
        holder.advisory_xact_lock(key).await.expect("lock");

        let mut other = db.begin().await.expect("begin");
        let taken: bool = sqlx::query_scalar("SELECT pg_try_advisory_xact_lock($1)")
            .bind(key)
            .fetch_one(other.connection())
            .await
            .expect("try lock");
        assert!(!taken, "the lock is held by another transaction");

        holder.commit().await.expect("commit releases the lock");
        let taken: bool = sqlx::query_scalar("SELECT pg_try_advisory_xact_lock($1)")
            .bind(key)
            .fetch_one(other.connection())
            .await
            .expect("try lock");
        assert!(taken);
        other.rollback().await.expect("rollback");
        test.cleanup().await.expect("cleanup");
    }

    #[tokio::test]
    #[ignore = "needs FORGE_TEST_DATABASE_URL"]
    async fn id_and_version_arrays_use_the_postgres_array_types() {
        let test = TestDatabase::<Postgres>::create_empty()
            .await
            .expect(IGNORE);
        let ids = vec![Id::<Owner>::new(), Id::new()];
        let read: Vec<Id<Owner>> = sqlx::query_scalar("SELECT $1::uuid[]")
            .bind(&ids)
            .fetch_one(test.database().pool())
            .await
            .expect("uuid[] round trip");
        assert_eq!(read, ids);
        let versions = vec![Version::INITIAL, Version::new(7).expect("version")];
        let read: Vec<Version> = sqlx::query_scalar("SELECT $1::bigint[]")
            .bind(&versions)
            .fetch_one(test.database().pool())
            .await
            .expect("bigint[] round trip");
        assert_eq!(read, versions);
        test.cleanup().await.expect("cleanup");
    }

    #[test]
    fn advisory_keys_are_stable() {
        assert_eq!(
            crate::advisory_key("forge-db:test"),
            crate::advisory_key("forge-db:test")
        );
        assert_ne!(crate::advisory_key("a"), crate::advisory_key("b"));
        // Pinned so the key never changes between releases.
        assert_eq!(crate::advisory_key(""), -3_750_763_034_362_895_579);
    }
}

/// MySQL-protocol behavior, run against MySQL and MariaDB.
macro_rules! mysql_specific {
    ($module:ident, $env:expr) => {
        mod $module {
            use super::*;
            use sqlx::MySql;

            async fn cross(
                tx: &mut crate::DbTransaction<MySql>,
                id: Id<Owner>,
            ) -> Result<(), DbError> {
                sqlx::query("UPDATE owners SET version = version + 1 WHERE id = ?")
                    .bind(id)
                    .execute(tx.connection())
                    .await
                    .map(|_| ())
                    .map_err(DbError::from)
            }

            #[tokio::test]
            #[ignore = "needs a MySQL/MariaDB test server"]
            async fn deadlocks_and_lock_wait_timeouts_are_serialization_failures() {
                let test = TestDatabase::<MySql>::create_from_env($env, &MYSQL_MIGRATIONS)
                    .await
                    .expect("provision");
                let db = test.database();
                let first = Id::<Owner>::new();
                let second = Id::<Owner>::new();
                for (id, email) in [(first, "a"), (second, "b")] {
                    sqlx::query("INSERT INTO owners (id, email, version) VALUES (?, ?, 1)")
                        .bind(id)
                        .bind(email)
                        .execute(db.pool())
                        .await
                        .expect("insert");
                }

                // 1205: waiting longer than innodb_lock_wait_timeout.
                let mut holder = db.begin().await.expect("begin");
                sqlx::query("UPDATE owners SET version = 2 WHERE id = ?")
                    .bind(first)
                    .execute(holder.connection())
                    .await
                    .expect("hold the row lock");
                let mut waiter = db.begin().await.expect("begin");
                sqlx::query("SET SESSION innodb_lock_wait_timeout = 1")
                    .execute(waiter.connection())
                    .await
                    .expect("shorten the wait");
                let timeout = sqlx::query("UPDATE owners SET version = 3 WHERE id = ?")
                    .bind(first)
                    .execute(waiter.connection())
                    .await
                    .map_err(DbError::from)
                    .expect_err("lock wait timeout");
                assert!(
                    matches!(timeout, DbError::SerializationFailure),
                    "{timeout:?}"
                );
                assert!(timeout.is_retryable());
                drop(waiter);
                holder.rollback().await.expect("rollback");

                // 1213: opposite lock order.
                let touch = |id: Id<Owner>| async move {
                    let mut tx = db.begin().await?;
                    sqlx::query("UPDATE owners SET version = version + 1 WHERE id = ?")
                        .bind(id)
                        .execute(tx.connection())
                        .await?;
                    Ok::<_, DbError>(tx)
                };
                let mut tx_a = touch(first).await.expect("lock first");
                let mut tx_b = touch(second).await.expect("lock second");
                let (result_a, result_b) = tokio::join!(
                    async {
                        let result = cross(&mut tx_a, second).await;
                        drop(tx_a);
                        result
                    },
                    async {
                        tokio::time::sleep(Duration::from_millis(300)).await;
                        let result = cross(&mut tx_b, first).await;
                        drop(tx_b);
                        result
                    }
                );
                let failures: Vec<_> = [&result_a, &result_b]
                    .into_iter()
                    .filter_map(|r| r.as_ref().err())
                    .collect();
                assert_eq!(failures.len(), 1, "exactly one deadlock victim");
                assert!(
                    matches!(failures[0], DbError::SerializationFailure),
                    "{failures:?}"
                );
                test.cleanup().await.expect("cleanup");
            }

            #[tokio::test]
            #[ignore = "needs a MySQL/MariaDB test server"]
            async fn mysql_and_mariadb_urls_are_both_accepted() {
                let test = TestDatabase::<MySql>::create_empty_from_env($env)
                    .await
                    .expect("provision");
                let url = test.url().expose().to_owned();
                let (_, rest) = url.split_once("://").expect("scheme");
                for scheme in ["mysql", "mariadb"] {
                    let db = Database::<MySql>::connect(&SecretString::new(format!(
                        "{scheme}://{rest}"
                    )))
                    .await
                    .expect("connect");
                    db.ping().await.expect("ping");
                    db.close().await;
                }
                test.cleanup().await.expect("cleanup");
            }
        }
    };
}

mysql_specific!(mysql_protocol, "FORGE_TEST_MYSQL_URL");
mysql_specific!(mariadb_protocol, "FORGE_TEST_MARIADB_URL");
