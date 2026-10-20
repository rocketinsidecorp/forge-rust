//! PostgreSQL adapter: schema, enqueue, claiming with fenced leases and the
//! operator queries.
//!
//! Every cross-tenant operation (claim, heartbeat, complete, fail, release,
//! and the operator commands) goes through a `SECURITY DEFINER` function
//! created by [`migrator`]; the runtime role itself only has `SELECT` and
//! `INSERT` on `forge_jobs`, filtered by row-level security to the
//! transaction's tenant scope.

use std::{borrow::Cow, sync::atomic::Ordering, time::Duration};

use forge_core::{Id, Timestamp};
use forge_db::{DbTransaction, Migrator, PgDatabase};
use forge_tenancy::{TenantContext, TenantId};
use sqlx::{PgConnection, Postgres, Row, postgres::PgRow};
use uuid::Uuid;

use crate::{
    EnqueueOptions, Enqueued, Job, JobContext, JobId, JobTenant, JobsError, MAX_PAYLOAD_BYTES,
    job::LeaseToken, name,
};

/// Bookkeeping table of the framework job schema, separate from the
/// application's `_sqlx_migrations`.
pub const MIGRATIONS_TABLE: &str = "_forge_jobs_migrations";

/// The embedded, reversible job schema (`forge_jobs`, `forge_job_schedules`,
/// row-level security, the `SECURITY DEFINER` functions and runtime grants).
///
/// Apply it with the migration role, e.g. `database.migrate(&migrator())`;
/// `forge::App::jobs` does so in the application's `migrate` command. The
/// runtime role that receives grants is read from the `forge.runtime_role`
/// setting (`ALTER DATABASE app SET forge.runtime_role = 'svc_runtime'`) and
/// defaults to `app_runtime`; an explicitly named role that does not exist
/// fails the migration.
#[must_use]
pub fn migrator() -> Migrator {
    use sqlx::{
        SqlSafeStr,
        migrate::{Migration, MigrationType},
    };

    let migration = |kind, sql: &'static str| {
        Migration::new(
            1,
            Cow::Borrowed("forge jobs"),
            kind,
            sql.into_sql_str(),
            false,
        )
    };
    let mut migrator = Migrator::with_migrations(vec![
        migration(
            MigrationType::ReversibleUp,
            include_str!("../migrations/postgres/1_forge_jobs.up.sql"),
        ),
        migration(
            MigrationType::ReversibleDown,
            include_str!("../migrations/postgres/1_forge_jobs.down.sql"),
        ),
    ]);
    migrator.table_name = Cow::Borrowed(MIGRATIONS_TABLE);
    migrator
}

/// Idempotency-key prefix reserved for schedule ticks.
pub(crate) const SCHEDULE_KEY_PREFIX: &str = "forge.schedule:";

const ZERO_TENANT: &str = "00000000-0000-0000-0000-000000000000";

/// Enqueues a global (tenant-less) job in `transaction`.
///
/// The job becomes visible to workers only when the transaction commits and
/// disappears if it rolls back, so business writes and the jobs they imply
/// are atomic. Under row-level security a transaction that carries a tenant
/// scope cannot insert global jobs; use [`enqueue_for_tenant`] there.
pub async fn enqueue<J: Job>(
    transaction: &mut DbTransaction<Postgres>,
    job: &J,
    options: EnqueueOptions,
) -> Result<Enqueued, JobsError> {
    let new = NewJob::of(job, &options, None)?;
    insert(transaction.connection(), &new).await
}

/// Enqueues a job owned by the authorized tenant in `transaction`.
///
/// The transaction must carry the same tenant scope (the transaction-local
/// `forge.tenant_id`/`forge.principal_id` settings of ADR 0011); otherwise
/// row-level security rejects the insert. The worker re-installs the scope
/// when it runs the job.
pub async fn enqueue_for_tenant<J: Job>(
    transaction: &mut DbTransaction<Postgres>,
    context: &TenantContext,
    job: &J,
    options: EnqueueOptions,
) -> Result<Enqueued, JobsError> {
    let new = NewJob::of(job, &options, Some(JobTenant::of(context)))?;
    insert(transaction.connection(), &new).await
}

