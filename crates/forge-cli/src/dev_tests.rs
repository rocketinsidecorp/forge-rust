//! `forge dev`'s watcher and supervisor: scan failures, an application that
//! exits by itself and one that cannot be started.

use std::cell::RefCell;
use std::fs;
use std::path::Path;
use std::rc::Rc;
use std::time::{Duration, SystemTime};

use crate::CliError;
use crate::application::BuildOutcome;
use crate::dev::{Host, Instance, Snapshot, scan, supervise};
use crate::process::Shutdown;

#[test]
fn scanning_a_missing_root_is_an_error_naming_it() {
    let directory = tempfile::tempdir().expect("tempdir");
    let missing = directory.path().join("gone");
    match scan(&missing) {
        Err(CliError::ReadDirectory { path, .. }) => assert_eq!(path, missing),
        other => panic!("expected a read error, got {other:?}"),
    }
}

#[test]
fn a_tree_with_too_many_watched_files_is_refused() {
    let directory = tempfile::tempdir().expect("tempdir");
    let source = directory.path().join("src");
    fs::create_dir(&source).expect("src");
    for index in 0..=50_000 {
        fs::write(source.join(format!("f{index}.rs")), "").expect("file");
    }
    match scan(directory.path()) {
        Err(CliError::TooManyFiles { limit, .. }) => assert_eq!(limit, 50_000),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

struct Exited;

impl Instance for Exited {
    fn exited(&mut self) -> bool {
        true
    }
    fn stop(&mut self) {}
}

/// Builds successfully once, then starts the application (or fails to).
struct OneBuild {
    starts: bool,
    built: bool,
}

impl Host for OneBuild {
    fn build(&mut self) -> Result<BuildOutcome, CliError> {
        if std::mem::replace(&mut self.built, true) {
            return Ok(BuildOutcome::Interrupted);
        }
        Ok(BuildOutcome::Built("/target/debug/app".into()))
    }

    fn start(&mut self, _: &Path) -> Result<Box<dyn Instance>, CliError> {
        if self.starts {
            Ok(Box::new(Exited))
        } else {
            Err(CliError::Usage("no executable".to_owned()))
        }
    }
}

fn run(host: &mut OneBuild, scans: Vec<Result<Snapshot, CliError>>, stop_at: &str) -> Vec<String> {
    let shutdown = Shutdown::new();
    let log: Rc<RefCell<Vec<String>>> = Rc::default();
    let queue = RefCell::new(scans.into_iter());
    let snapshot = || {
        Ok(Snapshot::from([(
            "src/main.rs".to_owned(),
            (SystemTime::UNIX_EPOCH, 1),
        )]))
    };
    let recorder = Rc::clone(&log);
    let stopper = shutdown.clone();
    let stop_at = stop_at.to_owned();
    supervise(
        host,
        || queue.borrow_mut().next().unwrap_or_else(snapshot),
        Duration::from_millis(1),
        Duration::ZERO,
        &shutdown,
        move |line| {
            recorder.borrow_mut().push(line.to_owned());
            if line.starts_with(&stop_at) {
                stopper.request();
            }
        },
    )
    .expect("supervise");
    log.borrow().clone()
}

fn tree() -> Result<Snapshot, CliError> {
    Ok(Snapshot::from([(
        "src/main.rs".to_owned(),
        (SystemTime::UNIX_EPOCH, 1),
    )]))
}

fn failing() -> Result<Snapshot, CliError> {
    Err(CliError::Usage("too many open files".to_owned()))
}

#[test]
fn the_supervisor_reports_an_exited_application_and_a_watcher_that_fails_and_recovers() {
    let mut host = OneBuild {
        starts: true,
        built: false,
    };
    // The initial scan and the one after the build, then two failures in a
    // row (reported once) and a recovery.
    let lines = run(
        &mut host,
        vec![tree(), tree(), failing(), failing(), tree()],
        "file watcher is scanning again",
    );
    assert_eq!(
        lines,
        [
            "building",
            "running",
            "application exited; waiting for changes",
            "file watcher cannot scan, changes are not seen until it can: too many open files",
            "file watcher is scanning again",
        ]
    );
}

#[test]
fn an_application_that_cannot_start_is_reported_and_the_watch_goes_on() {
    let mut host = OneBuild {
        starts: false,
        built: false,
    };
    let lines = run(
        &mut host,
        vec![tree(), tree()],
        "could not start the application",
    );
    assert_eq!(
        lines,
        ["building", "could not start the application: no executable"]
    );
}
