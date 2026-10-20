//! Durable jobs and the outbox dispatcher in the application lifecycle
//! (feature `jobs`, plus `events` for the outbox; ADR 0025).
//!
//! [`App::jobs`](crate::App::jobs) declares the subsystem. Composition hooks
//! register handlers and schedules on [`Composition::jobs`](crate::Composition::jobs)
//! and, with `events`, an outbox publisher on
//! [`Composition::events`](crate::Composition::events). The runtime then runs
//! inside `serve` ([`WorkerMode::InServe`]) or only in the dedicated
//! `jobs work` command ([`WorkerMode::Dedicated`]), sharing the process
//! shutdown: stop claiming, drain, cancel, release.

use std::fmt;

use forge_core::Cancellation;
use forge_db::PgDatabase;
use forge_jobs::{Jobs, JobsError, postgres::PgJobStore};
use thiserror::Error;
use tracing::{info, warn};

/// Where the job workers (and the outbox dispatcher) run.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub enum WorkerMode {
    /// `serve` runs the workers next to the HTTP server, and `jobs work`
    /// runs them alone: one process type is enough for small deployments.
    #[default]
    InServe,
    /// Only `jobs work` runs them; `serve` only enqueues. Scale web and
    /// worker processes independently.
    Dedicated,
}

/// What `App::jobs`/`App::events` declared.
#[derive(Clone, Copy, Debug)]
pub(crate) struct JobsDeclaration {
    pub(crate) mode: WorkerMode,
    #[cfg(feature = "events")]
    pub(crate) events: bool,
}

impl JobsDeclaration {
    pub(crate) const fn new(mode: WorkerMode) -> Self {
        Self {
            mode,
            #[cfg(feature = "events")]
            events: false,
        }
    }

    /// The embedded framework schemas `migrate` applies before the
    /// application's own migrations.
    pub(crate) fn migrators(&self) -> Vec<forge_db::Migrator> {
        #[cfg_attr(not(feature = "events"), allow(unused_mut))]
        let mut migrators = vec![forge_jobs::postgres::migrator()];
        #[cfg(feature = "events")]
        if self.events {
            migrators.push(forge_events::postgres::migrator());
        }
        migrators
    }
}

/// Outbox wiring for composition hooks (feature `events`).
#[cfg(feature = "events")]
#[derive(Default)]
pub struct EventsSetup {
    publisher: Option<
        Box<dyn FnOnce(forge_events::postgres::PgOutboxStore) -> forge_events::Dispatcher + Send>,
    >,
    settings: forge_events::DispatcherSettings,
}

#[cfg(feature = "events")]
impl fmt::Debug for EventsSetup {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EventsSetup")
            .field("publisher", &self.publisher.is_some())
            .field("settings", &self.settings)
            .finish()
    }
}

#[cfg(feature = "events")]
impl EventsSetup {
    /// Publishes committed outbox events to `publisher` (for example a
    /// [`forge_events::Subscribers`] registry). Without a publisher, events
    /// accumulate in the outbox and no dispatcher runs.
    pub fn publish_to(&mut self, publisher: impl forge_events::EventPublisher) -> &mut Self {
        self.publisher = Some(Box::new(move |store| {
            forge_events::Dispatcher::new(store, publisher)
        }));
        self
    }

    /// Replaces the dispatcher settings.
    pub fn settings(&mut self, settings: forge_events::DispatcherSettings) -> &mut Self {
        self.settings = settings;
        self
    }
}

/// The registrations collected from composition hooks.
#[derive(Debug, Default)]
pub(crate) struct JobsParts {
    pub(crate) jobs: Jobs,
    #[cfg(feature = "events")]
    pub(crate) events: Option<EventsSetup>,
}

impl JobsParts {
    pub(crate) fn new(declaration: JobsDeclaration) -> Self {
        let _ = declaration;
        Self {
            jobs: Jobs::new(),
            #[cfg(feature = "events")]
            events: declaration.events.then(EventsSetup::default),
        }
    }

