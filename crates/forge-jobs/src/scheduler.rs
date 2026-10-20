//! Recurring cron schedules that enqueue a job once per tick, across any
//! number of replicas.

use std::time::Duration;

use forge_core::{Cancellation, Timestamp};
use tracing::{debug, info, warn};

use crate::{
    Cron, EnqueueOptions, Job, JobsError,
    backoff::jitter,
    name,
    postgres::{JobSpec, NewJob, PgJobStore, SCHEDULE_KEY_PREFIX, TickOutcome, serialize},
};

/// Longest wait between scheduler passes, so a clock jump is noticed.
const MAX_SLEEP: Duration = Duration::from_secs(60);

/// A recurring job: enqueue `job` at every tick of `cron` (UTC).
///
/// Each tick is enqueued **exactly once** no matter how many replicas run the
/// scheduler: replicas race on a conditional upsert of the schedule's
/// `last_tick_at` row, only the replica that advances it enqueues the job,
/// and it does so in the same transaction. Ticks missed while no scheduler
/// ran are not replayed; only the latest due tick is considered, and it is
/// skipped when it is older than [`Schedule::max_lateness`] (default five
/// minutes), so a deployment never fires a burst of stale ticks.
///
/// ```
/// use forge_jobs::{Job, Schedule};
/// use serde::{Deserialize, Serialize};
///
/// #[derive(Serialize, Deserialize)]
/// struct PurgeExpiredSessions;
///
/// impl Job for PurgeExpiredSessions {
///     const NAME: &'static str = "accounts.purge_expired_sessions";
/// }
///
/// let schedule = Schedule::new("nightly.purge-sessions", "0 3 * * *".parse()?, &PurgeExpiredSessions)?;
/// assert_eq!(schedule.name(), "nightly.purge-sessions");
/// # Ok::<(), forge_jobs::JobsError>(())
/// ```
#[derive(Clone, Debug)]
pub struct Schedule {
    name: String,
    cron: Cron,
    spec: JobSpec,
    payload: String,
    lateness: Duration,
}

impl Schedule {
    /// A schedule named `name` (same grammar as job names) that enqueues a
    /// copy of `job` at every tick of `cron`.
    pub fn new<J: Job>(name: impl Into<String>, cron: Cron, job: &J) -> Result<Self, JobsError> {
        let name = name.into();
        name::schedule(&name)?;
        let spec = JobSpec::of::<J>();
        spec.validate()?;
        Ok(Self {
            name,
            cron,
            spec,
            payload: serialize(job)?,
            lateness: Duration::from_secs(300),
        })
    }

    /// How late a tick may be enqueued (database clock); older ticks are
    /// skipped. At least one minute.
    #[must_use]
    pub fn max_lateness(mut self, lateness: Duration) -> Self {
        self.lateness = lateness.max(Duration::from_secs(60));
        self
    }

    /// The schedule's name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The cron expression.
    #[must_use]
    pub const fn cron(&self) -> &Cron {
        &self.cron
    }

    pub(crate) const fn job_name(&self) -> &'static str {
        self.spec.name
    }

    pub(crate) fn job_for(&self, tick: Timestamp) -> Result<NewJob, JobsError> {
        let seconds = tick.unix_micros().div_euclid(1_000_000);
        let options = EnqueueOptions::new()
            .unique_key(format!("{SCHEDULE_KEY_PREFIX}{}:{seconds}", self.name));
        NewJob::from_parts(self.spec, self.payload.clone(), &options, None)
    }
}

/// Fires due ticks until `shutdown`; one database round trip per due tick.
pub(crate) async fn run(store: PgJobStore, schedules: Vec<Schedule>, shutdown: Cancellation) {
    let mut handled: Vec<Option<Timestamp>> = vec![None; schedules.len()];
    info!(schedules = schedules.len(), "scheduler started");
    while !shutdown.is_cancelled() {
        // The database clock decides which tick is due, so replicas with
        // skewed clocks agree (the database also refuses future ticks).
        let now = match store.database_now().await {
            Ok(now) => now,
            Err(error) => {
                warn!(%error, "could not read the database clock; retrying");
                tokio::select! {
                    () = shutdown.cancelled() => break,
                    () = tokio::time::sleep(jitter(Duration::from_secs(5))) => continue,
                }
            }
        };
        for (schedule, last) in schedules.iter().zip(handled.iter_mut()) {
            let Some(due) = schedule.cron.latest_at_or_before(now) else {
                continue;
            };
            if last.is_some_and(|last| last >= due) {
                continue;
            }
            if fire(&store, schedule, due).await {
                *last = Some(due);
            }
        }
        let next = schedules
            .iter()
            .filter_map(|schedule| schedule.cron.next_after(now))
            .min();
        let wait = next
            .map(|next| {
                let micros = next.unix_micros() - now.unix_micros();
                Duration::from_micros(u64::try_from(micros).unwrap_or(0))
            })
            .unwrap_or(Duration::from_secs(1))
            .min(MAX_SLEEP);
        // Spread replicas over the first second after a tick.
        let wait = wait + jitter(Duration::from_secs(1));
        tokio::select! {
            () = shutdown.cancelled() => break,
            () = tokio::time::sleep(wait) => {}
        }
    }
    info!("scheduler stopped");
}

/// Returns whether the tick is settled (fired, skipped or handled elsewhere).
async fn fire(store: &PgJobStore, schedule: &Schedule, due: Timestamp) -> bool {
    let job = match schedule.job_for(due) {
        Ok(job) => job,
        Err(error) => {
            warn!(schedule = schedule.name, %error, "invalid scheduled job");
            return true;
        }
    };
    match store
        .fire_tick(&schedule.name, due, schedule.lateness, job)
        .await
    {
        Ok(TickOutcome::Fired(job_id)) => {
            info!(
                schedule = schedule.name,
                job = schedule.spec.name,
                %job_id,
                "scheduled job enqueued"
            );
            true
        }
        Ok(TickOutcome::Skipped) => {
            info!(
                schedule = schedule.name,
                "skipped a tick older than the allowed lateness"
            );
            true
        }
        Ok(TickOutcome::AlreadyHandled) => {
            debug!(
                schedule = schedule.name,
                "tick already handled by another scheduler"
            );
            true
        }
        Ok(TickOutcome::Early) => {
            warn!(
                schedule = schedule.name,
                "tick is later than the database clock; will retry"
            );
            false
        }
        Err(error) => {
            warn!(schedule = schedule.name, %error, "could not fire schedule; will retry");
            false
        }
    }
}
