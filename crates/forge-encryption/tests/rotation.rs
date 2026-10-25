//! Key rotation: the re-encryption loop against an in-memory table, and the
//! SQLx `EncryptedColumn` adapters. SQLite always runs; PostgreSQL, MySQL and
//! MariaDB run with `--include-ignored` and their test URLs
//! (`FORGE_TEST_DATABASE_URL`, `FORGE_TEST_MYSQL_URL`, `FORGE_TEST_MARIADB_URL`).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Mutex;

use async_trait::async_trait;
use forge_core::Cancellation;
use forge_encryption::{
    Aad, BoxError, Cipher, EncryptionError, Key, KeyRing, Record, ReencryptFailure, Rewriter,
    RotateOptions, Update, generate_key,
};

const TABLE: &str = "people";
const COLUMN: &str = "ssn";

fn key(id: &str) -> Key {
    Key::new(id, generate_key().unwrap()).unwrap()
}

/// The old ring, and the new one with the old key kept for decryption.
fn rings() -> (KeyRing, KeyRing, KeyRing) {
    let old = key("2025-03");
    let new = key("2026-10");
    (
        KeyRing::new(old.clone(), []).unwrap(),
        KeyRing::new(new.clone(), [old]).unwrap(),
        KeyRing::new(new, []).unwrap(),
    )
}

fn row_id(n: u32) -> String {
    format!("0199f5c4-3a52-7d1e-9b7e-{n:012x}")
}

#[derive(Default)]
struct MemoryTable {
    rows: Mutex<Vec<(String, String, Option<String>)>>,
    /// Applied to the row's value just before `replace` runs, to simulate a
    /// concurrent application write.
    concurrent: Mutex<Option<(String, String)>>,
}

#[async_trait]
impl Rewriter for MemoryTable {
    fn table(&self) -> &str {
        TABLE
    }

    fn column(&self) -> &str {
        COLUMN
    }

    async fn page(&self, after: Option<&str>, limit: u32) -> Result<Vec<Record>, BoxError> {
        let rows = self.rows.lock().unwrap();
        Ok(rows
            .iter()
            .filter(|(id, _, _)| after.is_none_or(|after| id.as_str() > after))
            .take(limit as usize)
            .map(|(id, sealed, _)| Record {
                id: id.clone(),
                sealed: sealed.clone(),
            })
            .collect())
    }

    async fn replace(&self, update: &Update) -> Result<bool, BoxError> {
        if let Some((id, value)) = self.concurrent.lock().unwrap().take() {
            let mut rows = self.rows.lock().unwrap();
            if let Some(row) = rows.iter_mut().find(|row| row.0 == id) {
                row.1 = value;
            }
        }
        let mut rows = self.rows.lock().unwrap();
        let Some(row) = rows
            .iter_mut()
            .find(|row| row.0 == update.id && row.1 == update.old_sealed)
        else {
            return Ok(false);
        };
        row.1 = update.new_sealed.clone();
        if let Some(index) = &update.index {
            row.2 = Some(index.clone());
        }
        Ok(true)
    }
}

fn seeded(cipher: &Cipher, count: u32) -> MemoryTable {
    let table = MemoryTable::default();
    *table.rows.lock().unwrap() = (0..count)
        .map(|n| {
            let id = row_id(n);
            let sealed = cipher
                .encrypt(format!("ssn-{n}").as_bytes(), &Aad::new(TABLE, COLUMN, &id))
                .unwrap();
            (id, sealed, None)
        })
        .collect();
    table
}

