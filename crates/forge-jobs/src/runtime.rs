//! The job runtime: handler registry, queue configuration and the supervised
//! set of worker, scheduler and pruning loops.

use std::{
    collections::{BTreeMap, HashMap},
    fmt,
    sync::Arc,
    time::Duration,
};

use forge_core::Cancellation;
use tokio::task::JoinSet;
use tracing::{error, info, warn};

use crate::{
    JobHandler, JobsError, Schedule, name,
    postgres::{PgJobStore, Pruning},
    scheduler,
    worker::{Chain, ErasedHandler, QueueWorker, Typed},
};

/// Timing of the worker loops.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct WorkerSettings {
    /// Wait between claims while a queue is idle, jittered to `[½, 1]×`
    /// (default 1 s).
    pub poll_interval: Duration,
    /// Lease granted per claim and per renewal (default 30 s). Leases are
    /// renewed every third of it while the attempt runs; a worker that stops
    /// renewing loses its jobs to other workers after at most this long.
    pub lease: Duration,
    /// How long shutdown waits for in-flight attempts before cancelling them
    /// (default 20 s).
    pub drain_timeout: Duration,
    /// How long cancelled attempts get to stop before they are aborted and
    /// their leases released (default 5 s).
    pub cancel_grace: Duration,
    /// Concurrent attempts per queue unless configured with
    /// [`Jobs::concurrency`] (default 5).
    pub default_concurrency: u16,
}

impl Default for WorkerSettings {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(1),
            lease: Duration::from_secs(30),
            drain_timeout: Duration::from_secs(20),
            cancel_grace: Duration::from_secs(5),
            default_concurrency: 5,
        }
    }
}

impl WorkerSettings {
    /// Sets the idle poll interval.
    #[must_use]
    pub const fn poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }

    /// Sets the lease duration.
    #[must_use]
    pub const fn lease(mut self, lease: Duration) -> Self {
        self.lease = lease;
        self
    }

    /// Sets the drain timeout.
    #[must_use]
    pub const fn drain_timeout(mut self, timeout: Duration) -> Self {
        self.drain_timeout = timeout;
        self
    }

    /// Sets the cancel grace.
    #[must_use]
    pub const fn cancel_grace(mut self, grace: Duration) -> Self {
        self.cancel_grace = grace;
        self
    }

    /// Sets the default per-queue concurrency.
    #[must_use]
    pub const fn default_concurrency(mut self, workers: u16) -> Self {
        self.default_concurrency = workers;
        self
    }

    pub(crate) fn heartbeat_interval(&self) -> Duration {
        self.lease / 3
    }

    fn validate(&self) -> Result<(), JobsError> {
        if self.poll_interval < Duration::from_millis(10)
            || self.poll_interval > Duration::from_secs(300)
        {
            return Err(JobsError::InvalidSetting(
                "poll_interval must be between 10 ms and 5 minutes",
            ));
        }
        if self.lease < Duration::from_secs(3) || self.lease > Duration::from_secs(86_400) {
            return Err(JobsError::InvalidSetting(
                "lease must be between 3 seconds and 1 day",
            ));
        }
        validate_concurrency(self.default_concurrency)
    }
}

fn validate_concurrency(workers: u16) -> Result<(), JobsError> {
    if (1..=MAX_CONCURRENCY).contains(&workers) {
        Ok(())
    } else {
        Err(JobsError::InvalidSetting(
            "queue concurrency must be between 1 and 256",
        ))
    }
}

/// Upper bound of per-queue concurrency.
pub const MAX_CONCURRENCY: u16 = 256;

/// Registered handlers, queues and schedules, and the entry point that runs
/// them.
///
/// ```no_run
/// use forge_core::Cancellation;
/// use forge_jobs::{Job, JobContext, JobError, JobHandler, Jobs, postgres::PgJobStore};
/// use serde::{Deserialize, Serialize};
///
/// #[derive(Serialize, Deserialize)]
/// struct Ping;
///
/// impl Job for Ping {
///     const NAME: &'static str = "demo.ping";
/// }
///
/// struct PingHandler;
///
/// #[async_trait::async_trait]
/// impl JobHandler for PingHandler {
///     type Job = Ping;
///
///     async fn perform(&self, _: &JobContext, _: Ping) -> Result<(), JobError> {
///         Ok(())
///     }
/// }
///
/// # async fn run(store: PgJobStore, shutdown: Cancellation) -> Result<(), forge_jobs::JobsError> {
/// let mut jobs = Jobs::new();
/// jobs.register(PingHandler)?.concurrency("default", 10)?;
/// jobs.run(store, shutdown).await
/// # }
/// ```
pub struct Jobs {
    handlers: HashMap<&'static str, Arc<dyn ErasedHandler>>,
    concurrency: BTreeMap<String, u16>,
    schedules: Vec<Schedule>,
    settings: WorkerSettings,
    pruning: Option<Pruning>,
}

