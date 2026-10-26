//! `jobs status|dead|retry|discard|dead-events|retry-event`: operator
//! commands over the runtime role (feature `jobs`). Payloads are never
//! printed.

use std::{fmt::Write as _, process::ExitCode};

use forge_db::PgDatabase;
use forge_jobs::postgres::PgJobStore;
use tracing::error;

use crate::{app::ErrorChain, cli::JobsCommand};

/// Runs an operator command and returns the text to print, or `None` for
/// `Work` (handled by the caller).
pub(crate) async fn run(
    command: JobsCommand,
    database: &PgDatabase,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let jobs = PgJobStore::new(database.clone());
    let mut out = String::new();
    match command {
        JobsCommand::Work => {}
        JobsCommand::Status => {
            let stats = jobs.stats().await?;
            if stats.is_empty() {
                out.push_str("no jobs\n");
            } else {
                writeln!(
                    out,
                    "{:<24} {:<10} {:>10}  OLDEST_DUE",
                    "QUEUE", "STATE", "JOBS"
                )?;
                for row in stats {
                    let oldest = row
                        .oldest_run_at
                        .map_or_else(|| "-".to_owned(), |time| time.to_string());
                    writeln!(
                        out,
                        "{:<24} {:<10} {:>10}  {oldest}",
                        row.queue,
                        row.state.as_str(),
                        row.jobs
                    )?;
                }
            }
        }
        JobsCommand::Dead { limit } => {
            let dead = jobs.dead(limit, None).await?;
            if dead.is_empty() {
                out.push_str("no dead jobs\n");
            }
            for job in dead {
                let failed = job
                    .failed_at
                    .map_or_else(|| "-".to_owned(), |time| time.to_string());
                let tenant = job
                    .tenant_id
                    .map_or_else(|| "-".to_owned(), |tenant| tenant.to_string());
                writeln!(
                    out,
                    "{}  {}  {}  attempts={}/{}  failed_at={failed}  tenant={tenant}  error={}",
                    job.id,
                    job.queue,
                    job.name,
                    job.attempts,
                    job.max_attempts,
                    job.last_error.as_deref().unwrap_or("-"),
                )?;
            }
        }
        JobsCommand::Retry(id) => {
            if !jobs.retry(id).await? {
                return Err(format!("no dead job {id}").into());
            }
            writeln!(out, "requeued job {id}")?;
        }
        JobsCommand::Discard(id) => {
            if !jobs.discard(id).await? {
                return Err(format!("no dead job {id}").into());
            }
            writeln!(out, "discarded job {id}")?;
        }
        #[cfg(feature = "events")]
        JobsCommand::DeadEvents { limit } => {
            let outbox = forge_events::postgres::PgOutboxStore::new(database.clone());
            let dead = outbox.dead(limit, None).await?;
            if dead.is_empty() {
                out.push_str("no dead events\n");
            }
            for event in dead {
                writeln!(
                    out,
                    "{}  {}  v{}  attempts={}  error={}",
                    event.id,
                    event.event_type,
                    event.version,
                    event.attempts,
                    event.last_error.as_deref().unwrap_or("-"),
                )?;
            }
        }
        #[cfg(feature = "events")]
        JobsCommand::RetryEvent(id) => {
            let outbox = forge_events::postgres::PgOutboxStore::new(database.clone());
            if !outbox.retry(id).await? {
                return Err(format!("no dead event {id}").into());
            }
            writeln!(out, "requeued event {id}")?;
        }
    }
    Ok(out)
}

/// Connects with the runtime URL, refuses a role that could bypass
/// row-level security outside development and test (ADR 0044), runs
/// `command` and prints the result.
pub(crate) async fn execute(
    app: &'static str,
    url: &forge_config::SecretString,
    command: JobsCommand,
    environment: forge_config::Environment,
) -> Result<String, ExitCode> {
    let settings = forge_db::PoolSettings::default().with_environment(environment);
    let database = match PgDatabase::connect_with(url, settings).await {
        Ok(database) => database,
        Err(failure) => {
            error!(app, error = %ErrorChain(&failure), "failed to connect to the database");
            return Err(ExitCode::FAILURE);
        }
    };
    let handle = crate::database::DatabaseHandle::Postgres(database.clone());
    crate::app::require_row_security(app, Some(&handle), environment).await?;
    let result = run(command, &database).await;
    database.close().await;
    result.map_err(|failure| {
        error!(app, error = %ErrorChain(failure.as_ref()), "jobs command failed");
        ExitCode::FAILURE
    })
}
