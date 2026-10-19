//! The job contract: typed jobs, handlers, execution context and errors.

use std::{fmt, sync::atomic::AtomicBool, time::Duration};

use async_trait::async_trait;
use forge_core::{Cancellation, Id, Timestamp};
use forge_security::PrincipalId;
use forge_tenancy::{TenantContext, TenantId};
use serde::{Serialize, de::DeserializeOwned};
use uuid::Uuid;

use crate::Backoff;

/// Marker for job identifiers.
#[derive(Debug)]
pub enum JobMarker {}

/// UUIDv7 job identifier, generated in Rust at enqueue time.
pub type JobId = Id<JobMarker>;

/// A durable unit of work: its serialized form is the job payload.
///
/// The associated constants are the job's contract with the queue. `NAME` is
/// what the database stores to route the payload to its handler, so it must
/// stay stable across releases (rename by registering both names during a
/// rollout). Payloads are persisted as JSON in plain text: never put secrets,
/// credentials or tokens in them; store a reference instead.
///
/// ```
/// use std::time::Duration;
/// use forge_jobs::{Backoff, Job};
/// use serde::{Deserialize, Serialize};
///
/// #[derive(Serialize, Deserialize)]
/// struct SendWelcomeEmail {
///     user_id: uuid::Uuid,
/// }
///
/// impl Job for SendWelcomeEmail {
///     const NAME: &'static str = "accounts.send_welcome_email";
///     const QUEUE: &'static str = "mailers";
///     const MAX_ATTEMPTS: u16 = 8;
///     const TIMEOUT: Duration = Duration::from_secs(30);
///     const BACKOFF: Backoff = Backoff::exponential(Duration::from_secs(10), Duration::from_secs(600));
/// }
/// ```
pub trait Job: Serialize + DeserializeOwned + Send + Sync + 'static {
    /// Stable job name: lowercase letters, digits and `_ . : -`, at most 128
    /// bytes.
    const NAME: &'static str;
    /// Queue the job is enqueued on (same grammar, at most 64 bytes).
    const QUEUE: &'static str = "default";
    /// Attempts before the job is moved to the dead state (1 to 1000).
    const MAX_ATTEMPTS: u16 = 10;
    /// Deadline of one attempt. An attempt that exceeds it is cancelled and
    /// counts as a retryable failure.
    const TIMEOUT: Duration = Duration::from_secs(300);
    /// Default priority; lower values run first.
    const PRIORITY: i16 = 0;
    /// Delay between failed attempts.
    const BACKOFF: Backoff = Backoff::DEFAULT;
}

/// Runs jobs of one type.
///
/// Handlers are adapters: they translate the job into application use-case
/// calls, like HTTP handlers do for requests. Delivery is at-least-once, so
/// a handler must be idempotent: an attempt can run again after a crash, a
/// lost lease or a failed acknowledgement. For database-only effects,
/// [`complete_in`](crate::postgres::complete_in) commits the effects and the
/// completion atomically.
#[async_trait]
pub trait JobHandler: Send + Sync + 'static {
    /// The job type this handler runs.
    type Job: Job;

    /// Performs one attempt.
    async fn perform(&self, context: &JobContext, job: Self::Job) -> Result<(), JobError>;
}

/// The tenant scope a job was enqueued under.
///
/// It is recorded from the authorized [`TenantContext`] at enqueue time and
/// cannot be constructed by application code. The worker installs it as the
/// transaction-local `forge.tenant_id`/`forge.principal_id` settings in
/// [`begin_job_transaction`](crate::postgres::begin_job_transaction), so
/// row-level security scopes the handler's queries exactly like the request
/// that enqueued the job.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JobTenant {
    tenant_id: TenantId,
    principal_id: PrincipalId,
}

impl JobTenant {
    pub(crate) const fn new(tenant_id: TenantId, principal_id: PrincipalId) -> Self {
        Self {
            tenant_id,
            principal_id,
        }
    }

    pub(crate) fn of(context: &TenantContext) -> Self {
        Self::new(context.tenant_id(), context.principal_id())
    }

    /// The tenant that owns the job.
    #[must_use]
    pub const fn tenant_id(&self) -> TenantId {
        self.tenant_id
    }

    /// The principal that enqueued the job.
    #[must_use]
    pub const fn principal_id(&self) -> PrincipalId {
        self.principal_id
    }
}

/// Fencing token of one claim. Every state change of a claimed job must
/// present it, so a worker whose lease expired and was reclaimed cannot
/// complete or fail the job afterwards.
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub(crate) struct LeaseToken(pub(crate) Uuid);