    /// Builds the runtime over the runtime pool, instrumented when telemetry
    /// is on.
    pub(crate) fn into_background(
        self,
        database: PgDatabase,
        telemetry: Option<&forge_telemetry::Telemetry>,
    ) -> Background {
        let mut jobs = self.jobs;
        if let Some(telemetry) = telemetry {
            match telemetry.jobs() {
                Ok(instrumentation) => {
                    jobs.instrument(instrumentation);
                }
                Err(error) => warn!(%error, "job telemetry is not recorded"),
            }
        }
        #[cfg(feature = "events")]
        let dispatcher = self.events.and_then(|events| {
            let EventsSetup { publisher, settings } = events;
            let Some(publisher) = publisher else {
                warn!("App::events is declared but no composition hook set a publisher; the outbox will not be dispatched");
                return None;
            };
            let dispatcher = publisher(forge_events::postgres::PgOutboxStore::new(database.clone())).settings(settings);
            Some(match telemetry.map(forge_telemetry::Telemetry::outbox) {
                Some(Ok(instrumentation)) => dispatcher.instrument(instrumentation),
                Some(Err(error)) => {
                    warn!(%error, "outbox telemetry is not recorded");
                    dispatcher
                }
                None => dispatcher,
            })
        });
        Background {
            jobs,
            #[cfg(feature = "events")]
            dispatcher,
            database,
        }
    }
}

/// The supervised job runtime and outbox dispatcher of one process.
pub(crate) struct Background {
    jobs: Jobs,
    #[cfg(feature = "events")]
    dispatcher: Option<forge_events::Dispatcher>,
    database: PgDatabase,
}

impl fmt::Debug for Background {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Background")
            .field("jobs", &self.jobs)
            .finish_non_exhaustive()
    }
}

/// The job runtime or the dispatcher failed.
#[derive(Debug, Error)]
#[non_exhaustive]
pub(crate) enum BackgroundError {
    #[error("job runtime failed")]
    Jobs(#[from] JobsError),
    #[cfg(feature = "events")]
    #[error("outbox dispatcher failed")]
    Events(#[from] forge_events::EventsError),
}

impl Background {
    /// Runs until `shutdown` is cancelled, then drains. A failing part stops
    /// the other one and is returned; the caller should then stop the
    /// process.
    pub(crate) async fn run(self, shutdown: Cancellation) -> Result<(), BackgroundError> {
        let stop = shutdown.child();
        // `forge new` declares jobs before the application has any, so an
        // empty registry is the normal starting point, not a mistake.
        if self.jobs.is_empty() {
            info!("no job handler is registered yet; only finished jobs are pruned");
        }
        let store = PgJobStore::new(self.database.clone());
        let jobs = async {
            let result = self.jobs.run(store, stop.clone()).await;
            if result.is_err() {
                stop.cancel();
            }
            result.map_err(BackgroundError::from)
        };
        #[cfg(feature = "events")]
        let events = async {
            let Some(dispatcher) = self.dispatcher else {
                return Ok(());
            };
            let result = dispatcher.run(stop.clone()).await;
            if result.is_err() {
                stop.cancel();
            }
            result.map_err(BackgroundError::from)
        };
        #[cfg(not(feature = "events"))]
        let events = async { Ok::<(), BackgroundError>(()) };
        let (jobs, events) = tokio::join!(jobs, events);
        jobs.and(events)
    }
}

/// Refuses jobs on backends other than PostgreSQL, and without a database.
pub(crate) fn check_backend(scheme: Option<forge_config::DatabaseScheme>) -> Result<(), String> {
    match scheme {
        None => Err(
            "App::jobs requires App::postgres (durable jobs are stored in PostgreSQL)".to_owned(),
        ),
        Some(scheme) => forge_jobs::ensure_supported(scheme).map_err(|error| error.to_string()),
    }
}
