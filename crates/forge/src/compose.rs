//! Composition root hook: wiring adapters and routes with infrastructure.

use std::{error::Error, fmt, future::Future};

use forge_config::AppConfig;
use forge_core::Cancellation;
use forge_http::{RouteError, Router};
use thiserror::Error;

use crate::database::DatabaseHandle;
use crate::worker::{self, Shutdown, WorkQueue, Worker};

/// A registered composition step, run once at startup.
pub(crate) type ComposeHook =
    Box<dyn FnOnce(&mut Composition<'_>) -> Result<(), ComposeError> + Send + 'static>;

/// Startup context handed to [`App::compose`](crate::App::compose) hooks.
///
/// It gives the composition root access to the validated configuration, the
/// route table and the pool of the database the application declared (with
/// `App::postgres`, `App::mysql` or `App::sqlite`), so adapters and use cases
/// can be constructed with their concrete dependencies. Hooks run inside the
/// Tokio runtime, after the standard health routes are registered and before
/// the server accepts connections.
///
/// With the `testing` feature, [`Composition::for_testing`] builds one outside
/// the process lifecycle, so an application's composition function can be
/// exercised in tests.
pub struct Composition<'a> {
    router: &'a mut Router,
    config: &'a AppConfig,
    database: Option<DatabaseHandle>,
    workers: Vec<Worker>,
    shutdown: Cancellation,
    telemetry: Option<forge_telemetry::Telemetry>,
    #[cfg(feature = "jobs")]
    jobs: Option<&'a mut forge_jobs::Jobs>,
    #[cfg(feature = "events")]
    events: Option<&'a mut crate::background::EventsSetup>,
}

impl<'a> Composition<'a> {
    pub(crate) fn new(
        router: &'a mut Router,
        config: &'a AppConfig,
        database: Option<DatabaseHandle>,
        shutdown: Cancellation,
        telemetry: Option<forge_telemetry::Telemetry>,
    ) -> Self {
        Self {
            router,
            config,
            database,
            workers: Vec::new(),
            shutdown,
            telemetry,
            #[cfg(feature = "jobs")]
            jobs: None,
            #[cfg(feature = "events")]
            events: None,
        }
    }

    /// The workers registered so far, for the process lifecycle.
    pub(crate) fn into_workers(self) -> Vec<Worker> {
        self.workers
    }

    /// Registers a background worker owned by the process lifecycle.
    ///
    /// `run` starts when the server starts. Its [`Shutdown`] completes once
    /// the server has drained its connections; the worker should then
    /// return. Forge waits up to 5 seconds for it and aborts it after that.
    pub fn worker<F, Fut>(&mut self, name: &'static str, run: F)
    where
        F: FnOnce(Shutdown) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.workers.push(Worker {
            name,
            run: Box::new(move |shutdown| Box::pin(run(shutdown))),
        });
    }

