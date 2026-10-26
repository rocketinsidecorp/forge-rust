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
/// Shortest [`Schedule::max_lateness`]: the database clock decides, and a
/// replica may notice a tick up to a pass late.
const MIN_LATENESS: Duration = Duration::from_secs(60);
/// Most skipped ticks counted one by one (65536 minutes is about 45 days),
/// Forge-Go's `maxCatchUpWalk`.
const MAX_CATCH_UP_WALK: u32 = 1 << 16;

/// The time from `from` to `to`, zero when `to` is not later.
fn micros_between(from: Timestamp, to: Timestamp) -> Duration {
    Duration::from_micros(u64::try_from(to.unix_micros() - from.unix_micros()).unwrap_or(0))
}

/// A recurring job: enqueue `job` at every tick of `cron` (UTC).
///
/// Each tick is enqueued **exactly once** no matter how many replicas run the
/// scheduler: replicas race on a conditional upsert of the schedule's
/// `last_tick_at` row, only the replica that advances it enqueues the job,
/// and it does so in the same transaction. Missed ticks are not replayed
/// back to back: only the latest due tick is considered, once, and only
/// while it is at most [`Schedule::max_lateness`] late (default: one
/// interval of the schedule, the gap from the tick to the following one, so
/// after a stall the latest missed tick still runs once). The earlier missed
/// ticks a scheduler notices are skipped and reported in a warning, so a
/// deployment never fires a burst of stale ticks (Forge-Go's
/// `SchedulerOptions.MaxLateness`, the same default).
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
    /// `None`: one interval of the schedule.
    lateness: Option<Duration>,
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
            lateness: None,
        })
    }

    /// How late a tick may be enqueued (database clock); older ticks are
    /// skipped. At least one minute. Without it, one interval of the
    /// schedule: the gap from the tick to the following one.
    #[must_use]
    pub fn max_lateness(mut self, lateness: Duration) -> Self {
        self.lateness = Some(lateness.max(MIN_LATENESS));
        self
    }

    /// How late `tick` may be enqueued: [`Schedule::max_lateness`], or the
    /// gap to the following tick (at least one minute).
    #[must_use]
    pub fn lateness_of(&self, tick: Timestamp) -> Duration {
        self.lateness.unwrap_or_else(|| {
            self.cron
                .next_after(tick)
                .map(|next| micros_between(tick, next))
                .unwrap_or(MIN_LATENESS)
                .max(MIN_LATENESS)
        })
    }

    /// The due ticks strictly between `last` (handled) and `due` (the
    /// latest due), which are skipped. Counts at most [`MAX_CATCH_UP_WALK`].
    pub(crate) fn skipped_between(&self, last: Timestamp, due: Timestamp) -> u32 {
        let mut skipped = 0;
        let mut tick = last;
        while skipped < MAX_CATCH_UP_WALK {
            match self.cron.next_after(tick) {
                Some(next) if next < due => {
                    skipped += 1;
                    tick = next;
                }
                _ => break,
            }
        }
        skipped
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
            if let Some(last) = *last {
                let skipped = schedule.skipped_between(last, due);
                if skipped > 0 {
                    // Behind schedule (a stall, an unreachable database):
                    // only the latest due tick is a candidate.
                    warn!(
                        schedule = schedule.name,
                        skipped,
                        "skipped missed ticks instead of replaying them; only the latest due tick runs"
                    );
                }
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
        .fire_tick(&schedule.name, due, schedule.lateness_of(due), job)
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
            warn!(
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

#[cfg(test)]
mod tests {
    use forge_core::Timestamp;
    use serde::{Deserialize, Serialize};

    use super::{MAX_CATCH_UP_WALK, Schedule, fire};
    use crate::{
        Cron, Job,
        postgres::{PgJobStore, migrator},
        testing::RlsTestDatabase,
    };

    #[derive(Serialize, Deserialize)]
    struct Tick;

    impl Job for Tick {
        const NAME: &'static str = "scheduler.tick";
    }

    fn at(text: &str) -> Timestamp {
        Timestamp::parse(text).expect("timestamp")
    }

    #[test]
    fn the_ticks_between_the_last_handled_and_the_latest_due_are_skipped() {
        let every_minute = Schedule::new(
            "every-minute",
            Cron::parse("* * * * *").expect("cron"),
            &Tick,
        )
        .expect("schedule");
        let last = at("2026-10-26T10:00:00Z");
        // The next tick: nothing in between.
        assert_eq!(
            every_minute.skipped_between(last, at("2026-10-26T10:01:00Z")),
            0
        );
        // A ten-minute stall: nine ticks are skipped, the tenth runs.
        assert_eq!(
            every_minute.skipped_between(last, at("2026-10-26T10:10:00Z")),
            9
        );
        // A stall longer than the walk counts no further.
        assert_eq!(
            every_minute.skipped_between(last, at("2026-12-26T10:00:00Z")),
            MAX_CATCH_UP_WALK
        );
        let hourly = Schedule::new("hourly", Cron::parse("0 * * * *").expect("cron"), &Tick)
            .expect("schedule");
        assert_eq!(hourly.skipped_between(last, at("2026-10-26T13:00:00Z")), 2);
    }

    async fn queued_ticks(db: &RlsTestDatabase) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM forge_jobs WHERE job_name = $1")
            .bind(Tick::NAME)
            .fetch_one(db.admin().pool())
            .await
            .expect("count")
    }

    #[tokio::test]
    #[ignore = "needs FORGE_TEST_DATABASE_URL"]
    async fn a_tick_is_settled_unless_the_database_says_to_try_again() {
        let db = RlsTestDatabase::create(&[migrator()])
            .await
            .expect("needs FORGE_TEST_DATABASE_URL");
        let store = PgJobStore::new(db.runtime().clone());
        let every_minute = Cron::parse("* * * * *").expect("cron");
        let schedule = Schedule::new("settled", every_minute.clone(), &Tick).expect("schedule");
        let now = store.database_now().await.expect("database clock");
        let current = every_minute.latest_at_or_before(now).expect("tick");
        let micros = |tick: Timestamp, offset: i64| {
            Timestamp::from_unix_micros(tick.unix_micros() + offset).expect("timestamp")
        };

        // Fired: one job, and the tick is settled.
        assert!(fire(&store, &schedule, current).await);
        assert_eq!(queued_ticks(&db).await, 1);
        // Handled by another scheduler: settled, and no second job.
        assert!(fire(&store, &schedule, current).await);
        assert_eq!(queued_ticks(&db).await, 1);
        // Older than the allowed lateness: settled without a job.
        assert!(fire(&store, &schedule, micros(current, -7_200_000_000)).await);
        assert_eq!(queued_ticks(&db).await, 1);
        // Later than the database clock (a fast replica): not settled, no job.
        assert!(!fire(&store, &schedule, micros(current, 3_600_000_000)).await);
        assert_eq!(queued_ticks(&db).await, 1);

        // An unreachable database is not settled either, and the clock fails.
        db.runtime().close().await;
        assert!(!fire(&store, &schedule, micros(current, 60_000_000)).await);
        assert!(store.database_now().await.is_err());
        db.cleanup().await.expect("cleanup");
    }
}
