//! Use cases of the `tags` resource: list, show, create, update and
//! delete with optimistic locking.

use std::sync::Arc;

use forge::core::Version;

use crate::application::ports::tag_repository::{TagPage, TagRepository, TagStoreError};
use crate::domain::entities::tag::{Tag, TagAttributes, TagId, TagValidationErrors};

/// Permission to read tags.
pub const READ_PERMISSION: &str = "tags:read";
/// Permission to create, change and delete tags.
pub const WRITE_PERMISSION: &str = "tags:write";
/// Tags per index page.
pub const PAGE_SIZE: u16 = 25;

/// Why a tag use case failed.
#[derive(Debug, thiserror::Error)]
pub enum TagError {
    /// The attributes break business rules.
    #[error("{0}")]
    Invalid(TagValidationErrors),
    /// No such tag.
    #[error("tag not found")]
    NotFound,
    /// The tag changed since the caller read it.
    #[error("the tag was changed by someone else")]
    Conflict,
    /// Storage failed.
    #[error("tag storage failed")]
    Store(#[source] TagStoreError),
}

/// The tag use cases over a [`TagRepository`].
#[derive(Clone)]
pub struct TagService {
    repository: Arc<dyn TagRepository>,
}

impl TagService {
    #[must_use]
    pub fn new(repository: Arc<dyn TagRepository>) -> Self {
        Self { repository }
    }

    /// One page of tags, starting after `after`.
    pub async fn list(&self, after: Option<TagId>) -> Result<TagPage, TagError> {
        self.repository
            .page(after, PAGE_SIZE)
            .await
            .map_err(TagError::Store)
    }

    /// One tag.
    pub async fn find(&self, id: TagId) -> Result<Tag, TagError> {
        self.repository
            .find(id)
            .await
            .map_err(TagError::Store)?
            .ok_or(TagError::NotFound)
    }

    /// Validates and stores a new tag.
    pub async fn create(&self, attributes: TagAttributes) -> Result<Tag, TagError> {
        let tag = Tag::new(attributes).map_err(TagError::Invalid)?;
        self.repository
            .insert(&tag)
            .await
            .map_err(TagError::Store)?;
        Ok(tag)
    }

    /// Replaces the attributes of the tag read at version `expected`.
    pub async fn update(
        &self,
        id: TagId,
        expected: Version,
        attributes: TagAttributes,
    ) -> Result<Tag, TagError> {
        let tag = Tag::restore(id, expected, attributes).map_err(TagError::Invalid)?;
        match self.repository.update(&tag).await {
            Ok(version) => Ok(tag.with_version(version)),
            Err(TagStoreError::Conflict) => Err(self.conflict_or_missing(id).await),
            Err(error) => Err(TagError::Store(error)),
        }
    }

    /// Deletes the tag read at version `expected`.
    pub async fn delete(&self, id: TagId, expected: Version) -> Result<(), TagError> {
        match self.repository.delete(id, expected).await {
            Ok(()) => Ok(()),
            Err(TagStoreError::Conflict) => Err(self.conflict_or_missing(id).await),
            Err(error) => Err(TagError::Store(error)),
        }
    }

    /// A version mismatch is a conflict while the tag exists, and
    /// "not found" once it is gone.
    async fn conflict_or_missing(&self, id: TagId) -> TagError {
        match self.repository.find(id).await {
            Ok(Some(_)) => TagError::Conflict,
            Ok(None) => TagError::NotFound,
            Err(error) => TagError::Store(error),
        }
    }
}