/// Begins a transaction for a handler's database work: for a tenant-scoped
/// job it installs the job's tenant and principal as transaction-local
/// settings, so row-level security applies exactly as for the request that
/// enqueued it.
pub async fn begin_job_transaction(
    database: &PgDatabase,
    context: &JobContext,
) -> Result<DbTransaction<Postgres>, JobsError> {
    let mut transaction = database.begin().await?;
    if let Some(tenant) = context.tenant() {
        set_tenant_scope(transaction.connection(), tenant).await?;
    }
    Ok(transaction)
}

/// Marks the running job completed inside `transaction`, so the handler's
/// database effects and the completion commit (or roll back) together.
///
/// Fails with [`JobsError::LeaseLost`] when the lease was lost; the caller
/// must then roll back, because another worker owns the job.
pub async fn complete_in(
    transaction: &mut DbTransaction<Postgres>,
    context: &JobContext,
) -> Result<(), JobsError> {
    let completed: bool = sqlx::query_scalar("SELECT forge_jobs_complete($1, $2)")
        .bind(context.id().into_uuid())
        .bind(context.lease.0)
        .fetch_one(transaction.connection())
        .await?;
    if completed {
        context
            .completed_in_transaction
            .store(true, Ordering::Release);
        Ok(())
    } else {
        Err(JobsError::LeaseLost)
    }
}

async fn set_tenant_scope(
    connection: &mut PgConnection,
    tenant: &JobTenant,
) -> Result<(), JobsError> {
    sqlx::query(
        "SELECT set_config('forge.tenant_id', $1, true), \
         set_config('forge.principal_id', $2, true)",
    )
    .bind(tenant.tenant_id().to_string())
    .bind(tenant.principal_id().to_string())
    .execute(connection)
    .await?;
    Ok(())
}

/// A validated row to insert.
pub(crate) struct NewJob {
    pub(crate) id: JobId,
    pub(crate) tenant: Option<JobTenant>,
    pub(crate) queue: &'static str,
    pub(crate) name: &'static str,
    pub(crate) payload: String,
    pub(crate) priority: i16,
    pub(crate) max_attempts: i32,
    pub(crate) run_at_us: Option<i64>,
    pub(crate) delay_ms: i64,
    pub(crate) unique_key: Option<String>,
}

/// The queue contract of a job type, captured from its [`Job`] constants.
#[derive(Clone, Copy, Debug)]
pub(crate) struct JobSpec {
    pub(crate) name: &'static str,
    pub(crate) queue: &'static str,
    pub(crate) max_attempts: u16,
    pub(crate) priority: i16,
}

impl JobSpec {
    pub(crate) const fn of<J: Job>() -> Self {
        Self {
            name: J::NAME,
            queue: J::QUEUE,
            max_attempts: J::MAX_ATTEMPTS,
            priority: J::PRIORITY,
        }
    }

    /// Checks the names and attempt budget.
    pub(crate) fn validate(&self) -> Result<i32, JobsError> {
        name::job(self.name)?;
        name::queue(self.queue)?;
        validate_attempts(self.max_attempts)
    }
}

impl NewJob {
    pub(crate) fn of<J: Job>(
        job: &J,
        options: &EnqueueOptions,
        tenant: Option<JobTenant>,
    ) -> Result<Self, JobsError> {
        if options
            .unique_key
            .as_deref()
            .is_some_and(|key| key.starts_with(SCHEDULE_KEY_PREFIX))
        {
            return Err(JobsError::InvalidSetting(
                "unique keys starting with `forge.schedule:` are reserved for schedules",
            ));
        }
        Self::from_parts(JobSpec::of::<J>(), serialize(job)?, options, tenant)
    }

    pub(crate) fn from_parts(
        spec: JobSpec,
        payload: String,
        options: &EnqueueOptions,
        tenant: Option<JobTenant>,
    ) -> Result<Self, JobsError> {
        let max_attempts = spec.validate()?;
        if let Some(key) = &options.unique_key
            && (key.is_empty() || key.len() > 255)
        {
            return Err(JobsError::InvalidSetting(
                "unique keys must be 1 to 255 bytes",
            ));
        }
        let run_at_us = options.run_at.map(Timestamp::unix_micros);
        Ok(Self {
            id: JobId::new(),
            tenant,
            queue: spec.queue,
            name: spec.name,
            payload,
            priority: options.priority.unwrap_or(spec.priority),
            max_attempts,
            run_at_us,
            delay_ms: options.delay.map_or(0, millis),
            unique_key: options.unique_key.clone(),
        })
    }
}