#[tokio::test]
async fn rotation_keeps_old_values_readable_then_rewrites_them() {
    let (old_ring, rotating_ring, new_only) = rings();
    let old = Cipher::new(&old_ring).unwrap();
    let table = seeded(&old, 25);
    let rotating = Cipher::new(&rotating_ring).unwrap();
    // Before the rewrite, the new ring reads every old value.
    for (id, sealed, _) in table.rows.lock().unwrap().iter() {
        assert!(
            rotating
                .decrypt(sealed, &Aad::new(TABLE, COLUMN, id))
                .is_ok()
        );
    }
    let options = RotateOptions {
        batch_size: 10,
        blind_index: true,
        ..RotateOptions::default()
    };
    let report = rotating
        .reencrypt(&table, options.clone(), &Cancellation::new())
        .await
        .unwrap();
    assert_eq!(
        (
            report.scanned,
            report.rotated,
            report.reindexed,
            report.conflicts
        ),
        (25, 25, 25, 0)
    );
    assert_eq!(report.last_id, Some(row_id(24)));
    // Afterwards the old key can go: the new key alone reads everything.
    let new_cipher = Cipher::new(&new_only).unwrap();
    for (n, (id, sealed, index)) in table.rows.lock().unwrap().iter().enumerate() {
        let aad = Aad::new(TABLE, COLUMN, id);
        assert_eq!(
            &*new_cipher.decrypt(sealed, &aad).unwrap(),
            format!("ssn-{n}").as_bytes()
        );
        assert_eq!(
            index.as_deref(),
            Some(
                new_cipher
                    .blind_index(TABLE, COLUMN, format!("ssn-{n}").as_bytes())
                    .unwrap()
                    .as_str()
            )
        );
        assert!(matches!(
            old.decrypt(sealed, &aad),
            Err(EncryptionError::UnknownKey(_))
        ));
    }
    // A second run finds nothing to rotate.
    let again = rotating
        .reencrypt(&table, RotateOptions::default(), &Cancellation::new())
        .await
        .unwrap();
    assert_eq!((again.scanned, again.rotated, again.skipped), (25, 0, 25));
}

#[tokio::test]
async fn concurrent_writes_win_and_runs_resume_after_cancellation() {
    let (old_ring, rotating_ring, _) = rings();
    let old = Cipher::new(&old_ring).unwrap();
    let table = seeded(&old, 5);
    let rotating = Cipher::new(&rotating_ring).unwrap();
    let fresh = rotating
        .encrypt(b"written by the app", &Aad::new(TABLE, COLUMN, &row_id(0)))
        .unwrap();
    *table.concurrent.lock().unwrap() = Some((row_id(0), fresh.clone()));
    let report = rotating
        .reencrypt(&table, RotateOptions::default(), &Cancellation::new())
        .await
        .unwrap();
    assert_eq!((report.rotated, report.conflicts), (4, 1));
    assert_eq!(
        table.rows.lock().unwrap()[0].1,
        fresh,
        "the application's write is kept"
    );

    let cancelled = Cancellation::new();
    cancelled.cancel();
    let error = rotating
        .reencrypt(&table, RotateOptions::default(), &cancelled)
        .await
        .unwrap_err();
    assert!(matches!(error.failure, ReencryptFailure::Cancelled));
    let resumed = rotating
        .reencrypt(
            &table,
            RotateOptions {
                after: Some(row_id(2)),
                ..RotateOptions::default()
            },
            &Cancellation::new(),
        )
        .await
        .unwrap();
    assert_eq!(resumed.scanned, 2, "rows up to the checkpoint are skipped");
    assert!(
        rotating
            .reencrypt(
                &table,
                RotateOptions {
                    batch_size: 10_001,
                    ..RotateOptions::default()
                },
                &Cancellation::new()
            )
            .await
            .is_err()
    );
}

#[tokio::test]
async fn a_value_swapped_between_rows_stops_the_rotation_without_leaking_it() {
    let (old_ring, rotating_ring, _) = rings();
    let old = Cipher::new(&old_ring).unwrap();
    let table = seeded(&old, 3);
    {
        let mut rows = table.rows.lock().unwrap();
        let first = rows[0].1.clone();
        rows[1].1 = first;
    }
    let rotating = Cipher::new(&rotating_ring).unwrap();
    let error = rotating
        .reencrypt(&table, RotateOptions::default(), &Cancellation::new())
        .await
        .unwrap_err();
    assert!(matches!(&error.failure, ReencryptFailure::Row { id, .. } if *id == row_id(1)));
    assert_eq!(error.report.rotated, 1);
    assert_eq!(error.report.last_id, Some(row_id(0)));
    let text = format!("{error} {error:?}");
    assert!(!text.contains("ssn-"), "{text}");
}

