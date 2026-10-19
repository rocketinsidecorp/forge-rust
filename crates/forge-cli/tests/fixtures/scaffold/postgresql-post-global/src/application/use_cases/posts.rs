//! Use cases of the `posts` resource: list, show, create, update and
//! delete with optimistic locking.

use std::sync::Arc;

use forge::core::Version;

use crate::application::ports::post_repository::{PostPage, PostRepository, PostStoreError};
use crate::domain::entities::post::{Post, PostAttributes, PostId, PostValidationErrors};

/// Permission to read posts.
pub const READ_PERMISSION: &str = "posts:read";
/// Permission to create, change and delete posts.
pub const WRITE_PERMISSION: &str = "posts:write";
/// Posts per index page.
pub const PAGE_SIZE: u16 = 25;

/// Why a post use case failed.
#[derive(Debug, thiserror::Error)]
pub enum PostError {
    /// The attributes break business rules.
    #[error("{0}")]
    Invalid(PostValidationErrors),
    /// No such post.
    #[error("post not found")]
    NotFound,
    /// The post changed since the caller read it.
    #[error("the post was changed by someone else")]
    Conflict,
    /// Storage failed.
    #[error("post storage failed")]
    Store(#[source] PostStoreError),
}

/// The post use cases over a [`PostRepository`].
#[derive(Clone)]
pub struct PostService {
    repository: Arc<dyn PostRepository>,
}

impl PostService {
    #[must_use]
    pub fn new(repository: Arc<dyn PostRepository>) -> Self {
        Self { repository }
    }

    /// One page of posts, starting after `after`.
    pub async fn list(&self, after: Option<PostId>) -> Result<PostPage, PostError> {
        self.repository
            .page(after, PAGE_SIZE)
            .await
            .map_err(PostError::Store)
    }

    /// One post.
    pub async fn find(&self, id: PostId) -> Result<Post, PostError> {
        self.repository
            .find(id)
            .await
            .map_err(PostError::Store)?
            .ok_or(PostError::NotFound)
    }

    /// Validates and stores a new post.
    pub async fn create(&self, attributes: PostAttributes) -> Result<Post, PostError> {
        let post = Post::new(attributes).map_err(PostError::Invalid)?;
        self.repository
            .insert(&post)
            .await
            .map_err(PostError::Store)?;
        Ok(post)
    }

    /// Replaces the attributes of the post read at version `expected`.
    pub async fn update(
        &self,
        id: PostId,
        expected: Version,
        attributes: PostAttributes,
    ) -> Result<Post, PostError> {
        let post = Post::restore(id, expected, attributes).map_err(PostError::Invalid)?;
        match self.repository.update(&post).await {
            Ok(version) => Ok(post.with_version(version)),
            Err(PostStoreError::Conflict) => Err(self.conflict_or_missing(id).await),
            Err(error) => Err(PostError::Store(error)),
        }
    }

    /// Deletes the post read at version `expected`.
    pub async fn delete(&self, id: PostId, expected: Version) -> Result<(), PostError> {
        match self.repository.delete(id, expected).await {
            Ok(()) => Ok(()),
            Err(PostStoreError::Conflict) => Err(self.conflict_or_missing(id).await),
            Err(error) => Err(PostError::Store(error)),
        }
    }

    /// A version mismatch is a conflict while the post exists, and
    /// "not found" once it is gone.
    async fn conflict_or_missing(&self, id: PostId) -> PostError {
        match self.repository.find(id).await {
            Ok(Some(_)) => PostError::Conflict,
            Ok(None) => PostError::NotFound,
            Err(error) => PostError::Store(error),
        }
    }
}
