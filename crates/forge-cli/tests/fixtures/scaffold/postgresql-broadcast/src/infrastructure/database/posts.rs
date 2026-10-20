//! PostgreSQL storage of posts, implementing
//! [`PostRepository`]. SQL and SQLx stay in this module: the port and the
//! use cases see only domain and Forge types.
//!
//! Optimistic locking: `UPDATE` and `DELETE` match the expected `version`;
//! zero affected rows is [`PostStoreError::Conflict`].

use async_trait::async_trait;
use forge::core::Version;
use forge::db::{DbError, PgDatabase};
use sqlx::Row;
use sqlx::postgres::PgRow;

use crate::application::ports::post_repository::{PostPage, PostRepository, PostStoreError};
use crate::domain::entities::post::{Post, PostAttributes, PostId};

/// One row by id.
const FIND: &str = r#"SELECT "id", "version", "title", "body", "published"
    FROM "posts"
    WHERE "id" = $1"#;

/// The first page, in id order.
const PAGE_FIRST: &str = r#"SELECT "id", "version", "title", "body", "published"
    FROM "posts"
    ORDER BY "id"
    LIMIT $1"#;

/// The page after a cursor, in id order.
const PAGE_AFTER: &str = r#"SELECT "id", "version", "title", "body", "published"
    FROM "posts"
    WHERE "id" > $1
    ORDER BY "id"
    LIMIT $2"#;

/// A new row at the initial version.
const INSERT: &str = r#"INSERT INTO "posts" (
        "id", "version", "title", "body", "published"
    )
    VALUES ($1, $2, $3, $4, $5)"#;

/// Replaces the attributes if the version still matches.
const UPDATE: &str = r#"UPDATE "posts"
    SET "title" = $1,
        "body" = $2,
        "published" = $3,
        "version" = "version" + 1,
        "updated_at" = CURRENT_TIMESTAMP
    WHERE "id" = $4 AND "version" = $5"#;

/// Deletes the row if the version still matches.
const DELETE: &str = r#"DELETE FROM "posts"
    WHERE "id" = $1 AND "version" = $2"#;

/// [`PostRepository`] over the application's PostgreSQL pool.
#[derive(Clone, Debug)]
pub struct PgPostRepository {
    database: PgDatabase,
}

impl PgPostRepository {
    #[must_use]
    pub fn new(database: PgDatabase) -> Self {
        Self { database }
    }
}

#[async_trait]
impl PostRepository for PgPostRepository {
    async fn find(&self, id: PostId) -> Result<Option<Post>, PostStoreError> {
        let row = sqlx::query(FIND)
            .bind(id)
            .fetch_optional(self.database.pool())
            .await
            .map_err(unavailable)?;
        row.as_ref().map(restore).transpose()
    }

    async fn page(&self, after: Option<PostId>, limit: u16) -> Result<PostPage, PostStoreError> {
        // One extra row tells whether a next page exists.
        let fetch = i64::from(limit) + 1;
        let rows = match after {
            None => {
                sqlx::query(PAGE_FIRST)
                    .bind(fetch)
                    .fetch_all(self.database.pool())
                    .await
            }
            Some(after) => {
                sqlx::query(PAGE_AFTER)
                    .bind(after)
                    .bind(fetch)
                    .fetch_all(self.database.pool())
                    .await
            }
        }
        .map_err(unavailable)?;
        let mut posts = rows.iter().map(restore).collect::<Result<Vec<_>, _>>()?;
        let next = if posts.len() > usize::from(limit) {
            posts.truncate(usize::from(limit));
            posts.last().map(Post::id)
        } else {
            None
        };
        Ok(PostPage { posts, next })
    }

    async fn insert(&self, post: &Post) -> Result<(), PostStoreError> {
        let attributes = post.attributes();
        sqlx::query(INSERT)
            .bind(post.id())
            .bind(post.version())
            .bind(&attributes.title)
            .bind(&attributes.body)
            .bind(attributes.published)
            .execute(self.database.pool())
            .await
            .map_err(unavailable)?;
        Ok(())
    }

    async fn update(&self, post: &Post) -> Result<Version, PostStoreError> {
        let attributes = post.attributes();
        let next = post
            .version()
            .next()
            .map_err(|_| PostStoreError::Conflict)?;
        let result = sqlx::query(UPDATE)
            .bind(&attributes.title)
            .bind(&attributes.body)
            .bind(attributes.published)
            .bind(post.id())
            .bind(post.version())
            .execute(self.database.pool())
            .await
            .map_err(unavailable)?;
        if result.rows_affected() == 1 {
            Ok(next)
        } else {
            Err(PostStoreError::Conflict)
        }
    }

    async fn delete(&self, id: PostId, expected: Version) -> Result<(), PostStoreError> {
        let result = sqlx::query(DELETE)
            .bind(id)
            .bind(expected)
            .execute(self.database.pool())
            .await
            .map_err(unavailable)?;
        if result.rows_affected() == 1 {
            Ok(())
        } else {
            Err(PostStoreError::Conflict)
        }
    }
}

/// Rebuilds a post from a row; a value the domain rejects is `Corrupt`.
fn restore(row: &PgRow) -> Result<Post, PostStoreError> {
    let attributes = PostAttributes {
        title: row.try_get("title").map_err(corrupt)?,
        body: row.try_get("body").map_err(corrupt)?,
        published: row.try_get("published").map_err(corrupt)?,
    };
    Post::restore(
        row.try_get("id").map_err(corrupt)?,
        row.try_get("version").map_err(corrupt)?,
        attributes,
    )
    .map_err(|_| PostStoreError::Corrupt)
}

fn corrupt(_: sqlx::Error) -> PostStoreError {
    PostStoreError::Corrupt
}

/// Classifies the driver error (kept as the source for logs).
fn unavailable(error: sqlx::Error) -> PostStoreError {
    PostStoreError::Unavailable(Box::new(DbError::from(error)))
}