/// A rewriter that returns its checkpoint again would loop forever.
struct Stuck;

#[async_trait]
impl Rewriter for Stuck {
    fn table(&self) -> &str {
        TABLE
    }

    fn column(&self) -> &str {
        COLUMN
    }

    async fn page(&self, _after: Option<&str>, _limit: u32) -> Result<Vec<Record>, BoxError> {
        Ok(vec![Record {
            id: "same".to_owned(),
            sealed: "fg1.k1.AAAA".to_owned(),
        }])
    }

    async fn replace(&self, _update: &Update) -> Result<bool, BoxError> {
        Ok(false)
    }
}

#[tokio::test]
async fn a_rewriter_that_does_not_advance_is_refused() {
    let cipher = Cipher::new(&KeyRing::new(key("k1"), []).unwrap()).unwrap();
    let error = cipher
        .reencrypt(
            &Stuck,
            RotateOptions {
                batch_size: 1,
                after: Some("same".to_owned()),
                ..RotateOptions::default()
            },
            &Cancellation::new(),
        )
        .await
        .unwrap_err();
    assert!(matches!(error.failure, ReencryptFailure::NoProgress(_)));
}

macro_rules! sql_rotation {
    ($module:ident, $db:ty, $adapter:path, $env:expr, $create:expr, $insert:expr, $read:expr; $(#[$attr:meta])*) => {
        mod $module {
            use super::*;
            use forge_db::testing::TestDatabase;
            use forge_encryption::SqlColumn;
            use sqlx::{AssertSqlSafe, Row as _};
            use uuid::Uuid;

            #[tokio::test]
            $(#[$attr])*
            async fn encrypted_columns_rotate_in_place() {
                let test = TestDatabase::<$db>::create_empty_from_env($env).await.expect("database");
                let pool = test.database().pool().clone();
                sqlx::query(AssertSqlSafe($create)).execute(&pool).await.expect("table");
                let (old_ring, rotating_ring, new_only) = rings();
                let old = Cipher::new(&old_ring).unwrap();
                let mut ids = Vec::new();
                for n in 0..7_u32 {
                    let id = Uuid::now_v7();
                    let sealed = old
                        .attribute(TABLE, COLUMN)
                        .seal(&id.to_string(), &format!("ssn-{n}"))
                        .unwrap();
                    sqlx::query(AssertSqlSafe($insert))
                        .bind(id)
                        .bind(sealed)
                        .execute(&pool)
                        .await
                        .expect("insert");
                    ids.push(id);
                }
                // A NULL value is skipped.
                sqlx::query(AssertSqlSafe($insert))
                    .bind(Uuid::now_v7())
                    .bind(Option::<String>::None)
                    .execute(&pool)
                    .await
                    .expect("insert null");

                let spec = SqlColumn::new(TABLE, COLUMN).unwrap().with_index_column("ssn_idx").unwrap();
                let column = <$adapter>::new(test.database().clone(), spec).unwrap();
                let rotating = Cipher::new(&rotating_ring).unwrap();
                let report = rotating
                    .reencrypt(
                        &column,
                        RotateOptions { batch_size: 3, blind_index: true, ..RotateOptions::default() },
                        &Cancellation::new(),
                    )
                    .await
                    .expect("rotate");
                assert_eq!((report.scanned, report.rotated, report.reindexed, report.conflicts), (7, 7, 7, 0));
                assert_eq!(report.last_id, ids.iter().max().map(Uuid::to_string));

                let new_cipher = Cipher::new(&new_only).unwrap();
                let rows = sqlx::query(AssertSqlSafe($read)).fetch_all(&pool).await.expect("read");
                assert_eq!(rows.len(), 7);
                for row in rows {
                    let id: Uuid = row.try_get(0).unwrap();
                    let sealed: String = row.try_get(1).unwrap();
                    let index: String = row.try_get(2).unwrap();
                    let n = ids.iter().position(|known| *known == id).expect("known row");
                    let plain: String = new_cipher.attribute(TABLE, COLUMN).open(&id.to_string(), &sealed).unwrap();
                    assert_eq!(plain, format!("ssn-{n}"));
                    assert_eq!(index, new_cipher.blind_index(TABLE, COLUMN, plain.as_bytes()).unwrap());
                }

                // A ciphertext copied to another row fails and names that row.
                let rows = sqlx::query(AssertSqlSafe($read)).fetch_all(&pool).await.expect("read");
                let donor: String = rows[0].try_get(1).unwrap();
                let victim: Uuid = rows[1].try_get(0).unwrap();
                let swap = format!("UPDATE people SET ssn = {} WHERE id = {}", placeholder(stringify!($module), 1), placeholder(stringify!($module), 2));
                sqlx::query(AssertSqlSafe(swap)).bind(donor).bind(victim).execute(&pool).await.expect("swap");
                let error = new_cipher
                    .reencrypt(&column, RotateOptions { blind_index: true, ..RotateOptions::default() }, &Cancellation::new())
                    .await
                    .expect_err("swapped row");
                assert!(matches!(&error.failure, ReencryptFailure::Row { id, .. } if *id == victim.to_string()), "{error}");
                test.cleanup().await.expect("cleanup");
            }
        }
    };
}

fn placeholder(backend: &str, position: usize) -> String {
    if backend == "postgres" {
        format!("${position}")
    } else {
        "?".to_owned()
    }
}

sql_rotation!(
    sqlite, sqlx::Sqlite, forge_encryption::sqlite::EncryptedColumn, "",
    "CREATE TABLE people (id BLOB NOT NULL PRIMARY KEY, ssn TEXT, ssn_idx TEXT)",
    "INSERT INTO people (id, ssn) VALUES (?, ?)",
    "SELECT id, ssn, ssn_idx FROM people WHERE ssn IS NOT NULL ORDER BY id";
);
sql_rotation!(
    postgres, sqlx::Postgres, forge_encryption::postgres::EncryptedColumn, "FORGE_TEST_DATABASE_URL",
    "CREATE TABLE people (id uuid NOT NULL PRIMARY KEY, ssn text, ssn_idx text)",
    "INSERT INTO people (id, ssn) VALUES ($1, $2)",
    "SELECT id, ssn, ssn_idx FROM people WHERE ssn IS NOT NULL ORDER BY id";
    #[ignore = "needs FORGE_TEST_DATABASE_URL"]
);
sql_rotation!(
    mysql, sqlx::MySql, forge_encryption::mysql::EncryptedColumn, "FORGE_TEST_MYSQL_URL",
    "CREATE TABLE people (id BINARY(16) NOT NULL PRIMARY KEY, ssn TEXT, ssn_idx VARCHAR(64))",
    "INSERT INTO people (id, ssn) VALUES (?, ?)",
    "SELECT id, ssn, ssn_idx FROM people WHERE ssn IS NOT NULL ORDER BY id";
    #[ignore = "needs FORGE_TEST_MYSQL_URL"]
);

/// Row-level security: the generated tenant tables (`ENABLE` and `FORCE ROW
/// LEVEL SECURITY` on `forge.tenant_id`) hide every row from an unscoped
/// NOBYPASSRLS role, so an unscoped rotation must be refused instead of
/// reporting an empty success; a tenant-scoped one rotates that tenant only,
/// and the count of rows not on the active key proves completion.
mod postgres_row_level_security {
    use super::*;
    use forge_db::PgDatabase;
    use forge_encryption::SqlColumn;
    use forge_encryption::postgres::EncryptedColumn;
    use forge_jobs::testing::RlsTestDatabase;
    use forge_tenancy::TenantId;
    use sqlx::AssertSqlSafe;
    use uuid::Uuid;

    const ROWS_PER_TENANT: u32 = 5;
    const STALE: &str = "SELECT count(*) FROM people \
                         WHERE ssn IS NOT NULL AND substr(ssn, 1, length($1)) <> $1";

    /// Rows of `people` not sealed with the active key, as `database` sees
    /// them, optionally inside one tenant's scope.
    async fn stale(database: &PgDatabase, tenant: Option<TenantId>, active: &str) -> i64 {
        let mut transaction = database.begin().await.expect("begin");
        if let Some(tenant) = tenant {
            transaction.set_tenant(tenant).await.expect("scope");
        }
        let count = sqlx::query_scalar(STALE)
            .bind(format!("fg1.{active}."))
            .fetch_one(transaction.connection())
            .await
            .expect("count");
        transaction.rollback().await.expect("rollback");
        count
    }

    async fn values(database: &PgDatabase, tenant: TenantId) -> Vec<(Uuid, String)> {
        sqlx::query_as("SELECT id, ssn FROM people WHERE tenant_id = $1 ORDER BY id")
            .bind(tenant.into_uuid())
            .fetch_all(database.pool())
            .await
            .expect("values")
    }

    fn column(database: &PgDatabase) -> EncryptedColumn {
        let spec = SqlColumn::new(TABLE, COLUMN)
            .unwrap()
            .with_index_column("ssn_idx")
            .unwrap();
        EncryptedColumn::new(database.clone(), spec).unwrap()
    }

    /// Runs `statement` as the superuser.
    async fn admin(db: &RlsTestDatabase, statement: String) {
        sqlx::query(AssertSqlSafe(statement))
            .execute(db.admin().pool())
            .await
            .expect("statement");
    }

    /// An unscoped rotation of `people` to `cipher`'s active key as
    /// `database`.
    async fn unscoped(
        cipher: &Cipher,
        database: &PgDatabase,
    ) -> Result<forge_encryption::Report, forge_encryption::ReencryptError> {
        cipher
            .reencrypt(
                &column(database),
                RotateOptions::default(),
                &Cancellation::new(),
            )
            .await
    }

    /// The roles row-level security does not bind (a superuser, a
    /// `BYPASSRLS` role, the owner of a table without `FORCE`) rotate a
    /// tenant table unscoped and see every row; the same roles without the
    /// exemption are refused again.
    #[tokio::test]
    #[ignore = "needs FORGE_TEST_DATABASE_URL"]
    async fn exempt_roles_rotate_tenant_tables_unscoped() {
        let db = RlsTestDatabase::create(&[]).await.expect("database");
        let runtime_role = db.runtime_role().to_owned();
        for statement in [
            "CREATE TABLE people (id uuid PRIMARY KEY, tenant_id uuid NOT NULL, \
             ssn text, ssn_idx text)"
                .to_owned(),
            "ALTER TABLE people ENABLE ROW LEVEL SECURITY".to_owned(),
            "ALTER TABLE people FORCE ROW LEVEL SECURITY".to_owned(),
            "CREATE POLICY people_tenant_isolation ON people \
             USING (tenant_id = NULLIF(current_setting('forge.tenant_id', true), '')::uuid)"
                .to_owned(),
            format!("GRANT SELECT, UPDATE ON people TO \"{runtime_role}\""),
        ] {
            sqlx::query(AssertSqlSafe(statement))
                .execute(db.migrator().pool())
                .await
                .expect("schema");
        }
        let (first, second, third, fourth) = (key("k1"), key("k2"), key("k3"), key("k4"));
        let old = Cipher::new(&KeyRing::new(first.clone(), []).unwrap()).unwrap();
        for tenant in [TenantId::new(), TenantId::new()] {
            for n in 0..ROWS_PER_TENANT {
                let id = Uuid::now_v7();
                let sealed = old
                    .attribute(TABLE, COLUMN)
                    .seal(&id.to_string(), &format!("ssn-{n}"))
                    .unwrap();
                sqlx::query("INSERT INTO people (id, tenant_id, ssn) VALUES ($1, $2, $3)")
                    .bind(id)
                    .bind(tenant.into_uuid())
                    .bind(sealed)
                    .execute(db.admin().pool())
                    .await
                    .expect("seed");
            }
        }
        let all_rows = u64::from(2 * ROWS_PER_TENANT);
        let previous = [first.clone(), second.clone(), third.clone(), fourth.clone()];
        let ring = |active: &Key| {
            let others = previous
                .iter()
                .filter(|key| key.id() != active.id())
                .cloned();
            Cipher::new(&KeyRing::new(active.clone(), others).unwrap()).unwrap()
        };

        // A superuser.
        let to_second = ring(&second);
        let report = unscoped(&to_second, db.admin()).await.expect("superuser");
        assert_eq!((report.scanned, report.rotated), (all_rows, all_rows));
        assert_eq!(stale(db.admin(), None, "k2").await, 0);

        // A BYPASSRLS role; refused again once the attribute is gone.
        let to_third = ring(&third);
        assert!(unscoped(&to_third, db.runtime()).await.is_err());
        admin(&db, format!("ALTER ROLE \"{runtime_role}\" BYPASSRLS")).await;
        let report = unscoped(&to_third, db.runtime()).await.expect("BYPASSRLS");
        assert_eq!((report.scanned, report.rotated), (all_rows, all_rows));
        assert_eq!(stale(db.admin(), None, "k3").await, 0);
        admin(&db, format!("ALTER ROLE \"{runtime_role}\" NOBYPASSRLS")).await;
        let error = unscoped(&to_third, db.runtime())
            .await
            .expect_err("NOBYPASSRLS again");
        assert!(error.to_string().contains("row-level security"), "{error}");

        // The owner of a table without FORCE; other roles stay bound.
        let to_fourth = ring(&fourth);
        assert!(unscoped(&to_fourth, db.migrator()).await.is_err());
        sqlx::query("ALTER TABLE people NO FORCE ROW LEVEL SECURITY")
            .execute(db.migrator().pool())
            .await
            .expect("no force");
        let error = unscoped(&to_fourth, db.runtime())
            .await
            .expect_err("ENABLE still binds a non-owner");
        assert_eq!(error.report, Default::default());
        let report = unscoped(&to_fourth, db.migrator()).await.expect("owner");
        assert_eq!((report.scanned, report.rotated), (all_rows, all_rows));
        assert_eq!(stale(db.admin(), None, "k4").await, 0);
        db.cleanup().await.expect("cleanup");
    }

    #[tokio::test]
    #[ignore = "needs FORGE_TEST_DATABASE_URL"]
    async fn tenant_tables_rotate_per_tenant_and_refuse_unscoped_runs() {
        let db = RlsTestDatabase::create(&[]).await.expect("database");
        let runtime_role = db.runtime_role().to_owned();
        for statement in [
            "CREATE TABLE people (id uuid PRIMARY KEY, tenant_id uuid NOT NULL, \
             ssn text, ssn_idx text)"
                .to_owned(),
            "ALTER TABLE people ENABLE ROW LEVEL SECURITY".to_owned(),
            "ALTER TABLE people FORCE ROW LEVEL SECURITY".to_owned(),
            "CREATE POLICY people_tenant_isolation ON people \
             USING (tenant_id = NULLIF(current_setting('forge.tenant_id', true), '')::uuid) \
             WITH CHECK (tenant_id = NULLIF(current_setting('forge.tenant_id', true), '')::uuid)"
                .to_owned(),
            format!("GRANT SELECT, INSERT, UPDATE ON people TO \"{runtime_role}\""),
            "CREATE VIEW people_view AS SELECT * FROM people".to_owned(),
            format!("GRANT SELECT, UPDATE ON people_view TO \"{runtime_role}\""),
        ] {
            sqlx::query(AssertSqlSafe(statement))
                .execute(db.migrator().pool())
                .await
                .expect("schema");
        }
        let (old_ring, rotating_ring, new_only) = rings();
        let old = Cipher::new(&old_ring).unwrap();
        let (first, second) = (TenantId::new(), TenantId::new());
        for tenant in [first, second] {
            for n in 0..ROWS_PER_TENANT {
                let id = Uuid::now_v7();
                let sealed = old
                    .attribute(TABLE, COLUMN)
                    .seal(&id.to_string(), &format!("ssn-{n}"))
                    .unwrap();
                // The superuser bypasses row-level security.
                sqlx::query("INSERT INTO people (id, tenant_id, ssn) VALUES ($1, $2, $3)")
                    .bind(id)
                    .bind(tenant.into_uuid())
                    .bind(sealed)
                    .execute(db.admin().pool())
                    .await
                    .expect("seed");
            }
        }
        let rotating = Cipher::new(&rotating_ring).unwrap();
        let active = rotating.active_key_id().to_owned();
        let all_rows = i64::from(2 * ROWS_PER_TENANT);
        assert_eq!(stale(db.admin(), None, &active).await, all_rows);
        // What the old adapter scanned: the runtime role sees no row at all.
        assert_eq!(stale(db.runtime(), None, &active).await, 0);

        // 1. Without a tenant scope the rotation is refused, for the runtime
        //    role and for the owner (FORCE binds it too), before any write.
        for database in [db.runtime(), db.migrator()] {
            let error = rotating
                .reencrypt(
                    &column(database),
                    RotateOptions::default(),
                    &Cancellation::new(),
                )
                .await
                .expect_err("unscoped rotation of a tenant table");
            let ReencryptFailure::Page { source, .. } = &error.failure else {
                panic!("unexpected failure {error}");
            };
            let configuration = source
                .downcast_ref::<EncryptionError>()
                .expect("an encryption error");
            assert!(
                matches!(configuration, EncryptionError::Configuration(message)
                    if message.contains("row-level security") && message.contains(".tenant(")),
                "{configuration}"
            );
            assert_eq!(error.report, Default::default());
        }
        assert_eq!(stale(db.admin(), None, &active).await, all_rows);

        // A view carries no row-level security flags of its own while the
        // table under it hides every row: it is refused, not scanned empty.
        let view = EncryptedColumn::new(
            db.runtime().clone(),
            SqlColumn::new("people_view", COLUMN).unwrap(),
        )
        .unwrap();
        let error = rotating
            .reencrypt(&view, RotateOptions::default(), &Cancellation::new())
            .await
            .expect_err("unscoped rotation of a view");
        assert!(error.to_string().contains("not a table"), "{error}");
        assert_eq!(error.report, Default::default());

        // 2. Scoped to the first tenant: its rows rotate, the second
        //    tenant's rows are untouched.
        let before = values(db.admin(), second).await;
        let report = rotating
            .reencrypt(
                &column(db.runtime()).tenant(first),
                RotateOptions {
                    batch_size: 2,
                    blind_index: true,
                    ..RotateOptions::default()
                },
                &Cancellation::new(),
            )
            .await
            .expect("rotate the first tenant");
        let rows = u64::from(ROWS_PER_TENANT);
        assert_eq!(
            (
                report.scanned,
                report.rotated,
                report.reindexed,
                report.conflicts
            ),
            (rows, rows, rows, 0)
        );
        assert_eq!(values(db.admin(), second).await, before);

        // 3. The count of rows not on the active key proves completion, per
        //    tenant or as a role that sees every row; `scanned` never does.
        assert_eq!(stale(db.runtime(), Some(first), &active).await, 0);
        assert_eq!(
            stale(db.runtime(), Some(second), &active).await,
            i64::from(ROWS_PER_TENANT)
        );
        assert_eq!(
            stale(db.admin(), None, &active).await,
            i64::from(ROWS_PER_TENANT)
        );
        let report = rotating
            .reencrypt(
                &column(db.runtime()).tenant(second),
                RotateOptions::default(),
                &Cancellation::new(),
            )
            .await
            .expect("rotate the second tenant");
        assert_eq!((report.scanned, report.rotated), (rows, rows));
        for tenant in [first, second] {
            assert_eq!(stale(db.runtime(), Some(tenant), &active).await, 0);
        }
        assert_eq!(stale(db.admin(), None, &active).await, 0);

        // Only now may the old key go: the new key alone reads every row.
        let new_cipher = Cipher::new(&new_only).unwrap();
        for tenant in [first, second] {
            for (id, sealed) in values(db.admin(), tenant).await {
                let plain: String = new_cipher
                    .attribute(TABLE, COLUMN)
                    .open(&id.to_string(), &sealed)
                    .unwrap();
                assert!(plain.starts_with("ssn-"));
            }
        }
        db.cleanup().await.expect("cleanup");
    }
}
