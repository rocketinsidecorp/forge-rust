//! Forge application facade.
//!
//! Generated applications depend on this crate only. [`App`] owns the process
//! lifecycle: command-line dispatch, configuration loading, logging, the Tokio
//! runtime, signal handling, graceful shutdown, and the container health probe.
//!
//! ```no_run
//! use forge::http::{Method, Response, StatusCode};
//!
//! fn main() -> std::process::ExitCode {
//!     forge::App::new("my-service")
//!         .version(env!("CARGO_PKG_VERSION"))
//!         .routes(|router| {
//!             router.route(Method::GET, "/hello", |_| async {
//!                 Response::new(StatusCode::OK, "hello")
//!             })?;
//!             Ok(())
//!         })
//!         .run()
//! }
//! ```

mod app;
#[cfg(feature = "jobs")]
mod background;
mod cli;
mod command;
mod compose;
mod console;
mod database;
mod logging;
#[cfg(feature = "jobs")]
mod operator;
mod probe;
mod signal;
pub mod web;
mod worker;

pub use forge_audit as audit;
pub use forge_auth as auth;
/// Application cache with per-process stampede protection (feature `cache`,
/// ADR 0028).
#[cfg(feature = "cache")]
pub use forge_cache as cache;
pub use forge_config as config;
pub use forge_core as core;
pub use forge_db as db;
/// Integration events, the transactional outbox and idempotent consumers
/// (feature `events`, ADR 0025).
#[cfg(feature = "events")]
pub use forge_events as events;
pub use forge_http as http;
/// Durable jobs, workers and cron schedules (feature `jobs`, ADR 0025).
#[cfg(feature = "jobs")]
pub use forge_jobs as jobs;
/// Fixed-window rate limiting and its HTTP middleware (feature `ratelimit`,
/// ADR 0027).
#[cfg(feature = "ratelimit")]
pub use forge_ratelimit as ratelimit;
pub use forge_security as security;
/// File storage, signed links and strict uploads (feature `storage`, ADR 0029).
#[cfg(feature = "storage")]
pub use forge_storage as storage;
pub use forge_tenancy as tenancy;
/// Server-rendered views: HTML responses, CSRF protection, flash messages,
/// digested assets, the import map and the Content-Security-Policy (feature
/// `views`, ADR 0022). HTTP adapters only; domain and application code never
/// use it.
#[cfg(feature = "views")]
pub use forge_views as views;

pub use app::App;
#[cfg(feature = "events")]
pub use background::EventsSetup;
#[cfg(feature = "jobs")]
pub use background::WorkerMode;
pub use command::{CommandContext, CommandError, CommandOptions};
pub use compose::{ComposeError, Composition};
pub use worker::{RunningWorkers, Shutdown, SubmitError, WorkQueue};