pub(crate) fn validate_attempts(max_attempts: u16) -> Result<i32, JobsError> {
    if (1..=1000).contains(&max_attempts) {
        Ok(i32::from(max_attempts))
    } else {
        Err(JobsError::InvalidSetting(
            "MAX_ATTEMPTS must be between 1 and 1000",
        ))
    }
}

pub(crate) fn serialize<J: serde::Serialize>(job: &J) -> Result<String, JobsError> {
    let payload = serde_json::to_string(job).map_err(JobsError::Serialize)?;
    if payload.len() > MAX_PAYLOAD_BYTES {
        return Err(JobsError::PayloadTooLarge {
            limit: MAX_PAYLOAD_BYTES,
        });
    }
    Ok(payload)
}

pub(crate) async fn insert(
    connection: &mut PgConnection,
    job: &NewJob,
) -> Result<Enqueued, JobsError> {
    let tenant_id = job.tenant.map(|tenant| tenant.tenant_id().into_uuid());
    let principal_id = job.tenant.map(|tenant| tenant.principal_id().into_uuid());
    let inserted: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO forge_jobs (
             id, tenant_id, principal_id, queue, job_name, payload, priority,
             max_attempts, run_at, unique_key
         ) VALUES (
             $1, $2, $3, $4, $5, $6::jsonb, $7, $8,
             CASE WHEN $9::bigint IS NULL
                  THEN now() + $10::bigint * interval '1 millisecond'
                  ELSE timestamptz 'epoch' + $9::bigint * interval '1 microsecond'
             END,
             $11
         )
         ON CONFLICT ((coalesce(tenant_id, '00000000-0000-0000-0000-000000000000'::uuid)), unique_key)
             WHERE unique_key IS NOT NULL
         DO NOTHING
         RETURNING id",
    )
    .bind(job.id.into_uuid())
    .bind(tenant_id)
    .bind(principal_id)
    .bind(job.queue)
    .bind(job.name)
    .bind(&job.payload)
    .bind(job.priority)
    .bind(job.max_attempts)
    .bind(job.run_at_us)
    .bind(job.delay_ms)
    .bind(job.unique_key.as_deref())
    .fetch_optional(&mut *connection)
    .await
    .map_err(payload_error)?;
    if inserted.is_some() {
        return Ok(Enqueued::New(job.id));
    }
    let existing: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM forge_jobs
         WHERE coalesce(tenant_id, $1::uuid) = coalesce($2, $1::uuid) AND unique_key = $3",
    )
    .bind(ZERO_TENANT)
    .bind(tenant_id)
    .bind(job.unique_key.as_deref())
    .fetch_optional(connection)
    .await?;
    let existing = existing.ok_or(JobsError::Corrupt)?;
    Ok(Enqueued::Duplicate(job_id(existing)?))
}

/// A job claimed by a worker, still undecoded.
#[derive(Debug)]
pub(crate) struct ClaimedJob {
    pub(crate) id: JobId,
    pub(crate) tenant_id: Option<Uuid>,
    pub(crate) principal_id: Option<Uuid>,
    pub(crate) name: String,
    pub(crate) payload: String,
    pub(crate) attempt: u32,
    pub(crate) max_attempts: u32,
}

impl ClaimedJob {
    /// The tenant scope, or `Err` when the stored scope is inconsistent.
    pub(crate) fn tenant(&self) -> Result<Option<JobTenant>, JobsError> {
        match (self.tenant_id, self.principal_id) {
            (None, None) => Ok(None),
            (Some(tenant), Some(principal)) => Ok(Some(JobTenant::new(
                Id::from_uuid(tenant).map_err(|_| JobsError::Corrupt)?,
                Id::from_uuid(principal).map_err(|_| JobsError::Corrupt)?,
            ))),
            _ => Err(JobsError::Corrupt),
        }
    }
}

/// Result of renewing one lease.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LeaseStatus {
    /// Extended.
    Renewed,
    /// The row is locked by another transaction (usually the attempt's own
    /// `complete_in`); not extended, not lost.
    Busy,
    /// The token no longer matches: reclaimed, or finished.
    Lost,
}

