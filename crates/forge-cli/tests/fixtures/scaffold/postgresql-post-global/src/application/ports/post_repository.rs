//! Storage port of the `posts` resource, implemented in
//! `crate::infrastructure::database::posts` and by in-memory fakes in
//! tests. It speaks only domain and Forge types: no SQL or driver types.

use async_trait::async_trait;
use forge::core::Version;

use crate::domain::entities::post::{Post, PostId};

/// One page of posts in identifier order (UUIDv7, so creation order).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PostPage {
    pub posts: Vec<Post>,
    /// The cursor of the next page, when there is one.
    pub next: Option<PostId>,
}

/// Why storage could not complete an operation. Messages never contain SQL,
/// row values or credentials.
#[derive(Debug, thiserror::Error)]
pub enum PostStoreError {
    /// The stored version differs from the expected one, or the row is gone.
    #[error("the post was changed or deleted concurrently")]
    Conflict,
    /// A stored row does not satisfy the domain rules.
    #[error("stored post data is invalid")]
    Corrupt,
    /// The database failed; the source is for logs only.
    #[error("post storage is unavailable")]
    Unavailable(#[source] Box<dyn std::error::Error + Send + Sync>),
}

/// Persistence of [`Post`] values with optimistic locking.
#[async_trait]
pub trait PostRepository: Send + Sync {
    /// Loads one post.
    async fn find(&self, id: PostId) -> Result<Option<Post>, PostStoreError>;

    /// Reads at most `limit` posts whose identifier follows `after`.
    async fn page(&self, after: Option<PostId>, limit: u16) -> Result<PostPage, PostStoreError>;

    /// Stores a new post.
    async fn insert(&self, post: &Post) -> Result<(), PostStoreError>;

    /// Replaces the stored post if its version is still
    /// `post.version()` and returns the new version; otherwise fails
    /// with [`PostStoreError::Conflict`].
    async fn update(&self, post: &Post) -> Result<Version, PostStoreError>;

    /// Deletes the post if its version is still `expected`; otherwise
    /// fails with [`PostStoreError::Conflict`].
    async fn delete(&self, id: PostId, expected: Version) -> Result<(), PostStoreError>;
}
