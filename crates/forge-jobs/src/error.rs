//! Errors of the job system.

use forge_db::DbError;
use thiserror::Error;

use crate::CronError;

/// A job-system failure. Messages never contain payloads or connection URLs.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum JobsError {
    /// A queue, job or schedule name does not follow the name grammar.
    #[error("invalid {kind} name {name:?}")]
    InvalidName {
        /// `queue`, `job` or `schedule`.
        kind: &'static str,
        /// The rejected name (truncated).
        name: String,
    },
    /// Two handlers were registered for the same job name.
    #[error("a handler for job {0:?} is already registered")]
    DuplicateHandler(&'static str),
    /// Two schedules were registered with the same name.
    #[error("schedule {0:?} is already registered")]
    DuplicateSchedule(String),
    /// A schedule enqueues a job that has no registered handler.
    #[error("schedule {schedule:?} enqueues job {job:?}, which has no registered handler")]
    UnhandledSchedule {
        /// The schedule.
        schedule: String,
        /// The job it enqueues.
        job: &'static str,
    },
    /// A setting is out of its documented range.
    #[error("invalid job setting: {0}")]
    InvalidSetting(&'static str),
    /// The job could not be serialized to JSON.
    #[error("the job payload could not be serialized")]
    Serialize(#[source] serde_json::Error),
    /// The serialized job exceeds [`MAX_PAYLOAD_BYTES`](crate::MAX_PAYLOAD_BYTES).
    #[error("the job payload exceeds {limit} bytes")]
    PayloadTooLarge {
        /// The limit in bytes.
        limit: usize,
    },
    /// An invalid cron expression.
    #[error(transparent)]
    Cron(#[from] CronError),
    /// The database rejected or failed an operation.
    #[error("job store operation failed")]
    Database(#[from] DbError),
    /// A persisted row violates the job contract (e.g. a non-v7 identifier).
    #[error("persisted job data is invalid")]
    Corrupt,
    /// The worker no longer holds the job's lease: it expired and the job was
    /// reclaimed, or the job already finished.
    #[error("the job's lease is no longer held")]
    LeaseLost,
    /// The selected database backend cannot run durable jobs (ADR 0025).
    #[error(
        "{capability} require PostgreSQL; the {backend} backend is refused rather than emulated (ADR 0025)"
    )]
    UnsupportedBackend {
        /// The refused capability, e.g. `durable jobs`.
        capability: &'static str,
        /// The declared backend.
        backend: &'static str,
    },
    /// A supervised background loop panicked.
    #[error("background task {0} failed")]
    TaskFailed(&'static str),
}

impl From<forge_db::sqlx::Error> for JobsError {
    fn from(error: forge_db::sqlx::Error) -> Self {
        Self::Database(DbError::from(error))
    }
}

/// Refuses durable jobs, schedules and the outbox on backends other than
/// PostgreSQL (ADR 0021 capability matrix, ADR 0025).
///
/// ```
/// use forge_config::DatabaseScheme;
///
/// assert!(forge_jobs::ensure_supported(DatabaseScheme::Postgres).is_ok());
/// assert!(forge_jobs::ensure_supported(DatabaseScheme::Sqlite).is_err());
/// ```
pub fn ensure_supported(scheme: forge_config::DatabaseScheme) -> Result<(), JobsError> {
    match scheme {
        forge_config::DatabaseScheme::Postgres => Ok(()),
        other => Err(JobsError::UnsupportedBackend {
            capability: "durable jobs",
            backend: other.name(),
        }),
    }
}