/// State after a failed attempt was recorded.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AfterFailure {
    Retrying,
    Dead,
}

/// Outcome of firing one schedule tick.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TickOutcome {
    /// This replica won the tick and enqueued the job.
    Fired(JobId),
    /// This replica won the tick but it was older than the allowed lateness.
    Skipped,
    /// Another replica (or an earlier run) already handled this tick.
    AlreadyHandled,
    /// The tick is later than the database clock: not recorded.
    Early,
}

/// Lifecycle state of a job.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum JobState {
    /// Waiting for its run time or a free worker.
    Queued,
    /// Leased by a worker.
    Running,
    /// Finished successfully (kept until pruned).
    Completed,
    /// Out of attempts or failed permanently; waits for an operator.
    Dead,
}

impl JobState {
    fn parse(value: &str) -> Result<Self, JobsError> {
        match value {
            "queued" => Ok(Self::Queued),
            "running" => Ok(Self::Running),
            "completed" => Ok(Self::Completed),
            "dead" => Ok(Self::Dead),
            _ => Err(JobsError::Corrupt),
        }
    }

    /// The stored name: `queued`, `running`, `completed` or `dead`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Dead => "dead",
        }
    }
}

/// Job counts of one queue and state.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct QueueStats {
    /// The queue.
    pub queue: String,
    /// The state counted.
    pub state: JobState,
    /// Number of jobs.
    pub jobs: u64,
    /// For queued jobs, the earliest run time (queue latency indicator).
    pub oldest_run_at: Option<Timestamp>,
}

/// A dead job as shown to operators. Payloads are never exposed.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct DeadJob {
    /// The job.
    pub id: JobId,
    /// The owning tenant, for tenant-scoped jobs.
    pub tenant_id: Option<TenantId>,
    /// The queue.
    pub queue: String,
    /// The job name.
    pub name: String,
    /// Attempts made.
    pub attempts: u32,
    /// Attempt budget.
    pub max_attempts: u32,
    /// The last recorded error message.
    pub last_error: Option<String>,
    /// When the job entered the dead state.
    pub failed_at: Option<Timestamp>,
}

/// Retention for finished jobs, applied by a bounded background loop.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct Pruning {
    /// Completed jobs older than this are deleted (default one day). Their
    /// unique keys become reusable at that point.
    pub completed_after: Duration,
    /// Dead jobs older than this are deleted; `None` (the default) keeps them
    /// until an operator retries or discards them.
    pub dead_after: Option<Duration>,
    /// Time between pruning runs (default ten minutes).
    pub interval: Duration,
    /// Rows deleted per statement (default 1000, at most 10000).
    pub batch: u32,
    /// Statements per run (default 20), bounding one run's work.
    pub max_batches: u32,
}

impl Default for Pruning {
    fn default() -> Self {
        Self {
            completed_after: Duration::from_secs(24 * 3600),
            dead_after: None,
            interval: Duration::from_secs(600),
            batch: 1000,
            max_batches: 20,
        }
    }
}

impl Pruning {
    /// Sets how long completed jobs are kept.
    #[must_use]
    pub const fn completed_after(mut self, retention: Duration) -> Self {
        self.completed_after = retention;
        self
    }

    /// Sets how long dead jobs are kept (`None`: until an operator acts).
    #[must_use]
    pub const fn dead_after(mut self, retention: Option<Duration>) -> Self {
        self.dead_after = retention;
        self
    }

    /// Sets the time between runs.
    #[must_use]
    pub const fn interval(mut self, interval: Duration) -> Self {
        self.interval = interval;
        self
    }
}

/// The PostgreSQL job store used by workers and operator commands.
///
/// Connect it with the **runtime** role: everything it does goes through the
/// functions the schema grants to that role.
#[derive(Clone, Debug)]
pub struct PgJobStore {
    database: PgDatabase,
}

impl PgJobStore {
    /// Creates a store over the application's runtime pool.
    #[must_use]
    pub const fn new(database: PgDatabase) -> Self {
        Self { database }
    }

    /// The underlying pool.
    #[must_use]
    pub const fn database(&self) -> &PgDatabase {
        &self.database
    }