    /// Registers a bounded queue and the worker that handles its items one
    /// at a time, and returns the queue's sending side.
    ///
    /// [`WorkQueue::submit`] never waits: when `capacity` items are queued it
    /// refuses the item. At shutdown the worker finishes the item in hand;
    /// items still queued are dropped and counted in the log. Use it for work
    /// a request must not wait for (or must not be timed by), such as
    /// delivering a password-reset link; it is not a durable job queue.
    pub fn work_queue<T, F, Fut>(
        &mut self,
        name: &'static str,
        capacity: usize,
        handle: F,
    ) -> WorkQueue<T>
    where
        T: Send + 'static,
        F: Fn(T) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let (queue, worker) = worker::queue(name, capacity, handle);
        self.workers.push(worker);
        queue
    }

    /// Hands the declared job registrations to the hooks.
    #[cfg(feature = "jobs")]
    pub(crate) fn attach_jobs(&mut self, parts: &'a mut crate::background::JobsParts) {
        self.jobs = Some(&mut parts.jobs);
        #[cfg(feature = "events")]
        {
            self.events = parts.events.as_mut();
        }
    }

    /// Returns the job registry: register handlers, per-queue concurrency,
    /// cron schedules, worker timing and retention (feature `jobs`).
    ///
    /// Fails with [`ComposeError::JobsNotDeclared`] unless the application
    /// called [`App::jobs`](crate::App::jobs).
    #[cfg(feature = "jobs")]
    pub fn jobs(&mut self) -> Result<&mut forge_jobs::Jobs, ComposeError> {
        self.jobs
            .as_deref_mut()
            .ok_or(ComposeError::JobsNotDeclared)
    }

    /// Returns the outbox wiring: the publisher committed events go to and
    /// the dispatcher settings (feature `events`).
    ///
    /// Fails with [`ComposeError::EventsNotDeclared`] unless the application
    /// called [`App::events`](crate::App::events).
    #[cfg(feature = "events")]
    pub fn events(&mut self) -> Result<&mut crate::background::EventsSetup, ComposeError> {
        self.events
            .as_deref_mut()
            .ok_or(ComposeError::EventsNotDeclared)
    }

    /// A signal cancelled when the process starts shutting down: before the
    /// HTTP server drains its connections and before workers are told to
    /// stop.
    ///
    /// Long-lived responses (such as the event streams of
    /// `forge::broadcast`) end on it, so that open connections do not hold
    /// up the drain until the grace period. The returned value is a child:
    /// cancelling it never stops the application.
    #[must_use]
    pub fn shutdown_started(&self) -> Cancellation {
        self.shutdown.child()
    }

    /// Builds the broker that live updates are published to (feature
    /// `broadcast`, ADR 0031). It closes, ending every open event stream,
    /// when shutdown begins.
    ///
    /// - [`Fanout::InProcess`] reaches the subscribers of this process only,
    ///   on any backend (or none).
    /// - [`Fanout::Postgres`] also reaches every other replica over
    ///   PostgreSQL `LISTEN/NOTIFY`, and registers the listener as a worker
    ///   of the lifecycle. It needs `App::postgres`; MySQL, MariaDB and
    ///   SQLite applications are refused with
    ///   [`forge_broadcast::BroadcastError::UnsupportedBackend`] rather than
    ///   emulated.
    ///
    /// Call it once and share the broker.
    #[cfg(feature = "broadcast")]
    pub fn broadcast(
        &mut self,
        fanout: Fanout,
    ) -> Result<std::sync::Arc<dyn forge_broadcast::Broker>, ComposeError> {
        use std::sync::Arc;

        let local = forge_broadcast::MemoryBroker::new(
            forge_broadcast::MemoryOptions::default(),
            &self.shutdown,
        )
        .map_err(ComposeError::other)?;
        match fanout {
            Fanout::InProcess => Ok(Arc::new(local)),
            Fanout::Postgres => {
                let Some(database) = &self.database else {
                    return Err(ComposeError::DatabaseNotDeclared {
                        requested: "postgres",
                    });
                };
                forge_broadcast::ensure_fanout_supported(database.scheme())
                    .map_err(ComposeError::other)?;
                self.postgres_fanout(local)
            }
        }
    }

    /// The `LISTEN/NOTIFY` broker over the declared PostgreSQL pool, with its
    /// listener registered as a lifecycle worker.
    #[cfg(all(feature = "broadcast", feature = "postgres"))]
    fn postgres_fanout(
        &mut self,
        local: forge_broadcast::MemoryBroker,
    ) -> Result<std::sync::Arc<dyn forge_broadcast::Broker>, ComposeError> {
        let broker = forge_broadcast::postgres::PgBroker::new(
            self.postgres()?.clone(),
            local,
            forge_broadcast::postgres::PgOptions::default(),
        )
        .map_err(ComposeError::other)?;
        let listener = broker.clone();
        self.worker("broadcast-listener", move |_stopped| async move {
            // Returns when the broker closes, as shutdown begins.
            listener.listen().await;
        });
        Ok(std::sync::Arc::new(broker))
    }

    /// Without the `postgres` feature there is no PostgreSQL pool to fan out
    /// over.
    #[cfg(all(feature = "broadcast", not(feature = "postgres")))]
    #[allow(clippy::unused_self)]
    fn postgres_fanout(
        &mut self,
        _local: forge_broadcast::MemoryBroker,
    ) -> Result<std::sync::Arc<dyn forge_broadcast::Broker>, ComposeError> {
        Err(ComposeError::DatabaseNotDeclared {
            requested: "postgres",
        })
    }

    /// The process telemetry (ADR 0033), `None` when
    /// `FORGE_TELEMETRY_ENABLED=false`: register application metrics on
    /// [`Telemetry::metrics`](forge_telemetry::Telemetry::metrics) and start
    /// spans with [`Telemetry::tracer`](forge_telemetry::Telemetry::tracer),
    /// parented by [`Request::trace_context`](forge_http::Request::trace_context).
    /// Bootstrap and adapters only; domain and application code never see it.
    #[must_use]
    pub fn telemetry(&self) -> Option<&forge_telemetry::Telemetry> {
        self.telemetry.as_ref()
    }

    /// Returns the route table for registering application routes.
    pub fn router(&mut self) -> &mut Router {
        self.router
    }

    /// Returns the validated process configuration.
    #[must_use]
    pub fn config(&self) -> &AppConfig {
        self.config
    }

    /// Returns the application's PostgreSQL pool.
    ///
    /// The pool connects lazily, so this succeeds even while the database is
    /// unreachable; queries then fail with a retryable error and
    /// `/health/ready` reports `503`. Clone the handle to move it into
    /// adapters. Fails with [`ComposeError::DatabaseNotDeclared`] unless the
    /// application called [`App::postgres`](crate::App::postgres).
    #[cfg(feature = "postgres")]
    pub fn postgres(&self) -> Result<&forge_db::PgDatabase, ComposeError> {
        match &self.database {
            Some(DatabaseHandle::Postgres(database)) => Ok(database),
            _ => Err(ComposeError::DatabaseNotDeclared {
                requested: "postgres",
            }),
        }
    }

    /// Returns the application's MySQL 8.4+ / MariaDB 11.4+ pool.
    ///
    /// Behaves like `Composition::postgres` for an application that called
    /// [`App::mysql`](crate::App::mysql); `mysql://` and `mariadb://` URLs
    /// both select this backend.
    #[cfg(feature = "mysql")]
    pub fn mysql(&self) -> Result<&forge_db::MySqlDatabase, ComposeError> {
        match &self.database {
            Some(DatabaseHandle::MySql(database)) => Ok(database),
            _ => Err(ComposeError::DatabaseNotDeclared { requested: "mysql" }),
        }
    }

    /// Returns the application's SQLite pool.
    ///
    /// Behaves like `Composition::postgres` for an application that called
    /// [`App::sqlite`](crate::App::sqlite).
    #[cfg(feature = "sqlite")]
    pub fn sqlite(&self) -> Result<&forge_db::SqliteDatabase, ComposeError> {
        match &self.database {
            Some(DatabaseHandle::Sqlite(database)) => Ok(database),
            _ => Err(ComposeError::DatabaseNotDeclared {
                requested: "sqlite",
            }),
        }
    }
}