impl fmt::Debug for Jobs {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut names: Vec<_> = self.handlers.keys().collect();
        names.sort();
        formatter
            .debug_struct("Jobs")
            .field("handlers", &names)
            .field("concurrency", &self.concurrency)
            .field(
                "schedules",
                &self
                    .schedules
                    .iter()
                    .map(Schedule::name)
                    .collect::<Vec<_>>(),
            )
            .field("settings", &self.settings)
            .field("pruning", &self.pruning)
            .finish()
    }
}

impl Default for Jobs {
    fn default() -> Self {
        Self::new()
    }
}

impl Jobs {
    /// No handlers, default settings and default pruning.
    #[must_use]
    pub fn new() -> Self {
        Self {
            handlers: HashMap::new(),
            concurrency: BTreeMap::new(),
            schedules: Vec::new(),
            settings: WorkerSettings::default(),
            pruning: Some(Pruning::default()),
        }
    }

    /// Registers the handler of one job type. Fails on an invalid name or a
    /// second handler for the same job name.
    pub fn register<H: JobHandler>(&mut self, handler: H) -> Result<&mut Self, JobsError> {
        let typed = Typed(handler);
        let spec = crate::postgres::JobSpec::of::<H::Job>();
        spec.validate()?;
        if typed.timeout().is_zero() {
            return Err(JobsError::InvalidSetting("TIMEOUT must be positive"));
        }
        if self.handlers.contains_key(spec.name) {
            return Err(JobsError::DuplicateHandler(spec.name));
        }
        self.handlers.insert(spec.name, Arc::new(typed));
        Ok(self)
    }

    /// Sets how many attempts of `queue` run at once (1 to 256).
    pub fn concurrency(&mut self, queue: &str, workers: u16) -> Result<&mut Self, JobsError> {
        name::queue(queue)?;
        validate_concurrency(workers)?;
        self.concurrency.insert(queue.to_owned(), workers);
        Ok(self)
    }

    /// Adds a recurring schedule. Its job must have a registered handler
    /// by the time the runtime starts.
    pub fn schedule(&mut self, schedule: Schedule) -> Result<&mut Self, JobsError> {
        if self
            .schedules
            .iter()
            .any(|existing| existing.name() == schedule.name())
        {
            return Err(JobsError::DuplicateSchedule(schedule.name().to_owned()));
        }
        self.schedules.push(schedule);
        Ok(self)
    }

    /// Replaces the worker timing.
    pub fn settings(&mut self, settings: WorkerSettings) -> &mut Self {
        self.settings = settings;
        self
    }

    /// Replaces the retention policy; `None` disables background pruning.
    pub fn pruning(&mut self, pruning: Option<Pruning>) -> &mut Self {
        self.pruning = pruning;
        self
    }

    /// Whether no handler is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.handlers.is_empty()
    }

    /// Registered job names, sorted.
    #[must_use]
    pub fn job_names(&self) -> Vec<&'static str> {
        let mut names: Vec<_> = self.handlers.keys().copied().collect();
        names.sort_unstable();
        names
    }

    /// Checks the configuration without running anything.
    pub fn validate(&self) -> Result<(), JobsError> {
        self.settings.validate()?;
        for schedule in &self.schedules {
            if !self.handlers.contains_key(schedule.job_name()) {
                return Err(JobsError::UnhandledSchedule {
                    schedule: schedule.name().to_owned(),
                    job: schedule.job_name(),
                });
            }
        }
        Ok(())
    }

    /// Runs one worker loop per queue that has handlers, the scheduler (when
    /// schedules exist) and the pruning loop until `shutdown` is cancelled,
    /// then drains them (see [`WorkerSettings`]) and returns.
    ///
    /// Every loop is a task owned by this future; none outlives it. If a loop
    /// panics, the others are stopped gracefully and the panic is reported as
    /// [`JobsError::TaskFailed`].
    pub async fn run(self, store: PgJobStore, shutdown: Cancellation) -> Result<(), JobsError> {
        self.validate()?;
        let Self {
            handlers,
            concurrency,
            schedules,
            settings,
            pruning,
        } = self;

        let mut queues: BTreeMap<&'static str, HashMap<&'static str, Arc<dyn ErasedHandler>>> =
            BTreeMap::new();
        for (name, handler) in handlers {
            queues
                .entry(handler.queue())
                .or_default()
                .insert(name, handler);
        }

        let stop = shutdown.child();
        let mut tasks: JoinSet<()> = JoinSet::new();
        let mut names: HashMap<tokio::task::Id, &'static str> = HashMap::new();
        for (queue, handlers) in queues {
            let workers = concurrency
                .get(queue)
                .copied()
                .unwrap_or(settings.default_concurrency);
            let worker = QueueWorker {
                queue,
                concurrency: usize::from(workers),
                handlers,
                store: store.clone(),
                settings,
            };
            let handle = tasks.spawn(worker.run(stop.clone()));
            names.insert(handle.id(), "queue worker");
        }
        if !schedules.is_empty() {
            let handle = tasks.spawn(scheduler::run(store.clone(), schedules, stop.clone()));
            names.insert(handle.id(), "scheduler");
        }
        if let Some(pruning) = pruning {
            let handle = tasks.spawn(prune(store.clone(), pruning, stop.clone()));
            names.insert(handle.id(), "pruner");
        }

        let mut failure = None;
        while let Some(joined) = tasks.join_next().await {
            if let Err(join_error) = joined {
                let task = names
                    .get(&join_error.id())
                    .copied()
                    .unwrap_or("job runtime task");
                error!(task, "job runtime task failed; stopping the job runtime");
                failure.get_or_insert(JobsError::TaskFailed(task));
                stop.cancel();
            }
        }
        failure.map_or(Ok(()), Err)
    }
}

