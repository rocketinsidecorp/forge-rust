//! One queue's worker loop: bounded claiming, lease renewal, execution with
//! timeouts and panic isolation, and graceful drain.

use std::{
    collections::HashMap,
    fmt,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use async_trait::async_trait;
use forge_core::Cancellation;
use tokio::{
    task::{self, AbortHandle, JoinError, JoinSet},
    time::MissedTickBehavior,
};
use tracing::{debug, error, info, warn};

use crate::{
    Backoff, JobContext, JobError, JobHandler, JobId, WorkerSettings,
    backoff::jitter,
    job::{Disposition, LeaseToken},
    postgres::{AfterFailure, ClaimedJob, LeaseStatus, PgJobStore},
};

/// A handler with its job type erased: decodes the payload, then performs.
#[async_trait]
pub(crate) trait ErasedHandler: Send + Sync {
    fn name(&self) -> &'static str;
    fn queue(&self) -> &'static str;
    fn timeout(&self) -> Duration;
    fn backoff(&self) -> Backoff;
    async fn perform(&self, context: &JobContext, payload: &str) -> Result<(), JobError>;
}

pub(crate) struct Typed<H>(pub(crate) H);

#[async_trait]
impl<H: JobHandler> ErasedHandler for Typed<H> {
    fn name(&self) -> &'static str {
        <H::Job as crate::Job>::NAME
    }

    fn queue(&self) -> &'static str {
        <H::Job as crate::Job>::QUEUE
    }

    fn timeout(&self) -> Duration {
        <H::Job as crate::Job>::TIMEOUT
    }

    fn backoff(&self) -> Backoff {
        <H::Job as crate::Job>::BACKOFF
    }

    async fn perform(&self, context: &JobContext, payload: &str) -> Result<(), JobError> {
        // The decoder's message could quote payload content; it is not kept.
        let job = serde_json::from_str(payload)
            .map_err(|_| JobError::permanent("the job payload could not be decoded"))?;
        self.0.perform(context, job).await
    }
}

/// Bookkeeping for a running attempt, keyed by its task id.
struct InFlight {
    id: JobId,
    name: &'static str,
    token: LeaseToken,
    attempt: u32,
    backoff: Backoff,
    cancel: Cancellation,
    lease_lost: bool,
    /// Set when the attempt completed itself with `complete_in`.
    completed: Arc<AtomicBool>,
    /// When the lease was last known to be held: the claim, then every
    /// successful renewal.
    last_renewed: Instant,
    /// When the attempt was cancelled because its lease was (or could be)
    /// lost; it is aborted if it is still running after the cancel grace.
    cancelled_at: Option<Instant>,
    abort: AbortHandle,
}

pub(crate) struct QueueWorker {
    pub(crate) queue: &'static str,
    pub(crate) concurrency: usize,
    pub(crate) handlers: HashMap<&'static str, Arc<dyn ErasedHandler>>,
    pub(crate) store: PgJobStore,
    pub(crate) settings: WorkerSettings,
}

impl QueueWorker {
    /// Runs until `shutdown`, then drains: waits for in-flight attempts up to
    /// the drain timeout, cancels the rest cooperatively, aborts what is left
    /// after the cancel grace and releases their leases.
    pub(crate) async fn run(self, shutdown: Cancellation) {
        let names: Vec<String> = self
            .handlers
            .keys()
            .map(|name| (*name).to_owned())
            .collect();
        let interrupt = Cancellation::new();
        let mut tasks: JoinSet<()> = JoinSet::new();
        let mut in_flight: HashMap<task::Id, InFlight> = HashMap::new();
        let mut heartbeat = tokio::time::interval(self.settings.heartbeat_interval());
        heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
        info!(
            queue = self.queue,
            concurrency = self.concurrency,
            "worker started"
        );

        while !shutdown.is_cancelled() {
            let free = self.concurrency.saturating_sub(tasks.len());
            if free > 0 {
                let token = LeaseToken::new();
                // The database grants the lease after this instant.
                let claimed_at = Instant::now();
                match self
                    .store
                    .claim(self.queue, &names, free, self.settings.lease, token)
                    .await
                {
                    Ok(claimed) => {
                        for job in claimed {
                            self.start(
                                job,
                                token,
                                claimed_at,
                                &interrupt,
                                &mut tasks,
                                &mut in_flight,
                            );
                        }
                    }
                    Err(failure) => {
                        warn!(queue = self.queue, error = %Chain(&failure), "claiming jobs failed");
                    }
                }
            }
            // Below capacity means the queue had no more due jobs (or the
            // claim failed): poll again later. At capacity, wait for a slot.
            let below_capacity = tasks.len() < self.concurrency;
            tokio::select! {
                () = shutdown.cancelled() => break,
                Some(joined) = tasks.join_next_with_id(), if !tasks.is_empty() => {
                    self.finished(joined, &mut in_flight).await;
                }
                _ = heartbeat.tick() => self.renew(&mut in_flight).await,
                () = sleep_until_opt(self.next_check(&in_flight)) => self.expire(&mut in_flight),
                () = tokio::time::sleep(jitter(self.settings.poll_interval)), if below_capacity => {}
            }
        }

        if !tasks.is_empty() {
            info!(
                queue = self.queue,
                in_flight = tasks.len(),
                "worker stopping; draining in-flight jobs"
            );
            let deadline = Instant::now() + self.settings.drain_timeout;
            self.drain_until(deadline, &mut tasks, &mut in_flight, &mut heartbeat)
                .await;
        }
        if !tasks.is_empty() {
            warn!(
                queue = self.queue,
                in_flight = tasks.len(),
                "drain timeout elapsed; cancelling in-flight jobs"
            );
            interrupt.cancel();
            let deadline = Instant::now() + self.settings.cancel_grace;
            self.drain_until(deadline, &mut tasks, &mut in_flight, &mut heartbeat)
                .await;
            tasks.abort_all();
            while let Some(joined) = tasks.join_next_with_id().await {
                self.finished(joined, &mut in_flight).await;
            }
        }
        info!(queue = self.queue, "worker stopped");
    }