/// Test support: run composition functions without the process lifecycle.
///
/// ```
/// use forge::{
///     ComposeError, Composition,
///     config::AppConfig,
///     http::{Method, Response, Router, StatusCode},
/// };
///
/// fn compose(composition: &mut Composition<'_>) -> Result<(), ComposeError> {
///     composition.router().route(Method::GET, "/ping", |_| async {
///         Response::empty(StatusCode::NO_CONTENT)
///     })?;
///     Ok(())
/// }
///
/// let config = AppConfig::default();
/// let mut router = Router::new();
/// compose(&mut Composition::for_testing(&config, &mut router))?;
/// assert!(format!("{router:?}").contains("GET /ping"));
/// # Ok::<(), ComposeError>(())
/// ```
#[cfg(feature = "testing")]
impl<'a> Composition<'a> {
    /// Creates a composition over `config` and `router` with no database.
    ///
    /// Unlike [`App::run`](crate::App::run), this registers no health routes
    /// and creates no pool: the router receives exactly what the composition
    /// function registers. Attach a database with `with_postgres`,
    /// `with_mysql` or `with_sqlite`, for example from
    /// `forge::db::testing::TestDatabase::database`.
    #[must_use]
    pub fn for_testing(config: &'a AppConfig, router: &'a mut Router) -> Self {
        Self::new(router, config, None, Cancellation::new(), None)
    }

    /// Makes `telemetry` available through [`Composition::telemetry`].
    #[must_use]
    pub fn with_telemetry(mut self, telemetry: forge_telemetry::Telemetry) -> Self {
        self.telemetry = Some(telemetry);
        self
    }

    /// Uses `shutdown` as the process shutdown signal, so a test can begin
    /// shutdown by cancelling it.
    #[must_use]
    pub fn with_shutdown(mut self, shutdown: Cancellation) -> Self {
        self.shutdown = shutdown;
        self
    }

    /// Starts the registered workers on the current Tokio runtime, as
    /// `serve` does; stop them with [`RunningWorkers::stop`](crate::RunningWorkers::stop).
    #[must_use]
    pub fn start_workers(self) -> crate::RunningWorkers {
        crate::RunningWorkers::start(self.workers)
    }

    /// Makes `database` available through [`Composition::postgres`].
    #[cfg(feature = "postgres")]
    #[must_use]
    pub fn with_postgres(mut self, database: forge_db::PgDatabase) -> Self {
        self.database = Some(DatabaseHandle::Postgres(database));
        self
    }