impl LeaseToken {
    pub(crate) fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl fmt::Debug for LeaseToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("LeaseToken(..)")
    }
}

/// What a handler knows about the attempt it is running.
#[derive(Debug)]
pub struct JobContext {
    id: JobId,
    name: &'static str,
    queue: &'static str,
    attempt: u32,
    max_attempts: u32,
    tenant: Option<JobTenant>,
    pub(crate) lease: LeaseToken,
    cancellation: Cancellation,
    /// Set by `postgres::complete_in`; only used to classify a later
    /// fenced-out acknowledgement in logs.
    pub(crate) completed_in_transaction: AtomicBool,
}

impl JobContext {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        id: JobId,
        name: &'static str,
        queue: &'static str,
        attempt: u32,
        max_attempts: u32,
        tenant: Option<JobTenant>,
        lease: LeaseToken,
        cancellation: Cancellation,
    ) -> Self {
        Self {
            id,
            name,
            queue,
            attempt,
            max_attempts,
            tenant,
            lease,
            cancellation,
            completed_in_transaction: AtomicBool::new(false),
        }
    }

    /// The job's identifier.
    #[must_use]
    pub const fn id(&self) -> JobId {
        self.id
    }

    /// The job's registered name.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        self.name
    }

    /// The queue the job was claimed from.
    #[must_use]
    pub const fn queue(&self) -> &'static str {
        self.queue
    }

    /// This attempt's number, starting at 1.
    #[must_use]
    pub const fn attempt(&self) -> u32 {
        self.attempt
    }

    /// The attempt budget recorded at enqueue time.
    #[must_use]
    pub const fn max_attempts(&self) -> u32 {
        self.max_attempts
    }

    /// Whether a failure of this attempt moves the job to the dead state.
    #[must_use]
    pub const fn is_final_attempt(&self) -> bool {
        self.attempt >= self.max_attempts
    }

    /// The tenant scope, for tenant-scoped jobs.
    #[must_use]
    pub const fn tenant(&self) -> Option<&JobTenant> {
        self.tenant.as_ref()
    }

    /// Cancelled when the attempt must stop: its timeout elapsed, its lease
    /// was lost, or the worker is shutting down past its drain deadline.
    /// Long-running handlers should check it between steps.
    #[must_use]
    pub const fn cancellation(&self) -> &Cancellation {
        &self.cancellation
    }
}

/// Why an attempt failed, and whether to retry it.
///
/// Any `std::error::Error` converts into a retryable `JobError` with `?`; its
/// display text (and its sources') becomes the recorded error, so use errors
/// whose messages carry no payload data or secrets, such as Forge's
/// database errors. Messages are truncated to 1024 bytes and control
/// characters are replaced.
#[derive(Debug)]
pub struct JobError {
    message: String,
    disposition: Disposition,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Disposition {
    Retry(Option<Duration>),
    Discard,
}

/// Longest stored error message, in bytes.
pub(crate) const MAX_ERROR_LEN: usize = 1024;

impl JobError {
    /// A failure worth retrying after the job's backoff delay.
    #[must_use]
    pub fn retry(message: impl fmt::Display) -> Self {
        Self {
            message: sanitize(&message.to_string()),
            disposition: Disposition::Retry(None),
        }
    }

    /// A failure that retrying cannot fix: the job goes straight to the dead
    /// state, where an operator can inspect, retry or discard it.
    #[must_use]
    pub fn permanent(message: impl fmt::Display) -> Self {
        Self {
            message: sanitize(&message.to_string()),
            disposition: Disposition::Discard,
        }
    }

    /// Retries after `delay` instead of the backoff delay (e.g. a
    /// rate-limited dependency's `Retry-After`). No effect on a permanent
    /// error.
    #[must_use]
    pub fn retry_after(mut self, delay: Duration) -> Self {
        if let Disposition::Retry(_) = self.disposition {
            self.disposition = Disposition::Retry(Some(delay));
        }
        self
    }

    /// The recorded message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Whether the job will be retried (while attempts remain).
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        matches!(self.disposition, Disposition::Retry(_))
    }

    pub(crate) const fn disposition(&self) -> Disposition {
        self.disposition
    }
}

impl<E: std::error::Error + Send + Sync + 'static> From<E> for JobError {
    fn from(error: E) -> Self {
        let mut message = error.to_string();
        let mut source = error.source();
        while let Some(cause) = source {
            message.push_str(": ");
            message.push_str(&cause.to_string());
            source = cause.source();
        }
        Self::retry(message)
    }
}

