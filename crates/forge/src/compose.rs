//! Composition root hook: wiring adapters and routes with infrastructure.

use std::{error::Error, fmt, future::Future};

use forge_config::AppConfig;
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
}

impl<'a> Composition<'a> {
    pub(crate) fn new(
        router: &'a mut Router,
        config: &'a AppConfig,
        database: Option<DatabaseHandle>,
    ) -> Self {
        Self {
            router,
            config,
            database,
            workers: Vec::new(),
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
        Self::new(router, config, None)
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
