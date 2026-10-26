//! Durable jobs, workers and cron schedules for Forge (ADR 0025).
//!
//! # Contract
//!
//! A [`Job`] is a serializable value with a stable name, queue, attempt
//! budget, timeout, priority and [`Backoff`]. A [`JobHandler`] runs it. Jobs
//! are enqueued **inside the application's transaction**
//! ([`postgres::enqueue`], [`postgres::enqueue_for_tenant`]), so they commit
//! or roll back with the business writes that imply them, optionally with an
//! idempotency key ([`EnqueueOptions::unique_key`]), a run time and a
//! priority.
//!
//! # Guarantees
//!
//! - **At-least-once execution.** Workers claim jobs with
//!   `FOR UPDATE SKIP LOCKED` under a lease. A job whose worker dies is
//!   reclaimed when the lease expires and counts the lost attempt.
//! - **Fencing.** Each claim carries a fresh lease token; completion,
//!   failure, release and renewal must present it, so a stale worker cannot
//!   acknowledge a job that was reclaimed. A worker that loses a lease
//!   cancels the attempt.
//! - **Exactly-once effects for database work.** [`postgres::complete_in`]
//!   marks the job completed in the handler's own transaction.
//! - **Bounded retries.** Failed attempts are retried after capped
//!   exponential backoff with jitter; when attempts are exhausted (or the
//!   handler returns [`JobError::permanent`]) the job is **dead** until an
//!   operator retries or discards it.
//! - **Exactly-once schedule ticks** across replicas ([`Schedule`]).
//! - **Tenant isolation.** Tenant-scoped jobs carry the enqueuing
//!   [`TenantContext`](forge_tenancy::TenantContext)'s tenant and principal;
//!   row-level security confines every non-worker query to the
//!   transaction's tenant scope, and the worker reinstalls the scope for the
//!   handler ([`postgres::begin_job_transaction`]).
//!
//! # Lifecycle
//!
//! [`Jobs::run`] owns every task it starts: one bounded worker loop per
//! queue, the scheduler, the pruner and, when the store's connections carry
//! [`postgres::NOTIFY_SETTING`] on, the listener that wakes idle workers as
//! soon as a due job is enqueued (ADR 0046). Each claim leases at most
//! [`WorkerSettings::batch`] jobs and never more than the queue's free slots. On shutdown it stops claiming, waits
//! for in-flight attempts up to a deadline, cancels the remaining ones
//! cooperatively, then aborts them and releases their leases. A panicking
//! handler is recorded as a failed attempt; the worker keeps running. Logs
//! carry job ids, names, queues and attempts, never payloads; a panic hook
//! keeps attempt panic messages off stderr ([`panics`]).
//!
//! # Backends
//!
//! PostgreSQL only. MySQL/MariaDB and SQLite are refused explicitly with
//! [`ensure_supported`] instead of being emulated (ADR 0021, ADR 0025).

#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]
#![deny(missing_docs)]

mod backoff;
mod cron;
mod error;
mod job;
mod listener;
mod name;
pub mod panics;
#[cfg(test)]
mod pg_notify_tests;
#[cfg(test)]
mod pg_tests;
pub mod postgres;
mod runtime;
mod scheduler;
#[cfg(any(test, feature = "testing"))]
pub mod testing;
mod worker;

pub use backoff::Backoff;
pub use cron::{Cron, CronError};
pub use error::{JobsError, ensure_supported};
pub use job::{
    EnqueueOptions, Enqueued, Job, JobContext, JobError, JobHandler, JobId, JobMarker, JobTenant,
};
pub use name::{MAX_JOB_NAME, MAX_QUEUE_NAME, is_valid as is_valid_name};
pub use runtime::{Jobs, MAX_CLAIM_BATCH, MAX_CONCURRENCY, WorkerSettings};
pub use scheduler::Schedule;

/// Largest serialized job payload, in bytes (256 KiB). Larger payloads are
/// rejected at enqueue time: store large data elsewhere and enqueue a
/// reference.
pub const MAX_PAYLOAD_BYTES: usize = 256 * 1024;
