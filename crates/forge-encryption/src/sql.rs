//! The SQL side of a rotation: statements for one encrypted TEXT column and
//! the SQLx `EncryptedColumn` adapters of [`Rewriter`](crate::Rewriter).

use crate::EncryptionError;

/// How the primary key is stored. The record id of the associated data is its
/// text form: the canonical hyphenated UUID, the text itself or the decimal
/// integer.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub enum IdType {
    /// A UUID (UUIDv7 in Forge applications): `uuid` on PostgreSQL,
    /// `BINARY(16)` on MySQL/MariaDB, `BLOB` on SQLite, as `forge generate`
    /// creates them.
    #[default]
    Uuid,
    /// A text key.
    Text,
    /// A `BIGINT` key.
    Integer,
}

/// The placeholder style of the generated statements.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Placeholders {
    /// `$1, $2` (PostgreSQL).
    Dollar,
    /// `?` (MySQL, MariaDB, SQLite).
    Question,
}

/// One encrypted TEXT column and the statements a rewriter needs.
/// Identifiers are validated (lowercase ASCII letters, digits and
/// underscores, 1 to 63 characters, not starting with a digit): they come
/// from the application's source, never from user input.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SqlColumn {
    table: String,
    column: String,
    id_column: String,
    id_type: IdType,
    index_column: Option<String>,
}

impl SqlColumn {
    /// The column `table.column`, keyed by a UUID column `id`.
    pub fn new(
        table: impl Into<String>,
        column: impl Into<String>,
    ) -> Result<Self, EncryptionError> {
        let spec = Self {
            table: table.into(),
            column: column.into(),
            id_column: "id".to_owned(),
            id_type: IdType::Uuid,
            index_column: None,
        };
        spec.validate()?;
        Ok(spec)
    }

    /// Another primary-key column.
    pub fn with_id_column(mut self, id_column: impl Into<String>) -> Result<Self, EncryptionError> {
        self.id_column = id_column.into();
        self.validate()?;
        Ok(self)
    }

    /// Another primary-key type.
    #[must_use]
    pub fn with_id_type(mut self, id_type: IdType) -> Self {
        self.id_type = id_type;
        self
    }

    /// The column receiving the blind index when
    /// [`RotateOptions::blind_index`](crate::RotateOptions::blind_index) is set.
    pub fn with_index_column(
        mut self,
        index_column: impl Into<String>,
    ) -> Result<Self, EncryptionError> {
        self.index_column = Some(index_column.into());
        self.validate()?;
        Ok(self)
    }

    /// The table name.
    #[must_use]
    pub fn table(&self) -> &str {
        &self.table
    }

    /// The encrypted column.
    #[must_use]
    pub fn column(&self) -> &str {
        &self.column
    }

    /// The primary-key type.
    #[must_use]
    pub fn id_type(&self) -> IdType {
        self.id_type
    }

    fn validate(&self) -> Result<(), EncryptionError> {
        let names = [&self.table, &self.column, &self.id_column]
            .into_iter()
            .chain(self.index_column.as_ref());
        for name in names {
            if !valid_identifier(name) {
                return Err(EncryptionError::Configuration(format!(
                    "invalid SQL identifier {name:?}"
                )));
            }
        }
        Ok(())
    }

    /// The statement behind [`Rewriter::page`](crate::Rewriter::page). With
    /// `after` it takes one argument, the id after which to read.
    #[must_use]
    pub fn page_sql(&self, limit: u32, after: bool, style: Placeholders) -> String {
        let mut condition = format!("{} IS NOT NULL", self.column);
        if after {
            condition = format!(
                "{} > {} AND {condition}",
                self.id_column,
                placeholder(style, 1)
            );
        }
        format!(
            "SELECT {}, {} FROM {} WHERE {condition} ORDER BY {} LIMIT {limit}",
            self.id_column, self.column, self.table, self.id_column
        )
    }

    /// The statement behind [`Rewriter::replace`](crate::Rewriter::replace),
    /// a compare-and-set on the old ciphertext. Its arguments are, in order:
    /// the new ciphertext, the blind index (only `with_index`), the id and the
    /// old ciphertext.
    pub fn replace_sql(
        &self,
        with_index: bool,
        style: Placeholders,
    ) -> Result<String, EncryptionError> {
        let mut set = format!("{} = {}", self.column, placeholder(style, 1));
        let mut next = 2;
        if with_index {
            let Some(index) = &self.index_column else {
                return Err(EncryptionError::Configuration(
                    "a blind index was requested but no index column is configured".to_owned(),
                ));
            };
            set = format!("{set}, {index} = {}", placeholder(style, next));
            next += 1;
        }
        Ok(format!(
            "UPDATE {} SET {set} WHERE {} = {} AND {} = {}",
            self.table,
            self.id_column,
            placeholder(style, next),
            self.column,
            placeholder(style, next + 1)
        ))
    }
}

fn placeholder(style: Placeholders, position: usize) -> String {
    match style {
        Placeholders::Dollar => format!("${position}"),
        Placeholders::Question => "?".to_owned(),
    }
}

fn valid_identifier(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 63
        && (bytes[0].is_ascii_lowercase() || bytes[0] == b'_')
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'_')
}