/// Deletes finished jobs past their retention every `interval`.
async fn prune(store: PgJobStore, pruning: Pruning, shutdown: Cancellation) {
    let interval = pruning.interval.max(Duration::from_secs(1));
    loop {
        tokio::select! {
            () = shutdown.cancelled() => return,
            () = tokio::time::sleep(crate::backoff::jitter(interval)) => {}
        }
        match store.prune(&pruning).await {
            Ok(0) => {}
            Ok(deleted) => info!(deleted, "pruned finished jobs"),
            Err(error) => warn!(error = %Chain(&error), "pruning finished jobs failed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use serde::{Deserialize, Serialize};

    use super::*;
    use crate::{Job, JobContext, JobError};

    #[derive(Serialize, Deserialize)]
    struct Ping;

    impl Job for Ping {
        const NAME: &'static str = "test.ping";
    }

    struct PingHandler;

    #[async_trait]
    impl JobHandler for PingHandler {
        type Job = Ping;

        async fn perform(&self, _: &JobContext, _: Ping) -> Result<(), JobError> {
            Ok(())
        }
    }

    #[derive(Serialize, Deserialize)]
    struct Orphan;

    impl Job for Orphan {
        const NAME: &'static str = "test.orphan";
    }

    #[test]
    fn registration_rejects_duplicates_and_bad_settings() {
        let mut jobs = Jobs::new();
        assert!(jobs.is_empty());
        jobs.register(PingHandler).expect("first registration");
        assert!(matches!(
            jobs.register(PingHandler),
            Err(JobsError::DuplicateHandler("test.ping"))
        ));
        assert_eq!(jobs.job_names(), ["test.ping"]);
        assert!(jobs.concurrency("default", 0).is_err());
        assert!(jobs.concurrency("Bad Queue", 1).is_err());
        assert!(jobs.concurrency("default", MAX_CONCURRENCY + 1).is_err());
        jobs.concurrency("default", 3).expect("valid");

        let every_minute = "* * * * *".parse().expect("cron");
        let schedule = Schedule::new("minutely", every_minute, &Ping).expect("schedule");
        jobs.schedule(schedule.clone()).expect("first schedule");
        assert!(matches!(
            jobs.schedule(schedule),
            Err(JobsError::DuplicateSchedule(_))
        ));
        jobs.validate().expect("valid configuration");

        let orphan =
            Schedule::new("orphan", "@hourly".parse().expect("cron"), &Orphan).expect("schedule");
        jobs.schedule(orphan).expect("added");
        assert!(matches!(
            jobs.validate(),
            Err(JobsError::UnhandledSchedule { .. })
        ));

        let mut fast = Jobs::new();
        fast.settings(WorkerSettings::default().lease(Duration::from_secs(1)));
        assert!(matches!(fast.validate(), Err(JobsError::InvalidSetting(_))));
        fast.settings(WorkerSettings::default().poll_interval(Duration::ZERO));
        assert!(fast.validate().is_err());
        fast.settings(WorkerSettings::default().default_concurrency(0));
        assert!(fast.validate().is_err());
        assert!(format!("{jobs:?}").contains("test.ping"));
    }
}