    fn start(
        &self,
        job: ClaimedJob,
        token: LeaseToken,
        claimed_at: Instant,
        interrupt: &Cancellation,
        tasks: &mut JoinSet<()>,
        in_flight: &mut HashMap<task::Id, InFlight>,
    ) {
        let cancel = interrupt.child();
        let Some(handler) = self.handlers.get(job.name.as_str()).cloned() else {
            // Unreachable: claims are filtered by the registered names.
            error!(queue = self.queue, job_id = %job.id, "claimed a job without a handler");
            return;
        };
        let (tenant, scope_valid) = match job.tenant() {
            Ok(tenant) => (tenant, true),
            Err(_) => (None, false),
        };
        let context = JobContext::new(
            job.id,
            handler.name(),
            self.queue,
            job.attempt,
            job.max_attempts,
            tenant,
            token,
            cancel.clone(),
        );
        let completed = Arc::clone(&context.completed_in_transaction);
        let execution = Execution {
            store: self.store.clone(),
            handler: Arc::clone(&handler),
            context,
            payload: job.payload,
            scope_valid,
            interrupt: interrupt.clone(),
        };
        let abort = tasks.spawn(execution.run());
        in_flight.insert(
            abort.id(),
            InFlight {
                id: job.id,
                name: handler.name(),
                token,
                attempt: job.attempt,
                backoff: handler.backoff(),
                cancel,
                lease_lost: false,
                completed,
                last_renewed: claimed_at,
                cancelled_at: None,
                abort,
            },
        );
    }

    /// Records the end of a task: a panic is a failed attempt, an abort
    /// (shutdown) releases the lease without consuming the attempt.
    async fn finished(
        &self,
        joined: Result<(task::Id, ()), JoinError>,
        in_flight: &mut HashMap<task::Id, InFlight>,
    ) {
        let failure = match joined {
            Ok((id, ())) => {
                in_flight.remove(&id);
                return;
            }
            Err(failure) => failure,
        };
        let Some(job) = in_flight.remove(&failure.id()) else {
            return;
        };
        if failure.is_panic() {
            // The panic payload may quote job data; it is not recorded.
            let delay = job.backoff.delay(job.attempt);
            match self
                .store
                .fail(job.id, job.token, "the job panicked", delay, false)
                .await
            {
                Ok(state) => error!(
                    queue = self.queue,
                    job = job.name,
                    job_id = %job.id,
                    attempt = job.attempt,
                    outcome = outcome_name(state),
                    "job panicked"
                ),
                Err(error) => error!(
                    queue = self.queue,
                    job = job.name,
                    job_id = %job.id,
                    error = %Chain(&error),
                    "job panicked and the failure could not be recorded; its lease will expire"
                ),
            }
        } else if !job.lease_lost {
            match self.store.release(job.id, job.token).await {
                Ok(released) => info!(
                    queue = self.queue,
                    job = job.name,
                    job_id = %job.id,
                    released,
                    "aborted job at shutdown; lease released"
                ),
                Err(error) => warn!(
                    queue = self.queue,
                    job_id = %job.id,
                    error = %Chain(&error),
                    "could not release an aborted job; its lease will expire"
                ),
            }
        }
    }

