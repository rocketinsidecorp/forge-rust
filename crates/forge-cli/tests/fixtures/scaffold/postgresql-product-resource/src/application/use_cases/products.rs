//! Use cases of the `products` resource: list, show, create, update and
//! delete with optimistic locking.
//!
//! Products belong to a tenant. Every use case takes the [`TenantContext`]
//! that the HTTP adapter derived from an authenticated session, an active
//! membership and an RBAC grant, and checks that the grant is for this
//! resource, like the HTTP routes do: listing needs [`READ_PERMISSION`],
//! changes need [`WRITE_PERMISSION`], and loading one product accepts
//! either, because the edit and update pages load the product they change
//! under the write grant.

use std::sync::Arc;

use forge::core::Version;
use forge::tenancy::TenantContext;

use crate::application::ports::product_repository::{
    ProductPage, ProductRepository, ProductStoreError,
};
use crate::domain::entities::product::{
    Product, ProductAttributes, ProductId, ProductValidationErrors,
};

/// Permission to read products.
pub const READ_PERMISSION: &str = "products:read";
/// Permission to create, change and delete products.
pub const WRITE_PERMISSION: &str = "products:write";
/// Products per index page.
pub const PAGE_SIZE: u16 = 25;

/// Why a product use case failed.
#[derive(Debug, thiserror::Error)]
pub enum ProductError {
    /// The attributes break business rules.
    #[error("{0}")]
    Invalid(ProductValidationErrors),
    /// No such product.
    #[error("product not found")]
    NotFound,
    /// The product changed since the caller read it.
    #[error("the product was changed by someone else")]
    Conflict,
    /// The tenant context was authorized for another permission.
    #[error("permission denied")]
    Forbidden,
    /// Storage failed.
    #[error("product storage failed")]
    Store(#[source] ProductStoreError),
}

/// The product use cases over a [`ProductRepository`].
#[derive(Clone)]
pub struct ProductService {
    repository: Arc<dyn ProductRepository>,
}

impl ProductService {
    #[must_use]
    pub fn new(repository: Arc<dyn ProductRepository>) -> Self {
        Self { repository }
    }

    /// One page of products, starting after `after`.
    pub async fn list(
        &self,
        context: &TenantContext,
        after: Option<ProductId>,
    ) -> Result<ProductPage, ProductError> {
        authorize(context, &[READ_PERMISSION])?;
        self.repository
            .page(context, after, PAGE_SIZE)
            .await
            .map_err(ProductError::Store)
    }

    /// One product.
    pub async fn find(
        &self,
        context: &TenantContext,
        id: ProductId,
    ) -> Result<Product, ProductError> {
        authorize(context, &[READ_PERMISSION, WRITE_PERMISSION])?;
        self.repository
            .find(context, id)
            .await
            .map_err(ProductError::Store)?
            .ok_or(ProductError::NotFound)
    }

    /// Validates and stores a new product.
    pub async fn create(
        &self,
        context: &TenantContext,
        attributes: ProductAttributes,
    ) -> Result<Product, ProductError> {
        authorize(context, &[WRITE_PERMISSION])?;
        let product = Product::new(attributes).map_err(ProductError::Invalid)?;
        self.repository
            .insert(context, &product)
            .await
            .map_err(ProductError::Store)?;
        Ok(product)
    }

    /// Replaces the attributes of the product read at version `expected`.
    pub async fn update(
        &self,
        context: &TenantContext,
        id: ProductId,
        expected: Version,
        attributes: ProductAttributes,
    ) -> Result<Product, ProductError> {
        authorize(context, &[WRITE_PERMISSION])?;
        let product = Product::restore(id, expected, attributes).map_err(ProductError::Invalid)?;
        match self.repository.update(context, &product).await {
            Ok(version) => Ok(product.with_version(version)),
            Err(ProductStoreError::Conflict) => Err(self.conflict_or_missing(context, id).await),
            Err(error) => Err(ProductError::Store(error)),
        }
    }

    /// Deletes the product read at version `expected`.
    pub async fn delete(
        &self,
        context: &TenantContext,
        id: ProductId,
        expected: Version,
    ) -> Result<(), ProductError> {
        authorize(context, &[WRITE_PERMISSION])?;
        match self.repository.delete(context, id, expected).await {
            Ok(()) => Ok(()),
            Err(ProductStoreError::Conflict) => Err(self.conflict_or_missing(context, id).await),
            Err(error) => Err(ProductError::Store(error)),
        }
    }

    /// A version mismatch is a conflict while the product exists, and
    /// "not found" once it is gone.
    async fn conflict_or_missing(&self, context: &TenantContext, id: ProductId) -> ProductError {
        match self.repository.find(context, id).await {
            Ok(Some(_)) => ProductError::Conflict,
            Ok(None) => ProductError::NotFound,
            Err(error) => ProductError::Store(error),
        }
    }
}

/// Defense in depth: the context must carry a grant for one of `allowed`.
fn authorize(context: &TenantContext, allowed: &[&str]) -> Result<(), ProductError> {
    let granted = context.authorization().permission().as_str();
    if allowed.contains(&granted) {
        Ok(())
    } else {
        Err(ProductError::Forbidden)
    }
}
