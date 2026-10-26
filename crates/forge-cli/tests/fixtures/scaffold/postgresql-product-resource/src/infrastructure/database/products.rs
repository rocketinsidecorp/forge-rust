//! PostgreSQL storage of products, implementing
//! [`ProductRepository`]. SQL and SQLx stay in this module: the port and the
//! use cases see only domain and Forge types.
//!
//! Tenant isolation: every statement binds the tenant of the authorized
//! [`TenantContext`] in its `WHERE` clause (or `INSERT` values).
//! Each operation also runs in a transaction started by
//! [`begin_tenant_transaction`], which sets `forge.tenant_id` for the
//! row-level security policy of the `products` table (defense in depth).
//!
//! Optimistic locking: `UPDATE` and `DELETE` match the expected `version`;
//! zero affected rows is [`ProductStoreError::Conflict`].

use async_trait::async_trait;
use forge::core::Version;
use forge::db::{DbError, PgDatabase};
use forge::tenancy::TenantContext;
use sqlx::Row;
use sqlx::postgres::PgRow;

use crate::application::ports::product_repository::{
    ProductPage, ProductRepository, ProductStoreError,
};
use crate::domain::entities::product::{Product, ProductAttributes, ProductId};
use crate::infrastructure::database::begin_tenant_transaction;

/// One row by id.
const FIND: &str = r#"SELECT "id", "version", "name", "summary", "price_cents"
    FROM "products"
    WHERE "tenant_id" = $1 AND "id" = $2"#;

/// The first page, in id order.
const PAGE_FIRST: &str = r#"SELECT "id", "version", "name", "summary", "price_cents"
    FROM "products"
    WHERE "tenant_id" = $1
    ORDER BY "id"
    LIMIT $2"#;

/// The page after a cursor, in id order.
const PAGE_AFTER: &str = r#"SELECT "id", "version", "name", "summary", "price_cents"
    FROM "products"
    WHERE "tenant_id" = $1 AND "id" > $2
    ORDER BY "id"
    LIMIT $3"#;

/// A new row at the initial version.
const INSERT: &str = r#"INSERT INTO "products" (
        "id", "tenant_id", "version", "name", "summary", "price_cents"
    )
    VALUES ($1, $2, $3, $4, $5, $6)"#;

/// Replaces the attributes if the version still matches.
const UPDATE: &str = r#"UPDATE "products"
    SET "name" = $1,
        "summary" = $2,
        "price_cents" = $3,
        "version" = "version" + 1,
        "updated_at" = CURRENT_TIMESTAMP
    WHERE "tenant_id" = $4 AND "id" = $5 AND "version" = $6"#;

/// Deletes the row if the version still matches.
const DELETE: &str = r#"DELETE FROM "products"
    WHERE "tenant_id" = $1 AND "id" = $2 AND "version" = $3"#;

/// [`ProductRepository`] over the application's PostgreSQL pool.
#[derive(Clone, Debug)]
pub struct PgProductRepository {
    database: PgDatabase,
}

impl PgProductRepository {
    #[must_use]
    pub fn new(database: PgDatabase) -> Self {
        Self { database }
    }
}

#[async_trait]
impl ProductRepository for PgProductRepository {
    async fn find(
        &self,
        context: &TenantContext,
        id: ProductId,
    ) -> Result<Option<Product>, ProductStoreError> {
        let mut transaction = begin_tenant_transaction(self.database.pool(), context)
            .await
            .map_err(unavailable)?;
        let row = sqlx::query(FIND)
            .bind(context.tenant_id())
            .bind(id)
            .fetch_optional(&mut *transaction)
            .await
            .map_err(unavailable)?;
        transaction.commit().await.map_err(unavailable)?;
        row.as_ref().map(restore).transpose()
    }