    /// Extends the leases of running jobs; cancels jobs whose lease is gone.
    async fn renew(&self, in_flight: &mut HashMap<task::Id, InFlight>) {
        let leases: Vec<(JobId, LeaseToken)> = in_flight
            .values()
            .filter(|job| !job.lease_lost)
            .map(|job| (job.id, job.token))
            .collect();
        if leases.is_empty() {
            self.expire(in_flight);
            return;
        }
        // Bounded so an unreachable database never stalls the loop; the
        // renewal itself skips rows locked by other transactions.
        let renewal = tokio::time::timeout(
            self.settings.heartbeat_interval() / 2,
            self.store.heartbeat(&leases, self.settings.lease),
        )
        .await;
        match renewal {
            Ok(Ok(statuses)) => {
                let now = Instant::now();
                for job in in_flight.values_mut().filter(|job| !job.lease_lost) {
                    let status = statuses
                        .iter()
                        .find(|(id, _)| *id == job.id)
                        .map(|(_, status)| *status);
                    // Busy rows are locked by the attempt's own completing
                    // transaction, which keeps other workers from reclaiming them.
                    if matches!(status, Some(LeaseStatus::Renewed | LeaseStatus::Busy)) {
                        job.last_renewed = now;
                    }
                    // Completed by its own transaction: the lease ended
                    // normally.
                    if status == Some(LeaseStatus::Lost) && !job.completed.load(Ordering::Acquire) {
                        job.lease_lost = true;
                        job.cancel.cancel();
                        job.cancelled_at = Some(now);
                        warn!(
                            queue = self.queue,
                            job = job.name,
                            job_id = %job.id,
                            "lease lost; cancelling the attempt"
                        );
                    }
                }
            }
            Ok(Err(error)) => {
                warn!(queue = self.queue, error = %Chain(&error), "lease renewal failed");
            }
            Err(_) => warn!(queue = self.queue, "lease renewal timed out"),
        }
        self.expire(in_flight);
    }

    /// Cancels attempts whose lease may have expired because renewal has not
    /// succeeded in time (database unreachable or slow), whether or not the
    /// last renewal call returned; then aborts cancelled attempts that are
    /// still running after the cancel grace, so they stop occupying a
    /// concurrency slot until their timeout.
    fn expire(&self, in_flight: &mut HashMap<task::Id, InFlight>) {
        let now = Instant::now();
        let deadline = self.settings.renewal_deadline();
        for job in in_flight.values_mut() {
            if !job.lease_lost
                && !job.completed.load(Ordering::Acquire)
                && now.duration_since(job.last_renewed) >= deadline
            {
                job.lease_lost = true;
                job.cancel.cancel();
                job.cancelled_at = Some(now);
                warn!(
                    queue = self.queue,
                    job = job.name,
                    job_id = %job.id,
                    "lease could not be renewed in time; cancelling the attempt"
                );
            }
            if let Some(cancelled_at) = job.cancelled_at
                && !job.abort.is_finished()
                && now.duration_since(cancelled_at) >= self.settings.cancel_grace
            {
                job.abort.abort();
                job.cancelled_at = None;
                warn!(
                    queue = self.queue,
                    job = job.name,
                    job_id = %job.id,
                    "attempt ignored cancellation after losing its lease; aborted"
                );
            }
        }
    }

    /// The next renewal deadline or cancel-grace expiry among the attempts.
    fn next_check(&self, in_flight: &HashMap<task::Id, InFlight>) -> Option<Instant> {
        let deadline = self.settings.renewal_deadline();
        in_flight
            .values()
            .filter_map(|job| match job.cancelled_at {
                Some(cancelled_at) => Some(cancelled_at + self.settings.cancel_grace),
                None if !job.lease_lost && !job.completed.load(Ordering::Acquire) => {
                    Some(job.last_renewed + deadline)
                }
                None => None,
            })
            .min()
    }

    async fn drain_until(
        &self,
        deadline: Instant,
        tasks: &mut JoinSet<()>,
        in_flight: &mut HashMap<task::Id, InFlight>,
        heartbeat: &mut tokio::time::Interval,
    ) {
        let deadline = tokio::time::Instant::from_std(deadline);
        while !tasks.is_empty() {
            tokio::select! {
                Some(joined) = tasks.join_next_with_id() => self.finished(joined, in_flight).await,
                _ = heartbeat.tick() => self.renew(in_flight).await,
                () = tokio::time::sleep_until(deadline) => return,
            }
        }
    }
}

/// One attempt, run as a task of the queue's `JoinSet`.
struct Execution {
    store: PgJobStore,
    handler: Arc<dyn ErasedHandler>,
    context: JobContext,
    payload: String,
    scope_valid: bool,
    interrupt: Cancellation,
}

