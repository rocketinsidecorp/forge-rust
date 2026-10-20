//! Moving a column to the active key in bounded, resumable batches.

use std::fmt;

use async_trait::async_trait;
use forge_core::Cancellation;
use thiserror::Error;

use crate::{Aad, Cipher};

const DEFAULT_BATCH_SIZE: u32 = 100;
const MAX_BATCH_SIZE: u32 = 10_000;

/// The error type of a [`Rewriter`]: the storage's own error.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// One stored ciphertext and the id of its row.
#[derive(Clone, Eq, PartialEq)]
pub struct Record {
    /// The row's primary key as text (the record id of its associated data).
    pub id: String,
    /// The stored `fg1.` text.
    pub sealed: String,
}

impl fmt::Debug for Record {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Record")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

/// Replaces a stored ciphertext. `old_sealed` makes it a compare-and-set:
/// the store applies it only while the row still holds that value, so a
/// concurrent application write is never overwritten with stale data.
#[derive(Clone, Eq, PartialEq)]
pub struct Update {
    /// The row's primary key.
    pub id: String,
    /// The value read from the row.
    pub old_sealed: String,
    /// The value to store.
    pub new_sealed: String,
    /// The new blind index, when the rotation maintains one.
    pub index: Option<String>,
}

impl fmt::Debug for Update {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Update")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

/// The storage side of a rotation for one encrypted column: the port that
/// [`Cipher::reencrypt`] drives. The `EncryptedColumn` adapters (features
/// `postgres`, `mysql`, `sqlite`) implement it with SQLx.
#[async_trait]
pub trait Rewriter: Send + Sync {
    /// The table name, part of every value's associated data.
    fn table(&self) -> &str;
    /// The column name, part of every value's associated data.
    fn column(&self) -> &str;
    /// Up to `limit` rows whose id is greater than `after` (all rows when
    /// `None`), ordered by id in the database's own order. Rows whose value
    /// is NULL are omitted.
    async fn page(&self, after: Option<&str>, limit: u32) -> Result<Vec<Record>, BoxError>;
    /// Applies `update`, reporting whether a row changed. `false` means the
    /// row no longer holds `update.old_sealed`.
    async fn replace(&self, update: &Update) -> Result<bool, BoxError>;
}

/// Tunes [`Cipher::reencrypt`].
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RotateOptions {
    /// The page size; 100 when zero, at most 10,000.
    pub batch_size: u32,
    /// Resumes a previous run: rows with an id up to this one are skipped.
    /// Pass the `last_id` of the interrupted [`Report`].
    pub after: Option<String>,
    /// Also writes the blind index (under the active key) of every row
    /// visited, repairing missing or stale indexes.
    pub blind_index: bool,
}

/// What a rotation did. `last_id` is the checkpoint for
/// [`RotateOptions::after`].
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Report {
    /// Rows visited: the rows this connection can see. `0` never proves a
    /// column is done (row-level security can hide every row); count the
    /// rows not on the active key instead.
    pub scanned: u64,
    /// Rows moved to the active key.
    pub rotated: u64,
    /// Rows already on the active key.
    pub skipped: u64,
    /// Rows changed concurrently; run again to pick them up.
    pub conflicts: u64,
    /// Blind indexes written ([`RotateOptions::blind_index`]).
    pub reindexed: u64,
    /// The last row visited.
    pub last_id: Option<String>,
}

/// Why a rotation stopped. Messages name row ids, never values.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ReencryptFailure {
    /// The options are invalid.
    #[error("encryption: batch size must be between 1 and {MAX_BATCH_SIZE}")]
    BatchSize,
    /// Cancellation was requested; resume from the report's `last_id`.
    #[error("encryption: re-encryption cancelled")]
    Cancelled,
    /// A page could not be read.
    #[error("encryption: read page after {after:?}: {source}")]
    Page {
        /// The checkpoint the page started after.
        after: Option<String>,
        /// The storage error.
        source: BoxError,
    },
    /// The rewriter returned the checkpoint again; it would loop forever.
    #[error("encryption: rewriter did not advance past row {0:?}")]
    NoProgress(String),
    /// A row could not be decrypted, re-encrypted or written. Investigate it;
    /// never skip it.
    #[error("encryption: row {id:?}: {source}")]
    Row {
        /// The row's primary key.
        id: String,
        /// What failed.
        source: BoxError,
    },
}

/// A stopped rotation: the failure and the work done until then.
#[derive(Debug, Error)]
#[error("{failure}")]
pub struct ReencryptError {
    /// What was done before the rotation stopped; `last_id` resumes it.
    pub report: Report,
    /// Why it stopped.
    #[source]
    pub failure: ReencryptFailure,
}

impl Cipher {
    /// Moves every row of `rewriter` to the active key in bounded pages. Safe
    /// to interrupt and rerun: rows already on the active key are skipped,
    /// every write is a compare-and-set on the value read, and the report's
    /// `last_id` resumes a cancelled run. A row that cannot be decrypted stops
    /// the run with an error naming its id, never its value.
    pub async fn reencrypt(
        &self,
        rewriter: &dyn Rewriter,
        options: RotateOptions,
        cancellation: &Cancellation,
    ) -> Result<Report, ReencryptError> {
        let mut report = Report {
            last_id: options.after.clone(),
            ..Report::default()
        };
        let size = match options.batch_size {
            0 => DEFAULT_BATCH_SIZE,
            size if size > MAX_BATCH_SIZE => {
                return Err(ReencryptError {
                    report,
                    failure: ReencryptFailure::BatchSize,
                });
            }
            size => size,
        };
        loop {
            if cancellation.is_cancelled() {
                return Err(stop(report, ReencryptFailure::Cancelled));
            }
            let page = match rewriter.page(report.last_id.as_deref(), size).await {
                Ok(page) => page,
                Err(source) => {
                    let after = report.last_id.clone();
                    return Err(stop(report, ReencryptFailure::Page { after, source }));
                }
            };
            let full = page.len() >= usize::try_from(size).unwrap_or(usize::MAX);
            for record in page {
                if cancellation.is_cancelled() {
                    return Err(stop(report, ReencryptFailure::Cancelled));
                }
                // The database defines the order (uuid bytes, integers,
                // collation), so ids are never compared here; a row that
                // repeats the checkpoint would loop forever and is refused.
                if report.last_id.as_deref() == Some(record.id.as_str()) {
                    return Err(stop(report, ReencryptFailure::NoProgress(record.id)));
                }
                if let Err(source) = self
                    .reencrypt_record(rewriter, &record, &options, &mut report)
                    .await
                {
                    return Err(stop(
                        report,
                        ReencryptFailure::Row {
                            id: record.id,
                            source,
                        },
                    ));
                }
                report.scanned += 1;
                report.last_id = Some(record.id);
            }
            if !full {
                return Ok(report);
            }
        }
    }

