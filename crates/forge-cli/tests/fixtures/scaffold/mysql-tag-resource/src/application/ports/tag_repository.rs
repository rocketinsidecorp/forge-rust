//! Storage port of the `tags` resource, implemented in
//! `crate::infrastructure::database::tags` and by in-memory fakes in
//! tests. It speaks only domain and Forge types: no SQL or driver types.

use async_trait::async_trait;
use forge::core::Version;

use crate::domain::entities::tag::{Tag, TagId};

/// One page of tags in identifier order (UUIDv7, so creation order).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TagPage {
    pub tags: Vec<Tag>,
    /// The cursor of the next page, when there is one.
    pub next: Option<TagId>,
}

/// Why storage could not complete an operation. Messages never contain SQL,
/// row values or credentials.
#[derive(Debug, thiserror::Error)]
pub enum TagStoreError {
    /// The stored version differs from the expected one, or the row is gone.
    #[error("the tag was changed or deleted concurrently")]
    Conflict,
    /// A stored row does not satisfy the domain rules.
    #[error("stored tag data is invalid")]
    Corrupt,
    /// The database failed; the source is for logs only.
    #[error("tag storage is unavailable")]
    Unavailable(#[source] Box<dyn std::error::Error + Send + Sync>),
}

/// Persistence of [`Tag`] values with optimistic locking.
#[async_trait]
pub trait TagRepository: Send + Sync {
    /// Loads one tag.
    async fn find(&self, id: TagId) -> Result<Option<Tag>, TagStoreError>;

    /// Reads at most `limit` tags whose identifier follows `after`.
    async fn page(&self, after: Option<TagId>, limit: u16) -> Result<TagPage, TagStoreError>;

    /// Stores a new tag.
    async fn insert(&self, tag: &Tag) -> Result<(), TagStoreError>;

    /// Replaces the stored tag if its version is still
    /// `tag.version()` and returns the new version; otherwise fails
    /// with [`TagStoreError::Conflict`].
    async fn update(&self, tag: &Tag) -> Result<Version, TagStoreError>;

    /// Deletes the tag if its version is still `expected`; otherwise
    /// fails with [`TagStoreError::Conflict`].
    async fn delete(&self, id: TagId, expected: Version) -> Result<(), TagStoreError>;
}