    /// Enqueues a global job in its own transaction.
    pub async fn enqueue<J: Job>(
        &self,
        job: &J,
        options: EnqueueOptions,
    ) -> Result<Enqueued, JobsError> {
        let mut transaction = self.database.begin().await?;
        let enqueued = enqueue(&mut transaction, job, options).await?;
        transaction.commit().await?;
        Ok(enqueued)
    }

    /// Job counts per queue and state.
    pub async fn stats(&self) -> Result<Vec<QueueStats>, JobsError> {
        let rows =
            sqlx::query("SELECT queue, state, jobs, oldest_run_at_us FROM forge_jobs_stats()")
                .fetch_all(self.database.pool())
                .await?;
        rows.iter()
            .map(|row| {
                Ok(QueueStats {
                    queue: row.try_get("queue")?,
                    state: JobState::parse(row.try_get::<&str, _>("state")?)?,
                    jobs: u64::try_from(row.try_get::<i64, _>("jobs")?)
                        .map_err(|_| JobsError::Corrupt)?,
                    oldest_run_at: row
                        .try_get::<Option<i64>, _>("oldest_run_at_us")?
                        .map(timestamp)
                        .transpose()?,
                })
            })
            .collect()
    }

    /// Dead jobs, newest first: at most `limit` (1 to 500) jobs older than
    /// `before`, for keyset pagination.
    pub async fn dead(&self, limit: u16, before: Option<JobId>) -> Result<Vec<DeadJob>, JobsError> {
        let rows = sqlx::query(
            "SELECT id, tenant_id, queue, job_name, attempts, max_attempts, last_error, failed_at_us
             FROM forge_jobs_dead($1, $2)",
        )
        .bind(i32::from(limit.clamp(1, 500)))
        .bind(before.map(Id::into_uuid))
        .fetch_all(self.database.pool())
        .await?;
        rows.iter().map(dead_job).collect()
    }

    /// Requeues a dead job with a fresh attempt budget. False when no dead
    /// job has this id.
    pub async fn retry(&self, id: JobId) -> Result<bool, JobsError> {
        Ok(sqlx::query_scalar("SELECT forge_jobs_retry($1)")
            .bind(id.into_uuid())
            .fetch_one(self.database.pool())
            .await?)
    }

    /// Deletes a dead job. False when no dead job has this id.
    pub async fn discard(&self, id: JobId) -> Result<bool, JobsError> {
        Ok(sqlx::query_scalar("SELECT forge_jobs_discard($1)")
            .bind(id.into_uuid())
            .fetch_one(self.database.pool())
            .await?)
    }

    /// Deletes finished jobs past their retention, in bounded batches;
    /// returns the number deleted.
    pub async fn prune(&self, pruning: &Pruning) -> Result<u64, JobsError> {
        let batch = pruning.batch.clamp(1, 10_000);
        let mut total = 0_u64;
        for _ in 0..pruning.max_batches.max(1) {
            let deleted: i64 = sqlx::query_scalar("SELECT forge_jobs_prune($1, $2, $3)")
                .bind(millis(pruning.completed_after))
                .bind(pruning.dead_after.map(millis))
                .bind(i32::try_from(batch).unwrap_or(i32::MAX))
                .fetch_one(self.database.pool())
                .await?;
            let deleted = u64::try_from(deleted).unwrap_or(0);
            total += deleted;
            if deleted < u64::from(batch) {
                break;
            }
        }
        Ok(total)
    }

    pub(crate) async fn claim(
        &self,
        queue: &str,
        names: &[String],
        limit: usize,
        lease: Duration,
        token: LeaseToken,
    ) -> Result<Vec<ClaimedJob>, JobsError> {
        let rows = sqlx::query(
            "SELECT id, tenant_id, principal_id, job_name, payload, attempts, max_attempts
             FROM forge_jobs_claim($1, $2, $3, $4, $5)",
        )
        .bind(queue)
        .bind(names)
        .bind(i32::try_from(limit.clamp(1, 1000)).unwrap_or(1))
        .bind(token.0)
        .bind(millis(lease))
        .fetch_all(self.database.pool())
        .await?;
        rows.iter()
            .map(|row| {
                Ok(ClaimedJob {
                    id: job_id(row.try_get("id")?)?,
                    tenant_id: row.try_get("tenant_id")?,
                    principal_id: row.try_get("principal_id")?,
                    name: row.try_get("job_name")?,
                    payload: row.try_get("payload")?,
                    attempt: count(row.try_get("attempts")?)?,
                    max_attempts: count(row.try_get("max_attempts")?)?,
                })
            })
            .collect()
    }