impl fmt::Display for JobError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

/// Truncates to [`MAX_ERROR_LEN`] bytes on a character boundary and replaces
/// control characters, so a stored message cannot forge log lines.
pub(crate) fn sanitize(message: &str) -> String {
    let mut out = String::with_capacity(message.len().min(MAX_ERROR_LEN));
    for character in message.chars() {
        let character = if character.is_control() {
            ' '
        } else {
            character
        };
        if out.len() + character.len_utf8() > MAX_ERROR_LEN {
            break;
        }
        out.push(character);
    }
    out
}

/// Options for one enqueue.
///
/// ```
/// use std::time::Duration;
/// use forge_jobs::EnqueueOptions;
///
/// let options = EnqueueOptions::new()
///     .delay(Duration::from_secs(60))
///     .priority(-5)
///     .unique_key("welcome-email:42");
/// # let _ = options;
/// ```
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct EnqueueOptions {
    pub(crate) run_at: Option<Timestamp>,
    pub(crate) delay: Option<Duration>,
    pub(crate) priority: Option<i16>,
    pub(crate) unique_key: Option<String>,
}

impl EnqueueOptions {
    /// Runs as soon as a worker is free, with the job's default priority.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Runs no earlier than `time` (database clock).
    #[must_use]
    pub fn at(mut self, time: Timestamp) -> Self {
        self.run_at = Some(time);
        self.delay = None;
        self
    }

    /// Runs no earlier than `delay` after the enqueueing transaction's start
    /// (database clock).
    #[must_use]
    pub fn delay(mut self, delay: Duration) -> Self {
        self.delay = Some(delay);
        self.run_at = None;
        self
    }

    /// Overrides [`Job::PRIORITY`]; lower values run first.
    #[must_use]
    pub fn priority(mut self, priority: i16) -> Self {
        self.priority = Some(priority);
        self
    }

    /// Idempotency key: while a job with the same key exists in the same
    /// tenant scope (until finished jobs are pruned), enqueueing again
    /// returns [`Enqueued::Duplicate`] with the existing job's id instead of
    /// inserting. 1 to 255 bytes.
    #[must_use]
    pub fn unique_key(mut self, key: impl Into<String>) -> Self {
        self.unique_key = Some(key.into());
        self
    }
}

/// Result of an enqueue.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Enqueued {
    /// A new job was inserted.
    New(JobId),
    /// A job with the same unique key already exists.
    Duplicate(JobId),
}

impl Enqueued {
    /// The new or existing job's identifier.
    #[must_use]
    pub const fn id(&self) -> JobId {
        match *self {
            Self::New(id) | Self::Duplicate(id) => id,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_are_classified_and_sanitized() {
        let retry = JobError::retry("smtp\nunavailable").retry_after(Duration::from_secs(5));
        assert_eq!(retry.message(), "smtp unavailable");
        assert_eq!(
            retry.disposition(),
            Disposition::Retry(Some(Duration::from_secs(5)))
        );
        let permanent = JobError::permanent("invalid address").retry_after(Duration::ZERO);
        assert!(!permanent.is_retryable());
        assert_eq!(permanent.disposition(), Disposition::Discard);

        let long = "é".repeat(2000);
        let truncated = sanitize(&long);
        assert!(truncated.len() <= MAX_ERROR_LEN);
        assert!(truncated.chars().all(|c| c == 'é'));
    }

    #[test]
    fn std_errors_convert_with_their_sources() {
        #[derive(Debug, thiserror::Error)]
        #[error("outer")]
        struct Outer(#[source] std::io::Error);

        let error = JobError::from(Outer(std::io::Error::other("inner")));
        assert_eq!(error.message(), "outer: inner");
        assert!(error.is_retryable());
    }

    #[test]
    fn enqueue_options_keep_the_last_schedule() {
        let at = Timestamp::from_unix_micros(10).expect("timestamp");
        let options = EnqueueOptions::new().delay(Duration::from_secs(1)).at(at);
        assert_eq!(options.run_at, Some(at));
        assert_eq!(options.delay, None);
        let options = options.delay(Duration::from_secs(2));
        assert_eq!(options.run_at, None);
        let id = JobId::new();
        assert_eq!(Enqueued::Duplicate(id).id(), id);
        assert_eq!(format!("{:?}", LeaseToken::new()), "LeaseToken(..)");
    }
}