    async fn reencrypt_record(
        &self,
        rewriter: &dyn Rewriter,
        record: &Record,
        options: &RotateOptions,
        report: &mut Report,
    ) -> Result<(), BoxError> {
        let stale = self.needs_rotation(&record.sealed)?;
        if !stale && !options.blind_index {
            report.skipped += 1;
            return Ok(());
        }
        let aad = Aad::new(rewriter.table(), rewriter.column(), &record.id);
        let plaintext = self.decrypt(&record.sealed, &aad)?;
        let new_sealed = if stale {
            self.encrypt(&plaintext, &aad)?
        } else {
            record.sealed.clone()
        };
        let index = if options.blind_index {
            Some(self.blind_index(rewriter.table(), rewriter.column(), &plaintext)?)
        } else {
            None
        };
        let update = Update {
            id: record.id.clone(),
            old_sealed: record.sealed.clone(),
            new_sealed,
            index,
        };
        if !rewriter.replace(&update).await? {
            report.conflicts += 1;
            return Ok(());
        }
        if stale {
            report.rotated += 1;
        }
        if options.blind_index {
            report.reindexed += 1;
        }
        Ok(())
    }
}

fn stop(report: Report, failure: ReencryptFailure) -> ReencryptError {
    ReencryptError { report, failure }
}
