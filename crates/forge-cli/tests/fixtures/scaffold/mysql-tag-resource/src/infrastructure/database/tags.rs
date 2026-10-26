//! MySQL/MariaDB storage of tags, implementing
//! [`TagRepository`]. SQL and SQLx stay in this module: the port and the
//! use cases see only domain and Forge types.
//!
//! Optimistic locking: `UPDATE` and `DELETE` match the expected `version`;
//! zero affected rows is [`TagStoreError::Conflict`].

use async_trait::async_trait;
use forge::core::Version;
use forge::db::{DbError, MySqlDatabase};
use sqlx::Row;
use sqlx::mysql::MySqlRow;

use crate::application::ports::tag_repository::{TagPage, TagRepository, TagStoreError};
use crate::domain::entities::tag::{Tag, TagAttributes, TagId};

/// One row by id.
const FIND: &str = r#"SELECT `id`, `version`, `label`, `active`
    FROM `tags`
    WHERE `id` = ?"#;

/// The first page, in id order.
const PAGE_FIRST: &str = r#"SELECT `id`, `version`, `label`, `active`
    FROM `tags`
    ORDER BY `id`
    LIMIT ?"#;

/// The page after a cursor, in id order.
const PAGE_AFTER: &str = r#"SELECT `id`, `version`, `label`, `active`
    FROM `tags`
    WHERE `id` > ?
    ORDER BY `id`
    LIMIT ?"#;

/// A new row at the initial version.
const INSERT: &str = r#"INSERT INTO `tags` (
        `id`, `version`, `label`, `active`
    )
    VALUES (?, ?, ?, ?)"#;

/// Replaces the attributes if the version still matches.
const UPDATE: &str = r#"UPDATE `tags`
    SET `label` = ?,
        `active` = ?,
        `version` = `version` + 1,
        `updated_at` = CURRENT_TIMESTAMP(6)
    WHERE `id` = ? AND `version` = ?"#;

/// Deletes the row if the version still matches.
const DELETE: &str = r#"DELETE FROM `tags`
    WHERE `id` = ? AND `version` = ?"#;

/// [`TagRepository`] over the application's MySQL/MariaDB pool.
#[derive(Clone, Debug)]
pub struct MySqlTagRepository {
    database: MySqlDatabase,
}

impl MySqlTagRepository {
    #[must_use]
    pub fn new(database: MySqlDatabase) -> Self {
        Self { database }
    }
}

#[async_trait]
impl TagRepository for MySqlTagRepository {
    async fn find(&self, id: TagId) -> Result<Option<Tag>, TagStoreError> {
        let row = sqlx::query(FIND)
            .bind(id)
            .fetch_optional(self.database.pool())
            .await
            .map_err(unavailable)?;
        row.as_ref().map(restore).transpose()
    }

    async fn page(&self, after: Option<TagId>, limit: u16) -> Result<TagPage, TagStoreError> {
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
        let mut tags = rows.iter().map(restore).collect::<Result<Vec<_>, _>>()?;
        let next = if tags.len() > usize::from(limit) {
            tags.truncate(usize::from(limit));
            tags.last().map(Tag::id)
        } else {
            None
        };
        Ok(TagPage { tags, next })
    }

    async fn insert(&self, tag: &Tag) -> Result<(), TagStoreError> {
        let attributes = tag.attributes();
        sqlx::query(INSERT)
            .bind(tag.id())
            .bind(tag.version())
            .bind(&attributes.label)
            .bind(attributes.active)
            .execute(self.database.pool())
            .await
            .map_err(unavailable)?;
        Ok(())
    }

    async fn update(&self, tag: &Tag) -> Result<Version, TagStoreError> {
        let attributes = tag.attributes();
        let next = tag.version().next().map_err(|_| TagStoreError::Conflict)?;
        let result = sqlx::query(UPDATE)
            .bind(&attributes.label)
            .bind(attributes.active)
            .bind(tag.id())
            .bind(tag.version())
            .execute(self.database.pool())
            .await
            .map_err(unavailable)?;
        if result.rows_affected() == 1 {
            Ok(next)
        } else {
            Err(TagStoreError::Conflict)
        }
    }

    async fn delete(&self, id: TagId, expected: Version) -> Result<(), TagStoreError> {
        let result = sqlx::query(DELETE)
            .bind(id)
            .bind(expected)
            .execute(self.database.pool())
            .await
            .map_err(unavailable)?;
        if result.rows_affected() == 1 {
            Ok(())
        } else {
            Err(TagStoreError::Conflict)
        }
    }
}

/// Rebuilds a tag from a row; a value the domain rejects is `Corrupt`.
fn restore(row: &MySqlRow) -> Result<Tag, TagStoreError> {
    let attributes = TagAttributes {
        label: row.try_get("label").map_err(corrupt)?,
        active: row.try_get("active").map_err(corrupt)?,
    };
    Tag::restore(
        row.try_get("id").map_err(corrupt)?,
        row.try_get("version").map_err(corrupt)?,
        attributes,
    )
    .map_err(|_| TagStoreError::Corrupt)
}

fn corrupt(_: sqlx::Error) -> TagStoreError {
    TagStoreError::Corrupt
}

/// Classifies the driver error (kept as the source for logs).
fn unavailable(error: sqlx::Error) -> TagStoreError {
    TagStoreError::Unavailable(Box::new(DbError::from(error)))
}
