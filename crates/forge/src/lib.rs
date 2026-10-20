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
mod cli;
mod command;
mod compose;
mod console;
mod database;
mod logging;
mod probe;
mod signal;
pub mod web;
mod worker;

pub use forge_audit as audit;
pub use forge_auth as auth;
pub use forge_config as config;
pub use forge_core as core;
pub use forge_db as db;
pub use forge_http as http;
pub use forge_security as security;
pub use forge_tenancy as tenancy;
/// Server-rendered views: HTML responses, CSRF protection, flash messages,
/// digested assets, the import map and the Content-Security-Policy (feature
/// `views`, ADR 0022). HTTP adapters only; domain and application code never
/// use it.
#[cfg(feature = "views")]
pub use forge_views as views;

pub use app::App;
pub use command::{CommandContext, CommandError, CommandOptions};
pub use compose::{ComposeError, Composition};
pub use worker::{RunningWorkers, Shutdown, SubmitError, WorkQueue};