/// The SQLx [`Rewriter`](crate::Rewriter) of one backend.
#[cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]
macro_rules! encrypted_column {
    ($module:ident, $feature:literal, $database:ty, $style:expr, $doc:literal) => {
        #[cfg(feature = $feature)]
        #[doc = $doc]
        pub mod $module {
            use async_trait::async_trait;
            use forge_db::Database;
            use sqlx::{AssertSqlSafe, Row as _};
            use uuid::Uuid;

            use crate::{BoxError, EncryptionError, IdType, Record, Rewriter, SqlColumn, Update};

            /// The rewriter of one encrypted column, for
            /// [`Cipher::reencrypt`](crate::Cipher::reencrypt).
            #[derive(Debug)]
            pub struct EncryptedColumn {
                db: Database<$database>,
                spec: SqlColumn,
            }

            impl EncryptedColumn {
                /// The rewriter of `spec` on `db`.
                pub fn new(
                    db: Database<$database>,
                    spec: SqlColumn,
                ) -> Result<Self, EncryptionError> {
                    spec.validate()?;
                    Ok(Self { db, spec })
                }

                fn bind_id<'q>(
                    &self,
                    query: sqlx::query::Query<
                        'q,
                        $database,
                        <$database as sqlx::Database>::Arguments,
                    >,
                    id: &str,
                ) -> Result<
                    sqlx::query::Query<'q, $database, <$database as sqlx::Database>::Arguments>,
                    BoxError,
                > {
                    Ok(match self.spec.id_type {
                        IdType::Uuid => query.bind(Uuid::parse_str(id).map_err(|_| {
                            EncryptionError::Configuration("the row id is not a UUID".to_owned())
                        })?),
                        IdType::Text => query.bind(id.to_owned()),
                        IdType::Integer => query.bind(id.parse::<i64>().map_err(|_| {
                            EncryptionError::Configuration(
                                "the row id is not an integer".to_owned(),
                            )
                        })?),
                    })
                }
            }

            #[async_trait]
            impl Rewriter for EncryptedColumn {
                fn table(&self) -> &str {
                    self.spec.table()
                }

                fn column(&self) -> &str {
                    self.spec.column()
                }

                async fn page(
                    &self,
                    after: Option<&str>,
                    limit: u32,
                ) -> Result<Vec<Record>, BoxError> {
                    let sql = self.spec.page_sql(limit, after.is_some(), $style);
                    let mut query = sqlx::query(AssertSqlSafe(sql));
                    if let Some(after) = after {
                        query = self.bind_id(query, after)?;
                    }
                    let rows = query.fetch_all(self.db.pool()).await?;
                    rows.iter()
                        .map(|row| {
                            let id = match self.spec.id_type {
                                IdType::Uuid => row.try_get::<Uuid, _>(0)?.to_string(),
                                IdType::Text => row.try_get::<String, _>(0)?,
                                IdType::Integer => row.try_get::<i64, _>(0)?.to_string(),
                            };
                            Ok(Record {
                                id,
                                sealed: row.try_get::<String, _>(1)?,
                            })
                        })
                        .collect()
                }

                async fn replace(&self, update: &Update) -> Result<bool, BoxError> {
                    let sql = self.spec.replace_sql(update.index.is_some(), $style)?;
                    let mut query = sqlx::query(AssertSqlSafe(sql)).bind(update.new_sealed.clone());
                    if let Some(index) = &update.index {
                        query = query.bind(index.clone());
                    }
                    let query = self
                        .bind_id(query, &update.id)?
                        .bind(update.old_sealed.clone());
                    let result = query.execute(self.db.pool()).await?;
                    Ok(result.rows_affected() > 0)
                }
            }
        }
    };
}

#[cfg(feature = "postgres")]
encrypted_column!(
    postgres,
    "postgres",
    sqlx::Postgres,
    super::Placeholders::Dollar,
    "PostgreSQL re-encryption adapter (`uuid`, text or `bigint` keys)."
);
#[cfg(feature = "mysql")]
encrypted_column!(
    mysql,
    "mysql",
    sqlx::MySql,
    super::Placeholders::Question,
    "MySQL and MariaDB re-encryption adapter (`BINARY(16)` UUID, text or `BIGINT` keys)."
);
#[cfg(feature = "sqlite")]
encrypted_column!(
    sqlite,
    "sqlite",
    sqlx::Sqlite,
    super::Placeholders::Question,
    "SQLite re-encryption adapter (`BLOB` UUID, text or `INTEGER` keys)."
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statements_match_forge_go() {
        let spec = SqlColumn::new("people", "ssn")
            .expect("spec")
            .with_index_column("ssn_idx")
            .expect("index");
        assert_eq!(
            spec.page_sql(100, false, Placeholders::Dollar),
            "SELECT id, ssn FROM people WHERE ssn IS NOT NULL ORDER BY id LIMIT 100"
        );
        assert_eq!(
            spec.page_sql(5, true, Placeholders::Dollar),
            "SELECT id, ssn FROM people WHERE id > $1 AND ssn IS NOT NULL ORDER BY id LIMIT 5"
        );
        assert_eq!(
            spec.replace_sql(true, Placeholders::Dollar).expect("sql"),
            "UPDATE people SET ssn = $1, ssn_idx = $2 WHERE id = $3 AND ssn = $4"
        );
        assert_eq!(
            spec.replace_sql(false, Placeholders::Question)
                .expect("sql"),
            "UPDATE people SET ssn = ? WHERE id = ? AND ssn = ?"
        );
        let plain = SqlColumn::new("people", "ssn").expect("spec");
        assert!(plain.replace_sql(true, Placeholders::Dollar).is_err());
    }

    #[test]
    fn identifiers_are_validated() {
        for bad in ["", "People", "a-b", "1a", "a;drop", "a b", &"a".repeat(64)] {
            assert!(SqlColumn::new(bad, "ssn").is_err(), "{bad}");
            assert!(SqlColumn::new("people", bad).is_err(), "{bad}");
        }
        let spec = SqlColumn::new("people", "ssn").expect("spec");
        assert!(spec.clone().with_id_column("x y").is_err());
        assert!(spec.with_index_column("idx;").is_err());
    }
}