    pub(crate) async fn heartbeat(
        &self,
        leases: &[(JobId, LeaseToken)],
        lease: Duration,
    ) -> Result<Vec<(JobId, LeaseStatus)>, JobsError> {
        let ids: Vec<Uuid> = leases.iter().map(|(id, _)| id.into_uuid()).collect();
        let tokens: Vec<Uuid> = leases.iter().map(|(_, token)| token.0).collect();
        let rows = sqlx::query("SELECT job_id, status FROM forge_jobs_heartbeat($1, $2, $3)")
            .bind(ids)
            .bind(tokens)
            .bind(millis(lease))
            .fetch_all(self.database.pool())
            .await?;
        rows.iter()
            .map(|row| {
                let status = match row.try_get::<&str, _>("status")? {
                    "renewed" => LeaseStatus::Renewed,
                    "busy" => LeaseStatus::Busy,
                    "lost" => LeaseStatus::Lost,
                    _ => return Err(JobsError::Corrupt),
                };
                Ok((job_id(row.try_get("job_id")?)?, status))
            })
            .collect()
    }

    pub(crate) async fn complete(&self, id: JobId, token: LeaseToken) -> Result<bool, JobsError> {
        Ok(sqlx::query_scalar("SELECT forge_jobs_complete($1, $2)")
            .bind(id.into_uuid())
            .bind(token.0)
            .fetch_one(self.database.pool())
            .await?)
    }

    pub(crate) async fn fail(
        &self,
        id: JobId,
        token: LeaseToken,
        error: &str,
        retry_in: Duration,
        discard: bool,
    ) -> Result<Option<AfterFailure>, JobsError> {
        let state: Option<String> =
            sqlx::query_scalar("SELECT forge_jobs_fail($1, $2, $3, $4, $5)")
                .bind(id.into_uuid())
                .bind(token.0)
                .bind(error)
                .bind(millis(retry_in))
                .bind(discard)
                .fetch_one(self.database.pool())
                .await?;
        match state.as_deref() {
            None => Ok(None),
            Some("queued") => Ok(Some(AfterFailure::Retrying)),
            Some("dead") => Ok(Some(AfterFailure::Dead)),
            Some(_) => Err(JobsError::Corrupt),
        }
    }

    pub(crate) async fn release(&self, id: JobId, token: LeaseToken) -> Result<bool, JobsError> {
        Ok(sqlx::query_scalar("SELECT forge_jobs_release($1, $2)")
            .bind(id.into_uuid())
            .bind(token.0)
            .fetch_one(self.database.pool())
            .await?)
    }

    /// Records `tick` as the schedule's latest and, when this replica wins
    /// it, enqueues the job in the same transaction. The database clock
    /// decides whether the tick is due (`Early`) or too late (`Skipped`).
    pub(crate) async fn fire_tick(
        &self,
        schedule: &str,
        tick: Timestamp,
        lateness: Duration,
        job: NewJob,
    ) -> Result<TickOutcome, JobsError> {
        let outcome: String = sqlx::query_scalar(
            "SELECT forge_jobs_fire_tick($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
        )
        .bind(schedule)
        .bind(tick.unix_micros())
        .bind(millis(lateness))
        .bind(job.id.into_uuid())
        .bind(job.queue)
        .bind(job.name)
        .bind(&job.payload)
        .bind(job.priority)
        .bind(job.max_attempts)
        .bind(job.unique_key.as_deref())
        .fetch_one(self.database.pool())
        .await?;
        match outcome.as_str() {
            "fired" => Ok(TickOutcome::Fired(job.id)),
            "skipped" => Ok(TickOutcome::Skipped),
            "handled" => Ok(TickOutcome::AlreadyHandled),
            "early" => Ok(TickOutcome::Early),
            _ => Err(JobsError::Corrupt),
        }
    }