impl Execution {
    async fn run(self) {
        let context = &self.context;
        let started = Instant::now();
        debug!(queue = context.queue(), job = context.name(), job_id = %context.id(), attempt = context.attempt(), "job started");
        let result = if self.scope_valid {
            match tokio::time::timeout(
                self.handler.timeout(),
                self.handler.perform(context, &self.payload),
            )
            .await
            {
                Ok(result) => result,
                Err(_) => {
                    context.cancellation().cancel();
                    Err(JobError::retry("the attempt exceeded its timeout"))
                }
            }
        } else {
            Err(JobError::permanent("the stored tenant scope is invalid"))
        };
        let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        match result {
            Ok(()) => self.complete(elapsed_ms).await,
            Err(_) if self.interrupt.is_cancelled() => self.release().await,
            Err(failure) => self.fail(&failure, elapsed_ms).await,
        }
    }

    async fn complete(&self, elapsed_ms: u64) {
        let context = &self.context;
        match self.store.complete(context.id(), context.lease).await {
            Ok(true) => info!(
                queue = context.queue(),
                job = context.name(),
                job_id = %context.id(),
                attempt = context.attempt(),
                elapsed_ms,
                "job completed"
            ),
            Ok(false) if context.completed_in_transaction.load(Ordering::Acquire) => info!(
                queue = context.queue(),
                job = context.name(),
                job_id = %context.id(),
                attempt = context.attempt(),
                elapsed_ms,
                "job completed in its transaction"
            ),
            Ok(false) => warn!(
                queue = context.queue(),
                job = context.name(),
                job_id = %context.id(),
                "job finished after its lease was lost; another attempt owns it"
            ),
            Err(error) => warn!(
                queue = context.queue(),
                job = context.name(),
                job_id = %context.id(),
                error = %Chain(&error),
                "could not record completion; the job will run again when its lease expires"
            ),
        }
    }

    async fn release(&self) {
        let context = &self.context;
        match self.store.release(context.id(), context.lease).await {
            Ok(released) => info!(
                queue = context.queue(),
                job = context.name(),
                job_id = %context.id(),
                released,
                "job interrupted by shutdown; lease released"
            ),
            Err(error) => warn!(
                queue = context.queue(),
                job_id = %context.id(),
                error = %Chain(&error),
                "could not release an interrupted job; its lease will expire"
            ),
        }
    }

    async fn fail(&self, failure: &JobError, elapsed_ms: u64) {
        let context = &self.context;
        let (delay, discard) = match failure.disposition() {
            Disposition::Retry(Some(delay)) => (delay, false),
            Disposition::Retry(None) => (self.handler.backoff().delay(context.attempt()), false),
            Disposition::Discard => (Duration::ZERO, true),
        };
        match self
            .store
            .fail(
                context.id(),
                context.lease,
                failure.message(),
                delay,
                discard,
            )
            .await
        {
            Ok(Some(AfterFailure::Retrying)) => warn!(
                queue = context.queue(),
                job = context.name(),
                job_id = %context.id(),
                attempt = context.attempt(),
                elapsed_ms,
                retry_in_ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
                error = failure.message(),
                "job failed; will retry"
            ),
            Ok(Some(AfterFailure::Dead)) => error!(
                queue = context.queue(),
                job = context.name(),
                job_id = %context.id(),
                attempt = context.attempt(),
                elapsed_ms,
                error = failure.message(),
                "job failed; moved to the dead state"
            ),
            Ok(None) => warn!(
                queue = context.queue(),
                job = context.name(),
                job_id = %context.id(),
                "job failed after its lease was lost; another attempt owns it"
            ),
            Err(error) => warn!(
                queue = context.queue(),
                job_id = %context.id(),
                error = %Chain(&error),
                "could not record the failure; the job will run again when its lease expires"
            ),
        }
    }
}

fn outcome_name(state: Option<AfterFailure>) -> &'static str {
    match state {
        Some(AfterFailure::Retrying) => "retrying",
        Some(AfterFailure::Dead) => "dead",
        None => "lease_lost",
    }
}

/// Displays an error with its sources, separated by `: `.
pub(crate) struct Chain<'a>(pub(crate) &'a (dyn std::error::Error + 'static));

impl fmt::Display for Chain<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)?;
        let mut source = self.0.source();
        while let Some(error) = source {
            write!(formatter, ": {error}")?;
            source = error.source();
        }
        Ok(())
    }
}

/// Sleeps until `instant`, or forever when there is none.
async fn sleep_until_opt(instant: Option<Instant>) {
    match instant {
        Some(instant) => tokio::time::sleep_until(tokio::time::Instant::from_std(instant)).await,
        None => std::future::pending().await,
    }
}