    /// Makes `database` available through [`Composition::mysql`].
    #[cfg(feature = "mysql")]
    #[must_use]
    pub fn with_mysql(mut self, database: forge_db::MySqlDatabase) -> Self {
        self.database = Some(DatabaseHandle::MySql(database));
        self
    }

    /// Makes `database` available through [`Composition::sqlite`].
    #[cfg(feature = "sqlite")]
    #[must_use]
    pub fn with_sqlite(mut self, database: forge_db::SqliteDatabase) -> Self {
        self.database = Some(DatabaseHandle::Sqlite(database));
        self
    }

    /// Makes `jobs` available through [`Composition::jobs`].
    #[cfg(feature = "jobs")]
    #[must_use]
    pub fn with_jobs(mut self, jobs: &'a mut forge_jobs::Jobs) -> Self {
        self.jobs = Some(jobs);
        self
    }

    /// Makes `events` available through [`Composition::events`].
    #[cfg(feature = "events")]
    #[must_use]
    pub fn with_events(mut self, events: &'a mut crate::background::EventsSetup) -> Self {
        self.events = Some(events);
        self
    }
}

impl fmt::Debug for Composition<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Composition")
            .field("router", &self.router)
            .field("database", &self.database)
            .field("workers", &self.workers.len())
            .finish_non_exhaustive()
    }
}

/// Where live updates published through [`Composition::broadcast`] reach
/// (feature `broadcast`).
#[cfg(feature = "broadcast")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Fanout {
    /// The subscribers of this process only. Any backend; enough for one
    /// replica.
    InProcess,
    /// Every replica, over PostgreSQL `LISTEN/NOTIFY` (PostgreSQL only).
    Postgres,
}