    /// The database's current time, which schedulers use instead of the
    /// replica clock.
    pub(crate) async fn database_now(&self) -> Result<Timestamp, JobsError> {
        let micros: i64 = sqlx::query_scalar("SELECT forge_jobs_now_us()")
            .fetch_one(self.database.pool())
            .await?;
        timestamp(micros)
    }
}

fn dead_job(row: &PgRow) -> Result<DeadJob, JobsError> {
    Ok(DeadJob {
        id: job_id(row.try_get("id")?)?,
        tenant_id: row
            .try_get::<Option<Uuid>, _>("tenant_id")?
            .map(|tenant| Id::from_uuid(tenant).map_err(|_| JobsError::Corrupt))
            .transpose()?,
        queue: row.try_get("queue")?,
        name: row.try_get("job_name")?,
        attempts: count(row.try_get("attempts")?)?,
        max_attempts: count(row.try_get("max_attempts")?)?,
        last_error: row.try_get("last_error")?,
        failed_at: row
            .try_get::<Option<i64>, _>("failed_at_us")?
            .map(timestamp)
            .transpose()?,
    })
}

/// Maps the database payload cap (measured on the stored JSON text) to
/// [`JobsError::PayloadTooLarge`].
fn payload_error(error: sqlx::Error) -> JobsError {
    match forge_db::DbError::from(error) {
        forge_db::DbError::CheckViolation { constraint }
            if constraint.as_deref() == Some("forge_jobs_payload_size") =>
        {
            JobsError::PayloadTooLarge {
                limit: MAX_PAYLOAD_BYTES,
            }
        }
        other => JobsError::Database(other),
    }
}

fn job_id(value: Uuid) -> Result<JobId, JobsError> {
    Id::from_uuid(value).map_err(|_| JobsError::Corrupt)
}

fn count(value: i32) -> Result<u32, JobsError> {
    u32::try_from(value).map_err(|_| JobsError::Corrupt)
}

/// Whole milliseconds, saturating at `i64::MAX`.
pub(crate) fn millis(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

fn timestamp(micros: i64) -> Result<Timestamp, JobsError> {
    Timestamp::from_unix_micros(micros).map_err(|_| JobsError::Corrupt)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(serde::Serialize, serde::Deserialize)]
    struct Big(String);

    impl Job for Big {
        const NAME: &'static str = "test.big";
    }

    #[derive(serde::Serialize, serde::Deserialize)]
    struct BadName;

    impl Job for BadName {
        const NAME: &'static str = "Bad Name";
    }

    #[derive(serde::Serialize, serde::Deserialize)]
    struct NoAttempts;

    impl Job for NoAttempts {
        const NAME: &'static str = "test.no_attempts";
        const MAX_ATTEMPTS: u16 = 0;
    }

    #[test]
    fn jobs_are_validated_before_touching_the_database() {
        let options = EnqueueOptions::new();
        assert!(matches!(
            NewJob::of(&Big("x".repeat(MAX_PAYLOAD_BYTES)), &options, None),
            Err(JobsError::PayloadTooLarge { .. })
        ));
        assert!(matches!(
            NewJob::of(&BadName, &options, None),
            Err(JobsError::InvalidName { kind: "job", .. })
        ));
        assert!(matches!(
            NewJob::of(&NoAttempts, &options, None),
            Err(JobsError::InvalidSetting(_))
        ));
        assert!(matches!(
            NewJob::of(&Big(String::new()), &options.clone().unique_key(""), None),
            Err(JobsError::InvalidSetting(_))
        ));
        let ok = NewJob::of(&Big("a".into()), &EnqueueOptions::new().priority(-3), None)
            .expect("valid job");
        assert_eq!(
            (ok.priority, ok.max_attempts, ok.queue),
            (-3, 10, "default")
        );
    }

    #[test]
    fn time_conversions_round_trip() {
        let time = timestamp(1_792_434_165_935_102).expect("timestamp");
        assert_eq!(time.unix_micros(), 1_792_434_165_935_102);
        assert!(matches!(timestamp(i64::MAX), Err(JobsError::Corrupt)));
        assert_eq!(millis(Duration::MAX), i64::MAX);
        assert_eq!(migrator().table_name, MIGRATIONS_TABLE);
        assert!(
            migrator()
                .iter()
                .any(|m| m.migration_type.is_down_migration())
        );
    }
}
