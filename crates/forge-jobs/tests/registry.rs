//! The job registry, schedules and value types through the public API, with
//! no database: what registration refuses and what the small types report.

use std::time::Duration;

use async_trait::async_trait;
use forge_jobs::{
    Backoff, Cron, Job, JobContext, JobError, JobHandler, Jobs, JobsError, Schedule,
    postgres::{JobState, Pruning},
};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
struct Instant;

impl Job for Instant {
    const NAME: &'static str = "registry.instant";
    const TIMEOUT: Duration = Duration::ZERO;
}

struct InstantHandler;

#[async_trait]
impl JobHandler for InstantHandler {
    type Job = Instant;

    async fn perform(&self, _: &JobContext, _: Instant) -> Result<(), JobError> {
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
struct Report;

impl Job for Report {
    const NAME: &'static str = "registry.report";
}

#[test]
fn a_job_that_may_not_run_at_all_cannot_be_registered() {
    let mut jobs = Jobs::default();
    assert!(jobs.is_empty());
    match jobs.register(InstantHandler) {
        Err(JobsError::InvalidSetting(reason)) => assert_eq!(reason, "TIMEOUT must be positive"),
        other => panic!("{:?}", other.map(|_| ())),
    }
    assert!(jobs.is_empty());
}

#[test]
fn schedules_report_their_name_and_expression_and_floor_their_lateness() {
    let cron = Cron::parse("*/5 * * * *").expect("valid");
    assert_eq!(format!("{cron:?}"), "Cron(\"*/5 * * * *\")");
    let schedule = Schedule::new("registry-report", cron.clone(), &Report).expect("valid");
    assert_eq!(schedule.name(), "registry-report");
    assert_eq!(format!("{:?}", schedule.cron()), format!("{cron:?}"));
    let tick = forge_core::Timestamp::parse("2026-10-26T10:05:00Z").expect("tick");
    // By default a tick may be one interval late: the gap to the next tick.
    assert_eq!(schedule.lateness_of(tick), Duration::from_secs(300));
    // A shorter lateness than a minute is raised to a minute, never to nothing.
    let strict = schedule.clone().max_lateness(Duration::from_secs(1));
    assert_eq!(strict.name(), "registry-report");
    assert_eq!(strict.lateness_of(tick), Duration::from_secs(60));
    let loose = schedule.clone().max_lateness(Duration::from_secs(3600));
    assert_eq!(loose.lateness_of(tick), Duration::from_secs(3600));
    // Uneven schedules: the gap from this tick to the following one.
    let workdays = Schedule::new(
        "registry-workdays",
        Cron::parse("0 9 * * 1-5").expect("valid"),
        &Report,
    )
    .expect("valid");
    let friday = forge_core::Timestamp::parse("2026-10-30T09:00:00Z").expect("friday");
    assert_eq!(workdays.lateness_of(friday), Duration::from_secs(72 * 3600));
    let monday = forge_core::Timestamp::parse("2026-10-26T09:00:00Z").expect("monday");
    assert_eq!(workdays.lateness_of(monday), Duration::from_secs(24 * 3600));
    let daily = Schedule::new(
        "registry-daily",
        Cron::parse("30 3 * * *").expect("valid"),
        &Report,
    )
    .expect("valid");
    assert_eq!(
        daily.lateness_of(forge_core::Timestamp::parse("2026-10-26T03:30:00Z").expect("tick")),
        Duration::from_secs(24 * 3600)
    );
    assert!(Schedule::new("Not Valid", cron, &Report).is_err());
}

#[test]
fn small_types_have_the_defaults_and_names_operators_see() {
    assert_eq!(Backoff::default(), Backoff::DEFAULT);
    assert_eq!(JobState::Running.as_str(), "running");
    assert_eq!(JobState::Completed.as_str(), "completed");
    assert_eq!(JobState::Queued.as_str(), "queued");
    assert_eq!(JobState::Dead.as_str(), "dead");
    let pruning = Pruning::default()
        .dead_after(Some(Duration::from_secs(7 * 86_400)))
        .interval(Duration::from_secs(30));
    assert_eq!(pruning.dead_after, Some(Duration::from_secs(7 * 86_400)));
    assert_eq!(pruning.interval, Duration::from_secs(30));
    let mut jobs = Jobs::new();
    jobs.pruning(None);
    assert!(jobs.validate().is_ok());
}

#[test]
fn job_errors_print_their_message_and_chain_their_sources() {
    #[derive(Debug)]
    struct Outer(std::io::Error);
    impl std::fmt::Display for Outer {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("could not deliver")
        }
    }
    impl std::error::Error for Outer {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.0)
        }
    }
    let error = JobError::from(Outer(std::io::Error::other("connection reset")));
    assert_eq!(error.to_string(), "could not deliver: connection reset");
    assert!(error.is_retryable());
    assert!(!JobError::permanent("bad input").is_retryable());
    assert_eq!(JobError::retry("later").message(), "later");
    assert!(
        !JobError::retry("later")
            .retry_after(Duration::from_secs(5))
            .message()
            .is_empty()
    );
}