/// A composition hook failed; startup is aborted with exit code 1.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ComposeError {
    /// A composition hook asked for a database the application did not
    /// declare, e.g. `Composition::postgres` without `App::postgres`.
    #[error(
        "the application requested the {requested} database but did not declare it with \
         App::{requested}"
    )]
    DatabaseNotDeclared {
        /// The requested backend: `"postgres"`, `"mysql"` or `"sqlite"`.
        requested: &'static str,
    },
    /// A composition hook asked for the job registry without `App::jobs`.
    #[cfg(feature = "jobs")]
    #[error("the application requested jobs but did not declare them with App::jobs")]
    JobsNotDeclared,
    /// A composition hook asked for the outbox without `App::events`.
    #[cfg(feature = "events")]
    #[error("the application requested the outbox but did not declare it with App::events")]
    EventsNotDeclared,
    /// A route could not be registered, for example because it is a duplicate.
    #[error("failed to register routes")]
    Route(#[from] RouteError),
    /// Any other failure while building adapters or use cases.
    #[error("failed to compose the application")]
    Other(#[from] Box<dyn Error + Send + Sync + 'static>),
}

impl ComposeError {
    /// Wraps an arbitrary error as [`ComposeError::Other`].
    #[must_use]
    pub fn other(error: impl Error + Send + Sync + 'static) -> Self {
        Self::Other(Box::new(error))
    }
}

#[cfg(test)]
mod compose_tests;

#[cfg(test)]
mod tests {
    use std::io;

    use super::*;

    #[test]
    fn errors_have_useful_messages_and_sources() {
        let route = ComposeError::from(RouteError::InvalidPath("x".to_owned()));
        assert_eq!(route.to_string(), "failed to register routes");
        assert!(route.source().is_some());

        let other = ComposeError::other(io::Error::other("adapter failed"));
        assert_eq!(other.to_string(), "failed to compose the application");
        assert_eq!(
            other.source().map(ToString::to_string).as_deref(),
            Some("adapter failed")
        );

        let undeclared = ComposeError::DatabaseNotDeclared {
            requested: "postgres",
        };
        assert!(undeclared.to_string().contains("App::postgres"));
    }

    #[cfg(feature = "testing")]
    #[test]
    fn testing_composition_starts_without_a_database() {
        let config = AppConfig::default();
        let mut router = Router::new();
        let composition = Composition::for_testing(&config, &mut router);

        assert!(format!("{composition:?}").contains("database: None"));
        #[cfg(feature = "postgres")]
        assert!(matches!(
            composition.postgres(),
            Err(ComposeError::DatabaseNotDeclared {
                requested: "postgres"
            })
        ));
        #[cfg(feature = "mysql")]
        assert!(matches!(
            composition.mysql(),
            Err(ComposeError::DatabaseNotDeclared { requested: "mysql" })
        ));
        #[cfg(feature = "sqlite")]
        assert!(matches!(
            composition.sqlite(),
            Err(ComposeError::DatabaseNotDeclared {
                requested: "sqlite"
            })
        ));
    }

    #[cfg(all(feature = "testing", feature = "broadcast"))]
    #[tokio::test]
    async fn the_in_process_broker_closes_when_shutdown_begins() -> Result<(), Box<dyn Error>> {
        let config = AppConfig::default();
        let mut router = Router::new();
        let shutdown = Cancellation::new();
        let mut composition =
            Composition::for_testing(&config, &mut router).with_shutdown(shutdown.clone());
        let started = composition.shutdown_started();
        let broker = composition.broadcast(Fanout::InProcess)?;
        let channel = forge_broadcast::Channel::global("posts")?;
        let mut subscription = broker.subscribe(&channel)?;

        // Cancelling the handed-out child never stops the application.
        composition.shutdown_started().cancel();
        assert!(!shutdown.is_cancelled() && !started.is_cancelled());

        shutdown.cancel();
        assert!(started.is_cancelled());
        assert_eq!(
            subscription.next().await,
            Err(forge_broadcast::SubscriptionEnd::Closed)
        );
        assert!(broker.subscribe(&channel).is_err());
        Ok(())
    }

    #[cfg(all(feature = "testing", feature = "broadcast"))]
    #[test]
    fn postgres_fanout_needs_a_postgres_database() {
        let config = AppConfig::default();
        let mut router = Router::new();
        let mut composition = Composition::for_testing(&config, &mut router);

        assert!(matches!(
            composition.broadcast(Fanout::Postgres),
            Err(ComposeError::DatabaseNotDeclared {
                requested: "postgres"
            })
        ));
    }

    #[cfg(all(feature = "testing", feature = "broadcast", feature = "sqlite"))]
    #[tokio::test]
    async fn postgres_fanout_is_refused_on_sqlite_not_emulated() -> Result<(), Box<dyn Error>> {
        let config = AppConfig::default();
        let mut router = Router::new();
        let test =
            forge_db::testing::TestDatabase::<forge_db::sqlx::Sqlite>::create_empty().await?;
        let mut composition =
            Composition::for_testing(&config, &mut router).with_sqlite(test.database().clone());

        let refused = composition
            .broadcast(Fanout::Postgres)
            .err()
            .ok_or("SQLite must be refused")?;
        let reason = refused
            .source()
            .map(ToString::to_string)
            .unwrap_or_default();
        assert!(reason.contains("requires PostgreSQL"), "{reason}");
        assert!(reason.contains("refused rather than emulated"), "{reason}");
        assert!(composition.broadcast(Fanout::InProcess).is_ok());
        assert_eq!(composition.workers.len(), 0);
        test.cleanup().await?;
        Ok(())
    }

    #[cfg(all(feature = "testing", feature = "broadcast", feature = "postgres"))]
    #[tokio::test]
    async fn postgres_fanout_registers_its_listener_as_a_worker() -> Result<(), Box<dyn Error>> {
        let config = AppConfig::default();
        let mut router = Router::new();
        // A lazy pool never connects here: only the wiring is checked.
        let url = forge_config::SecretString::new("postgres://app:secret@127.0.0.1:1/app");
        let database = forge_db::PgDatabase::connect_lazy(&url)?;
        let shutdown = Cancellation::new();
        let mut composition = Composition::for_testing(&config, &mut router)
            .with_postgres(database)
            .with_shutdown(shutdown.clone());

        let _broker = composition.broadcast(Fanout::Postgres)?;

        assert_eq!(composition.workers.len(), 1);
        assert_eq!(composition.workers[0].name, "broadcast-listener");
        // The listener returns once shutdown begins, so stopping is prompt.
        let workers = composition.start_workers();
        shutdown.cancel();
        tokio::time::timeout(std::time::Duration::from_secs(5), workers.stop()).await?;
        Ok(())
    }

    #[cfg(all(feature = "testing", feature = "sqlite"))]
    #[tokio::test]
    async fn testing_composition_exposes_only_the_attached_database() -> Result<(), Box<dyn Error>>
    {
        let config = AppConfig::default();
        let mut router = Router::new();
        let test =
            forge_db::testing::TestDatabase::<forge_db::sqlx::Sqlite>::create_empty().await?;

        let composition =
            Composition::for_testing(&config, &mut router).with_sqlite(test.database().clone());
        composition.sqlite()?.ping().await?;
        #[cfg(feature = "postgres")]
        assert!(composition.postgres().is_err());
        #[cfg(feature = "mysql")]
        assert!(composition.mysql().is_err());
        test.cleanup().await?;
        Ok(())
    }
}