    async fn page(
        &self,
        context: &TenantContext,
        after: Option<ProductId>,
        limit: u16,
    ) -> Result<ProductPage, ProductStoreError> {
        // One extra row tells whether a next page exists.
        let fetch = i64::from(limit) + 1;
        let mut transaction = begin_tenant_transaction(self.database.pool(), context)
            .await
            .map_err(unavailable)?;
        let rows = match after {
            None => {
                sqlx::query(PAGE_FIRST)
                    .bind(context.tenant_id())
                    .bind(fetch)
                    .fetch_all(&mut *transaction)
                    .await
            }
            Some(after) => {
                sqlx::query(PAGE_AFTER)
                    .bind(context.tenant_id())
                    .bind(after)
                    .bind(fetch)
                    .fetch_all(&mut *transaction)
                    .await
            }
        }
        .map_err(unavailable)?;
        transaction.commit().await.map_err(unavailable)?;
        let mut products = rows.iter().map(restore).collect::<Result<Vec<_>, _>>()?;
        let next = if products.len() > usize::from(limit) {
            products.truncate(usize::from(limit));
            products.last().map(Product::id)
        } else {
            None
        };
        Ok(ProductPage { products, next })
    }

    async fn insert(
        &self,
        context: &TenantContext,
        product: &Product,
    ) -> Result<(), ProductStoreError> {
        let attributes = product.attributes();
        let mut transaction = begin_tenant_transaction(self.database.pool(), context)
            .await
            .map_err(unavailable)?;
        sqlx::query(INSERT)
            .bind(product.id())
            .bind(context.tenant_id())
            .bind(product.version())
            .bind(&attributes.name)
            .bind(&attributes.summary)
            .bind(attributes.price_cents)
            .execute(&mut *transaction)
            .await
            .map_err(unavailable)?;
        transaction.commit().await.map_err(unavailable)?;
        Ok(())
    }

    async fn update(
        &self,
        context: &TenantContext,
        product: &Product,
    ) -> Result<Version, ProductStoreError> {
        let attributes = product.attributes();
        let next = product
            .version()
            .next()
            .map_err(|_| ProductStoreError::Conflict)?;
        let mut transaction = begin_tenant_transaction(self.database.pool(), context)
            .await
            .map_err(unavailable)?;
        let result = sqlx::query(UPDATE)
            .bind(&attributes.name)
            .bind(&attributes.summary)
            .bind(attributes.price_cents)
            .bind(context.tenant_id())
            .bind(product.id())
            .bind(product.version())
            .execute(&mut *transaction)
            .await
            .map_err(unavailable)?;
        transaction.commit().await.map_err(unavailable)?;
        if result.rows_affected() == 1 {
            Ok(next)
        } else {
            Err(ProductStoreError::Conflict)
        }
    }

    async fn delete(
        &self,
        context: &TenantContext,
        id: ProductId,
        expected: Version,
    ) -> Result<(), ProductStoreError> {
        let mut transaction = begin_tenant_transaction(self.database.pool(), context)
            .await
            .map_err(unavailable)?;
        let result = sqlx::query(DELETE)
            .bind(context.tenant_id())
            .bind(id)
            .bind(expected)
            .execute(&mut *transaction)
            .await
            .map_err(unavailable)?;
        transaction.commit().await.map_err(unavailable)?;
        if result.rows_affected() == 1 {
            Ok(())
        } else {
            Err(ProductStoreError::Conflict)
        }
    }
}

/// Rebuilds a product from a row; a value the domain rejects is `Corrupt`.
fn restore(row: &PgRow) -> Result<Product, ProductStoreError> {
    let attributes = ProductAttributes {
        name: row.try_get("name").map_err(corrupt)?,
        summary: row.try_get("summary").map_err(corrupt)?,
        price_cents: row.try_get("price_cents").map_err(corrupt)?,
    };
    Product::restore(
        row.try_get("id").map_err(corrupt)?,
        row.try_get("version").map_err(corrupt)?,
        attributes,
    )
    .map_err(|_| ProductStoreError::Corrupt)
}

fn corrupt(_: sqlx::Error) -> ProductStoreError {
    ProductStoreError::Corrupt
}

/// Classifies the driver error (kept as the source for logs).
fn unavailable(error: sqlx::Error) -> ProductStoreError {
    ProductStoreError::Unavailable(Box::new(DbError::from(error)))
}
