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
mod observability;
pub mod openapi;
#[cfg(feature = "jobs")]
mod operator;
mod probe;
mod routes;
mod signal;
pub mod web;
mod worker;

pub use forge_audit as audit;
pub use forge_auth as auth;
/// Live page updates: Turbo Streams over Server-Sent Events, signed stream
/// names and PostgreSQL `LISTEN/NOTIFY` fanout (feature `broadcast`, ADR
/// 0031). HTTP adapters and the composition root only.
#[cfg(feature = "broadcast")]
pub use forge_broadcast as broadcast;
/// Application cache with per-process stampede protection (feature `cache`,
/// ADR 0028).
#[cfg(feature = "cache")]
pub use forge_cache as cache;
pub use forge_config as config;
pub use forge_core as core;
/// Encrypted per-environment credentials, `config/credentials/<env>.json.enc`
/// (ADR 0041): read them in bootstrap with `forge::credentials::open`.
pub use forge_credentials as credentials;
pub use forge_db as db;
/// Attribute encryption with key rotation and blind indexes (ADR 0041).
/// Infrastructure adapters and the composition root only; the
/// `EncryptedColumn` re-encryption adapters follow the backend features.
pub use forge_encryption as encryption;
/// Integration events, the transactional outbox and idempotent consumers
/// (feature `events`, ADR 0025).
#[cfg(feature = "events")]
pub use forge_events as events;
pub use forge_http as http;
/// Translations (feature `i18n`, enabled by `views`, ADR 0037): the
/// `config/locales` catalogs, `%{name}` interpolation, CLDR plural rules and
/// localized numbers and dates. Jobs and mailers translate with a
/// `Translator` directly; pages use `forge::views::I18n`.
#[cfg(feature = "i18n")]
pub use forge_i18n as i18n;
/// Durable jobs, workers and cron schedules (feature `jobs`, ADR 0025).
#[cfg(feature = "jobs")]
pub use forge_jobs as jobs;
/// Email: the `Sender` port, the SMTP, development file and test senders,
/// askama templates, browser previews and the password reset mailer
/// (feature `mail`, enabled by `views`, ADR 0035). Application code depends
/// on `Sender`; bootstrap selects the sender with `from_config`.
#[cfg(feature = "mail")]
pub use forge_mail as mail;
/// Fixed-window rate limiting and its HTTP middleware (feature `ratelimit`,
/// ADR 0027).
#[cfg(feature = "ratelimit")]
pub use forge_ratelimit as ratelimit;
pub use forge_security as security;
/// File storage, signed links and strict uploads (feature `storage`, ADR 0029).
#[cfg(feature = "storage")]
pub use forge_storage as storage;
/// Traces, metrics and log export (ADR 0033): custom spans and
/// application metrics through [`Composition::telemetry`]. Bootstrap and
/// adapters only.
pub use forge_telemetry as telemetry;
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
#[cfg(feature = "broadcast")]
pub use compose::Fanout;
pub use compose::{ComposeError, Composition};
pub use worker::{RunningWorkers, Shutdown, SubmitError, WorkQueue};
