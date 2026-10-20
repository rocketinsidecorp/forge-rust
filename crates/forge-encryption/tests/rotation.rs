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
