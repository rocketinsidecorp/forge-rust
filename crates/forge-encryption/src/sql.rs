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

/// What the catalog says about one table and the role running a rotation.
#[cfg(feature = "postgres")]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct RowSecurity {
    /// `pg_class.relrowsecurity`: `ENABLE ROW LEVEL SECURITY`.
    enabled: bool,
    /// `pg_class.relforcerowsecurity`: `FORCE ROW LEVEL SECURITY`, which
    /// also binds the table owner.
    forced: bool,
    /// `current_user` has the privileges of the table's owner.
    owner: bool,
    /// `pg_roles.rolsuper` of `current_user`.
    superuser: bool,
    /// `pg_roles.rolbypassrls` of `current_user`.
    bypass: bool,
}

#[cfg(feature = "postgres")]
impl RowSecurity {
    /// Whether the policies filter the rows this role sees: superusers and
    /// `BYPASSRLS` roles never see them applied, owners only when forced.
    const fn applies(self) -> bool {
        self.enabled && !self.superuser && !self.bypass && (self.forced || !self.owner)
    }
}

/// The SQLx [`Rewriter`](crate::Rewriter) of one backend. `$field`s are the
/// backend's own state (the PostgreSQL tenant scope); `$extra` adds its
/// items, among them `begin(&self, check)`, which opens the transaction every
/// page read (`check == true`) and compare-and-set update runs in.
#[cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]
macro_rules! encrypted_column {
    (
        $module:ident, $feature:literal, $database:ty, $style:expr, $doc:literal,
        { $($field:ident: $field_type:ty = $field_init:expr),* }, { $($extra:item)* }
    ) => {
        #[cfg(feature = $feature)]
        #[doc = $doc]
        pub mod $module {
            use async_trait::async_trait;
            use forge_db::{Database, DbTransaction};
            use sqlx::{AssertSqlSafe, Row as _};
            use uuid::Uuid;

            use crate::{BoxError, EncryptionError, IdType, Record, Rewriter, SqlColumn, Update};

            /// The rewriter of one encrypted column, for
            /// [`Cipher::reencrypt`](crate::Cipher::reencrypt).
            #[derive(Debug)]
            pub struct EncryptedColumn {
                db: Database<$database>,
                spec: SqlColumn,
                $($field: $field_type,)*
            }

            impl EncryptedColumn {
                /// The rewriter of `spec` on `db`.
                pub fn new(
                    db: Database<$database>,
                    spec: SqlColumn,
                ) -> Result<Self, EncryptionError> {
                    spec.validate()?;
                    Ok(Self {
                        db,
                        spec,
                        $($field: $field_init,)*
                    })
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

            $($extra)*

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
                    let mut transaction: DbTransaction<$database> = self.begin(true).await?;
                    let rows = query.fetch_all(transaction.connection()).await?;
                    transaction.commit().await?;
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
                    let mut transaction: DbTransaction<$database> = self.begin(false).await?;
                    let result = query.execute(transaction.connection()).await?;
                    transaction.commit().await?;
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
    "PostgreSQL re-encryption adapter (`uuid`, text or `bigint` keys).\n\n\
     A table under row-level security (the generated tenant tables: `ENABLE` and \
     `FORCE ROW LEVEL SECURITY` on `forge.tenant_id`) is rotated one tenant at a \
     time with [`EncryptedColumn::tenant`](crate::postgres::EncryptedColumn::tenant). \
     Without a tenant scope every page first checks the catalog and fails with \
     [`EncryptionError::Configuration`](crate::EncryptionError::Configuration) \
     when the policies apply to the connected role, instead of seeing no row and \
     reporting an empty, successful rotation.",
    { scope: Option<forge_tenancy::TenantId> = None },
    {
        impl EncryptedColumn {
            /// Scopes every page read and compare-and-set update to `tenant`:
            /// each runs in a transaction that first installs it as
            /// `forge.tenant_id` ([`DbTransaction::set_tenant`]), so the
            /// table's row-level security policies show and accept that
            /// tenant's rows only. Rotate a tenant table once per tenant.
            #[must_use]
            pub fn tenant(mut self, tenant: forge_tenancy::TenantId) -> Self {
                self.scope = Some(tenant);
                self
            }

            async fn begin(&self, check: bool) -> Result<DbTransaction<sqlx::Postgres>, BoxError> {
                let mut transaction = self.db.begin().await?;
                match self.scope {
                    Some(tenant) => transaction.set_tenant(tenant).await?,
                    None if check => self.refuse_row_security(&mut transaction).await?,
                    None => {}
                }
                Ok(transaction)
            }

            /// Fails closed when row-level security would hide rows from an
            /// unscoped rotation.
            async fn refuse_row_security(
                &self,
                transaction: &mut DbTransaction<sqlx::Postgres>,
            ) -> Result<(), BoxError> {
                let state: Option<(bool, bool, bool, bool, bool)> = sqlx::query_as(
                    "SELECT c.relrowsecurity, c.relforcerowsecurity, \
                     pg_has_role(current_user, c.relowner, 'USAGE'), r.rolsuper, r.rolbypassrls \
                     FROM pg_class c JOIN pg_roles r ON r.rolname = current_user \
                     WHERE c.oid = to_regclass($1)",
                )
                .bind(self.spec.table())
                .fetch_optional(transaction.connection())
                .await?;
                let table = self.spec.table();
                let Some((enabled, forced, owner, superuser, bypass)) = state else {
                    return Err(EncryptionError::Configuration(format!(
                        "table {table} does not exist on the search path"
                    ))
                    .into());
                };
                let security = super::RowSecurity {
                    enabled,
                    forced,
                    owner,
                    superuser,
                    bypass,
                };
                if security.applies() {
                    return Err(EncryptionError::Configuration(format!(
                        "table {table} has row-level security for this database role, \
                         which sees no row without a tenant scope: rotate it once per \
                         tenant with EncryptedColumn::new(..)?.tenant(tenant_id), or as a \
                         role that bypasses row-level security (see docs/encryption.md)"
                    ))
                    .into());
                }
                Ok(())
            }
        }
    }
);
#[cfg(feature = "mysql")]
encrypted_column!(
    mysql,
    "mysql",
    sqlx::MySql,
    super::Placeholders::Question,
    "MySQL and MariaDB re-encryption adapter (`BINARY(16)` UUID, text or `BIGINT` keys).\n\n\
     MySQL and MariaDB have no row-level security: tenant isolation lives in \
     the application's `TenantContext` predicates, which a rotation does not \
     apply, so it visits every row of the table.",
    {},
    {
        impl EncryptedColumn {
            async fn begin(&self, _check: bool) -> Result<DbTransaction<sqlx::MySql>, BoxError> {
                Ok(self.db.begin().await?)
            }
        }
    }
);
#[cfg(feature = "sqlite")]
encrypted_column!(
    sqlite,
    "sqlite",
    sqlx::Sqlite,
    super::Placeholders::Question,
    "SQLite re-encryption adapter (`BLOB` UUID, text or `INTEGER` keys).\n\n\
     SQLite has no row-level security: tenant isolation lives in the \
     application's `TenantContext` predicates, which a rotation does not \
     apply, so it visits every row of the table.",
    {},
    {
        impl EncryptedColumn {
            async fn begin(&self, _check: bool) -> Result<DbTransaction<sqlx::Sqlite>, BoxError> {
                Ok(self.db.begin().await?)
            }
        }
    }
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

    #[cfg(feature = "postgres")]
    #[test]
    fn row_security_applies_only_to_roles_it_binds() {
        let tenant_table = RowSecurity {
            enabled: true,
            forced: true,
            ..RowSecurity::default()
        };
        assert!(tenant_table.applies(), "the generated runtime role");
        assert!(
            RowSecurity {
                owner: true,
                ..tenant_table
            }
            .applies(),
            "FORCE binds the owner"
        );
        for exempt in [
            RowSecurity::default(),
            RowSecurity {
                superuser: true,
                ..tenant_table
            },
            RowSecurity {
                bypass: true,
                ..tenant_table
            },
            RowSecurity {
                forced: false,
                owner: true,
                ..tenant_table
            },
        ] {
            assert!(!exempt.applies(), "{exempt:?}");
        }
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
