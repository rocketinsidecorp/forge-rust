//! Storage port of the `products` resource, implemented in
//! `crate::infrastructure::database::products` and by in-memory fakes in
//! tests. It speaks only domain and Forge types: no SQL or driver types.

use async_trait::async_trait;
use forge::core::Version;
use forge::tenancy::TenantContext;

use crate::domain::entities::product::{Product, ProductId};

/// One page of products in identifier order (UUIDv7, so creation order).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProductPage {
    pub products: Vec<Product>,
    /// The cursor of the next page, when there is one.
    pub next: Option<ProductId>,
}

/// Why storage could not complete an operation. Messages never contain SQL,
/// row values or credentials.
#[derive(Debug, thiserror::Error)]
pub enum ProductStoreError {
    /// The stored version differs from the expected one, or the row is gone.
    #[error("the product was changed or deleted concurrently")]
    Conflict,
    /// A stored row does not satisfy the domain rules.
    #[error("stored product data is invalid")]
    Corrupt,
    /// The database failed; the source is for logs only.
    #[error("product storage is unavailable")]
    Unavailable(#[source] Box<dyn std::error::Error + Send + Sync>),
}

/// Persistence of [`Product`] values with optimistic locking.
///
/// Every operation takes the authorized [`TenantContext`] and only ever sees
/// rows of its tenant.
#[async_trait]
pub trait ProductRepository: Send + Sync {
    /// Loads one product.
    async fn find(
        &self,
        context: &TenantContext,
        id: ProductId,
    ) -> Result<Option<Product>, ProductStoreError>;

    /// Reads at most `limit` products whose identifier follows `after`.
    async fn page(
        &self,
        context: &TenantContext,
        after: Option<ProductId>,
        limit: u16,
    ) -> Result<ProductPage, ProductStoreError>;

    /// Stores a new product.
    async fn insert(
        &self,
        context: &TenantContext,
        product: &Product,
    ) -> Result<(), ProductStoreError>;

    /// Replaces the stored product if its version is still
    /// `product.version()` and returns the new version; otherwise fails
    /// with [`ProductStoreError::Conflict`].
    async fn update(
        &self,
        context: &TenantContext,
        product: &Product,
    ) -> Result<Version, ProductStoreError>;

    /// Deletes the product if its version is still `expected`; otherwise
    /// fails with [`ProductStoreError::Conflict`].
    async fn delete(
        &self,
        context: &TenantContext,
        id: ProductId,
        expected: Version,
    ) -> Result<(), ProductStoreError>;
}
