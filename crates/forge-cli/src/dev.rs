//! `forge dev`: run the application and rebuild it when files change.
//!
//! A polling watcher (standard library only, on the thread that supervises)
//! scans the source tree every `--poll-interval` and reports one batch of
//! changes once the tree has been quiet for `--debounce`. The supervisor then
//! builds with Cargo. **The old process keeps serving until a build
//! succeeds**; only then is it sent SIGTERM, awaited (up to 35 seconds, then
//! killed) and replaced. A failed build prints Cargo's diagnostics and leaves
//! the running application alone, and so does an error that kept Cargo from
//! running at all: it is printed, the previous build keeps serving, and the
//! next change builds again. When the application exits (by itself or on a
//! stop) whatever it left running in its process group is killed too. If the tree changes while a build runs, the
//! finished build is discarded and the build restarts. A crashed application
//! is not restarted in a loop: it waits for the next change.
//!
//! Polling instead of inotify keeps the dependency list unchanged and behaves
//! the same on every file system, including bind mounts. Unlike Forge-Go's
//! `forge dev` there is no browser-reload proxy yet (see ADR 0036).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use crate::CliError;
use crate::application::{BuildOutcome, cargo_build, scrub};
use crate::process::{OwnedChild, STOP_GRACE, Shutdown};

/// Shortest and longest accepted `--poll-interval` and `--debounce`.
const MIN_POLL_INTERVAL: Duration = Duration::from_millis(50);
const MAX_INTERVAL: Duration = Duration::from_secs(60);
/// A watched tree larger than this is almost certainly missing an ignore
/// rule, and polling it would burn CPU.
const MAX_FILES: usize = 50_000;
/// Changed files named in one log line.
const REPORTED_PATHS: usize = 5;

/// Parses `--poll-interval` (Go duration syntax: `500ms`, `1s`, `1m`).
pub(crate) fn parse_poll_interval(value: &str) -> Result<Duration, String> {
    let duration = forge_config::parse_go_duration(value)
        .ok_or_else(|| format!("{value:?} is not a duration such as 500ms or 1s"))?;
    if (MIN_POLL_INTERVAL..=MAX_INTERVAL).contains(&duration) {
        Ok(duration)
    } else {
        Err("must be between 50ms and 1m".to_owned())
    }
}

/// Parses `--debounce`.
pub(crate) fn parse_debounce(value: &str) -> Result<Duration, String> {
    let duration = forge_config::parse_go_duration(value)
        .ok_or_else(|| format!("{value:?} is not a duration such as 300ms or 1s"))?;
    if duration <= MAX_INTERVAL {
        Ok(duration)
    } else {
        Err("must be between 0 and 1m".to_owned())
    }
}

/// The modification time and size of every watched file, by path relative to
/// the root with `/` separators.
pub(crate) type Snapshot = BTreeMap<String, (SystemTime, u64)>;

/// Directories never entered, wherever they are.
const IGNORED_DIRECTORIES: &[&str] = &[
    "target",
    "tmp",
    "vendor",
    "node_modules",
    "storage",
    "log",
    "logs",
    "coverage",
    "dist",
];

/// Directories never entered at the root: they hold no code of the binary.
const IGNORED_ROOT_DIRECTORIES: &[&str] = &["tests", "benches", "examples", "docs", "docker"];

/// Extensions that trigger a rebuild: Rust sources (and `build.rs`),
/// templates, assets and migrations, which the binary embeds.
const EXTENSIONS: &[&str] = &[
    "rs", "html", "css", "js", "mjs", "json", "sql", "txt", "svg", "png", "jpg", "jpeg", "gif",
    "webp", "ico", "woff", "woff2", "ttf",
];

/// File names that trigger a rebuild whatever their extension.
const NAMES: &[&str] = &["Cargo.toml", "Cargo.lock", "rust-toolchain.toml"];

/// Whether a file (`/`-separated, relative to the root) triggers a rebuild.
fn watched(relative: &str) -> bool {
    let name = relative.rsplit('/').next().unwrap_or(relative);
    if name.ends_with('~') || name.starts_with(".#") || name.starts_with('.') {
        return false;
    }
    if NAMES.contains(&name) {
        return true;
    }
    Path::new(name)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| EXTENSIONS.contains(&extension))
}

/// Scans the tree under `root`.
pub(crate) fn scan(root: &Path) -> Result<Snapshot, CliError> {
    let mut files = Snapshot::new();
    let mut pending: Vec<(PathBuf, String)> = vec![(root.to_path_buf(), String::new())];
    while let Some((directory, prefix)) = pending.pop() {
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            // A directory that vanished between listing and reading.
            Err(error) if directory != root && error.kind() == std::io::ErrorKind::NotFound => {
                continue;
            }
            Err(source) => {
                return Err(CliError::ReadDirectory {
                    path: directory,
                    source,
                });
            }
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let relative = format!("{prefix}{name}");
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                let ignored = name.starts_with('.')
                    || IGNORED_DIRECTORIES.contains(&name.as_str())
                    || (prefix.is_empty() && IGNORED_ROOT_DIRECTORIES.contains(&name.as_str()));
                if !ignored {
                    pending.push((entry.path(), format!("{relative}/")));
                }
            } else if kind.is_file() && watched(&relative) {
                let Ok(metadata) = entry.metadata() else {
                    continue;
                };
                if files.len() >= MAX_FILES {
                    return Err(CliError::TooManyFiles {
                        root: root.to_path_buf(),
                        limit: MAX_FILES,
                    });
                }
                let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                files.insert(relative, (modified, metadata.len()));
            }
        }
    }
    Ok(files)
}

/// The paths that differ between two snapshots, sorted.
fn changed(before: &Snapshot, after: &Snapshot) -> Vec<String> {
    let mut paths: Vec<String> = after
        .iter()
        .filter(|(path, state)| before.get(*path) != Some(state))
        .map(|(path, _)| path.clone())
        .chain(
            before
                .keys()
                .filter(|path| !after.contains_key(*path))
                .cloned(),
        )
        .collect();
    paths.sort();
    paths
}

fn describe(paths: &[String]) -> String {
    let shown = paths
        .iter()
        .take(REPORTED_PATHS)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    match paths.len().saturating_sub(REPORTED_PATHS) {
        0 => shown,
        more => format!("{shown} and {more} more"),
    }
}

/// A started application.
pub(crate) trait Instance {
    /// Whether it exited by itself.
    fn exited(&mut self) -> bool;
    /// Asks it to stop and waits for it, killing it after the grace period.
    fn stop(&mut self);
}

/// How the supervisor builds and starts the application.
pub(crate) trait Host {
    /// Builds the application.
    fn build(&mut self) -> Result<BuildOutcome, CliError>;
    /// Starts a built binary.
    fn start(&mut self, binary: &Path) -> Result<Box<dyn Instance>, CliError>;
}

/// How often the supervisor wakes to look at the shutdown flag.
const SLICE: Duration = Duration::from_millis(25);

/// Sleeps for `duration`, or less when a shutdown is requested.
fn pause(duration: Duration, shutdown: &Shutdown) {
    let end = Instant::now() + duration;
    while !shutdown.requested() {
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return;
        }
        std::thread::sleep(left.min(SLICE));
    }
}

/// What a failed build leaves behind.
fn still_serving(running: bool) -> &'static str {
    if running {
        "the previous build keeps running"
    } else {
        "nothing is running yet, save a fix to build again"
    }
}

/// The supervisor: build, run, and follow changes until a shutdown request.
/// The first build happens at once, so a build error at start does not end
/// the loop: the developer fixes the code and saves.
pub(crate) fn supervise(
    host: &mut dyn Host,
    mut scan_tree: impl FnMut() -> Result<Snapshot, CliError>,
    poll: Duration,
    debounce: Duration,
    shutdown: &Shutdown,
    mut log: impl FnMut(&str),
) -> Result<(), CliError> {
    let mut previous = scan_tree()?;
    let mut running: Option<Box<dyn Instance>> = None;
    let mut build_now = true;
    let mut quiet_since: Option<Instant> = None;
    let mut scan_failing = false;
    while !shutdown.requested() {
        if build_now {
            build_now = false;
            log("building");
            // An error that stopped the build from running at all (cargo
            // missing, no binary of that name) is a failed build like any
            // other: the previous build keeps serving, the error is printed,
            // and the next save builds again.
            match host.build() {
                Ok(BuildOutcome::Interrupted) => break,
                Ok(BuildOutcome::Failed(_)) => {
                    log(&format!(
                        "build failed; {}",
                        still_serving(running.is_some())
                    ));
                }
                Err(error) => log(&format!(
                    "build failed ({error}); {}",
                    still_serving(running.is_some())
                )),
                Ok(BuildOutcome::Built(binary)) => {
                    let after = scan_tree().unwrap_or_else(|_| previous.clone());
                    let moved = changed(&previous, &after);
                    if moved.is_empty() {
                        restart(host, &mut running, &binary, &mut log);
                    } else {
                        log(&format!(
                            "{} changed while building; building again",
                            describe(&moved)
                        ));
                        previous = after;
                        build_now = true;
                        continue;
                    }
                }
            }
        }
        pause(poll, shutdown);
        if running.as_mut().is_some_and(|instance| instance.exited()) {
            log("application exited; waiting for changes");
            running = None;
        }
        match scan_tree() {
            Ok(current) => {
                if scan_failing {
                    log("file watcher is scanning again");
                    scan_failing = false;
                }
                let moved = changed(&previous, &current);
                if !moved.is_empty() {
                    log(&format!("changed: {}", describe(&moved)));
                    quiet_since = Some(Instant::now());
                    previous = current;
                }
            }
            Err(error) => {
                // Keep the last good baseline; say so once per run of failures.
                if !scan_failing {
                    log(&format!(
                        "file watcher cannot scan, changes are not seen until it can: {error}"
                    ));
                }
                scan_failing = true;
            }
        }
        if quiet_since.is_some_and(|since| since.elapsed() >= debounce) {
            quiet_since = None;
            build_now = true;
        }
    }
    if let Some(mut instance) = running.take() {
        instance.stop();
    }
    Ok(())
}

fn restart(
    host: &mut dyn Host,
    running: &mut Option<Box<dyn Instance>>,
    binary: &Path,
    log: &mut impl FnMut(&str),
) {
    if let Some(mut old) = running.take() {
        log("restarting");
        old.stop();
    }
    match host.start(binary) {
        Ok(instance) => {
            *running = Some(instance);
            log("running");
        }
        Err(error) => log(&format!("could not start the application: {error}")),
    }
}

/// The real host: `cargo build` into the package's target directory, and the
/// binary it produced as a child in its own process group.
pub(crate) struct CargoHost {
    pub(crate) root: PathBuf,
    pub(crate) bin: String,
    pub(crate) shutdown: Shutdown,
}

impl Host for CargoHost {
    fn build(&mut self) -> Result<BuildOutcome, CliError> {
        cargo_build(&self.root, Some(&self.bin), false, &self.shutdown)
    }

    fn start(&mut self, binary: &Path) -> Result<Box<dyn Instance>, CliError> {
        let mut command = Command::new(binary);
        command
            .current_dir(&self.root)
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        scrub(&mut command);
        Ok(Box::new(Running {
            child: OwnedChild::spawn(command, true)?,
            shutdown: self.shutdown.clone(),
        }))
    }
}

struct Running {
    child: OwnedChild,
    shutdown: Shutdown,
}

impl Instance for Running {
    fn exited(&mut self) -> bool {
        let exited = !matches!(self.child.try_wait(), Ok(None));
        if exited {
            // It exited by itself: what it started must not outlive it.
            self.child.sweep_group();
        }
        exited
    }

    fn stop(&mut self) {
        // A failure to wait leaves nothing more to do: dropping kills it.
        let _ = self.child.stop(&self.shutdown, STOP_GRACE);
        // The application is gone; so is whatever ignored its SIGTERM.
        self.child.sweep_group();
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::rc::Rc;

    use super::*;

    #[test]
    fn durations_use_go_syntax_within_bounds() {
        assert_eq!(parse_poll_interval("500ms"), Ok(Duration::from_millis(500)));
        assert_eq!(parse_poll_interval("1s"), Ok(Duration::from_secs(1)));
        assert_eq!(parse_poll_interval("1m"), Ok(Duration::from_secs(60)));
        for bad in ["49ms", "61s", "0", "fast", "-1s", ""] {
            assert!(parse_poll_interval(bad).is_err(), "{bad:?}");
        }
        assert_eq!(parse_debounce("0"), Ok(Duration::ZERO));
        assert_eq!(parse_debounce("300ms"), Ok(Duration::from_millis(300)));
        assert!(parse_debounce("2m").is_err());
        assert!(parse_debounce("soon").is_err());
    }

    #[test]
    fn rust_sources_templates_assets_and_migrations_are_watched() {
        for path in [
            "src/main.rs",
            "src/adapters/http/mod.rs",
            "build.rs",
            "Cargo.toml",
            "Cargo.lock",
            "templates/welcome/index.html",
            "assets/application.css",
            "migrations/20261020000000_create_posts.up.sql",
            "assets/logo.png",
        ] {
            assert!(watched(path), "{path}");
        }
        for path in [
            "README.md",
            "src/.main.rs.swp",
            "src/main.rs~",
            "src/.#main.rs",
            ".env",
            "compose.yaml",
            "src/notes.md",
            "Dockerfile",
        ] {
            assert!(!watched(path), "{path}");
        }
    }

    #[test]
    fn scan_skips_build_output_hidden_and_non_code_directories() {
        let directory = tempfile::tempdir().expect("tempdir");
        let root = directory.path();
        for (path, content) in [
            ("Cargo.toml", "x"),
            ("src/main.rs", "fn main() {}"),
            ("src/deep/er/mod.rs", ""),
            ("templates/a.html", ""),
            ("target/debug/build.rs", ""),
            (".git/hooks/pre-commit.rs", ""),
            ("tests/http.rs", ""),
            ("storage/data.json", ""),
            ("src/tmp/scratch.rs", ""),
            ("README.md", ""),
        ] {
            let path = root.join(path);
            fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
            fs::write(path, content).expect("write");
        }
        let seen: Vec<String> = scan(root).expect("scan").into_keys().collect();
        assert_eq!(
            seen,
            [
                "Cargo.toml",
                "src/deep/er/mod.rs",
                "src/main.rs",
                "templates/a.html"
            ]
        );
    }

    #[test]
    fn changes_are_added_removed_and_modified_files() {
        let at = |seconds| SystemTime::UNIX_EPOCH + Duration::from_secs(seconds);
        let mut before = Snapshot::new();
        before.insert("a.rs".to_owned(), (at(1), 1));
        before.insert("b.rs".to_owned(), (at(1), 1));
        before.insert("c.rs".to_owned(), (at(1), 1));
        let mut after = before.clone();
        after.remove("a.rs");
        after.insert("b.rs".to_owned(), (at(2), 1));
        after.insert("c.rs".to_owned(), (at(1), 2));
        after.insert("d.rs".to_owned(), (at(3), 9));
        assert_eq!(changed(&before, &after), ["a.rs", "b.rs", "c.rs", "d.rs"]);
        assert!(changed(&before, &before).is_empty());
        assert_eq!(
            describe(&(1..=8).map(|n| format!("f{n}.rs")).collect::<Vec<_>>()),
            "f1.rs, f2.rs, f3.rs, f4.rs, f5.rs and 3 more"
        );
    }

    /// An edit of the fake tree, applied at a given scan.
    type Edit = Box<dyn Fn(&mut Snapshot)>;

    /// What the fake host did, in order.
    type Events = Rc<RefCell<Vec<String>>>;

    struct FakeInstance {
        name: String,
        events: Events,
    }

    impl Instance for FakeInstance {
        fn exited(&mut self) -> bool {
            false
        }
        fn stop(&mut self) {
            self.events.borrow_mut().push(format!("stop {}", self.name));
        }
    }

    struct FakeHost {
        events: Events,
        builds: VecDeque<Result<BuildOutcome, CliError>>,
        shutdown: Shutdown,
        /// Stop supervising after this many builds.
        stop_after: usize,
        built: usize,
        /// Snapshots handed out by the scan closure during a build.
        on_build: Option<Box<dyn FnMut()>>,
    }

    impl Host for FakeHost {
        fn build(&mut self) -> Result<BuildOutcome, CliError> {
            self.built += 1;
            self.events
                .borrow_mut()
                .push(format!("build {}", self.built));
            if let Some(hook) = self.on_build.as_mut() {
                hook();
            }
            if self.built >= self.stop_after {
                // The last build ends the test once it has been handled.
                self.shutdown.request();
            }
            self.builds
                .pop_front()
                .unwrap_or_else(|| Ok(BuildOutcome::Built(PathBuf::from("/bin/app"))))
        }

        fn start(&mut self, binary: &Path) -> Result<Box<dyn Instance>, CliError> {
            let name = format!("{}#{}", binary.display(), self.built);
            self.events.borrow_mut().push(format!("start {name}"));
            Ok(Box::new(FakeInstance {
                name,
                events: Rc::clone(&self.events),
            }))
        }
    }

    fn drive(
        builds: Vec<BuildOutcome>,
        stop_after: usize,
        on_build: Option<Box<dyn FnMut()>>,
        files: Rc<RefCell<Snapshot>>,
        edit_after_scans: Option<(usize, Edit)>,
    ) -> (Vec<String>, Vec<String>) {
        drive_results(
            builds.into_iter().map(Ok).collect(),
            stop_after,
            on_build,
            files,
            edit_after_scans,
        )
    }

    fn drive_results(
        builds: Vec<Result<BuildOutcome, CliError>>,
        stop_after: usize,
        on_build: Option<Box<dyn FnMut()>>,
        files: Rc<RefCell<Snapshot>>,
        edit_after_scans: Option<(usize, Edit)>,
    ) -> (Vec<String>, Vec<String>) {
        let events: Events = Rc::default();
        let shutdown = Shutdown::new();
        let mut host = FakeHost {
            events: Rc::clone(&events),
            builds: builds.into(),
            shutdown: shutdown.clone(),
            stop_after,
            built: 0,
            on_build,
        };
        let logs = Rc::new(RefCell::new(Vec::new()));
        let log_sink = Rc::clone(&logs);
        let scans = Rc::new(RefCell::new(0usize));
        let scanned = Rc::clone(&files);
        supervise(
            &mut host,
            move || {
                let mut count = scans.borrow_mut();
                *count += 1;
                if let Some((after, edit)) = &edit_after_scans
                    && *count == *after
                {
                    edit(&mut scanned.borrow_mut());
                }
                Ok(scanned.borrow().clone())
            },
            Duration::from_millis(1),
            Duration::ZERO,
            &shutdown,
            move |line| log_sink.borrow_mut().push(line.to_owned()),
        )
        .expect("supervise");
        let events = events.borrow().clone();
        let logs = logs.borrow().clone();
        (events, logs)
    }

    fn edit_main(files: &mut Snapshot) {
        files.insert(
            "src/main.rs".to_owned(),
            (SystemTime::UNIX_EPOCH + Duration::from_secs(99), 7),
        );
    }

    #[test]
    fn it_builds_starts_and_replaces_the_application_after_a_change() {
        let files = Rc::new(RefCell::new(Snapshot::new()));
        let (events, logs) = drive(Vec::new(), 2, None, files, Some((3, Box::new(edit_main))));
        assert_eq!(
            events,
            [
                "build 1",
                "start /bin/app#1",
                "build 2",
                "stop /bin/app#1",
                "start /bin/app#2",
                "stop /bin/app#2",
            ],
            "the old one stops only after the new build succeeded; shutdown stops the last"
        );
        assert!(
            logs.iter().any(|line| line == "changed: src/main.rs"),
            "{logs:?}"
        );
        assert!(logs.iter().any(|line| line == "restarting"), "{logs:?}");
    }

    #[test]
    fn a_failed_build_keeps_the_running_application() {
        let files = Rc::new(RefCell::new(Snapshot::new()));
        let (events, logs) = drive(
            vec![
                BuildOutcome::Built(PathBuf::from("/bin/app")),
                BuildOutcome::Failed(Some(101)),
            ],
            2,
            None,
            files,
            Some((3, Box::new(edit_main))),
        );
        assert_eq!(
            events,
            ["build 1", "start /bin/app#1", "build 2", "stop /bin/app#1"],
            "no second start, and the first one ran until shutdown"
        );
        assert!(
            logs.iter()
                .any(|line| line == "build failed; the previous build keeps running"),
            "{logs:?}"
        );
    }

    #[test]
    fn a_build_that_could_not_run_keeps_the_running_application_and_the_loop() {
        let files = Rc::new(RefCell::new(Snapshot::new()));
        let (events, logs) = drive_results(
            vec![
                Ok(BuildOutcome::Built(PathBuf::from("/bin/app"))),
                Err(CliError::NoBinary {
                    name: Some("shop".to_owned()),
                }),
            ],
            2,
            None,
            files,
            Some((3, Box::new(edit_main))),
        );
        // The error neither ends the supervisor nor kills the application:
        // the shutdown stops it gracefully, like after a failed build.
        assert_eq!(
            events,
            ["build 1", "start /bin/app#1", "build 2", "stop /bin/app#1"]
        );
        let failure = logs
            .iter()
            .find(|line| line.starts_with("build failed ("))
            .unwrap_or_else(|| panic!("{logs:?}"));
        assert_eq!(
            failure,
            "build failed (`cargo build` produced no binary named `shop`); \
             the previous build keeps running"
        );
    }

    #[test]
    fn a_first_build_that_could_not_run_waits_for_a_fix_too() {
        let files = Rc::new(RefCell::new(Snapshot::new()));
        let (events, logs) = drive_results(
            vec![Err(CliError::NoBinary { name: None })],
            1,
            None,
            files,
            None,
        );
        assert_eq!(events, ["build 1"]);
        assert!(
            logs.iter()
                .any(|line| line.contains("nothing is running yet")),
            "{logs:?}"
        );
    }

    #[test]
    fn a_failed_first_build_waits_for_a_fix() {
        let files = Rc::new(RefCell::new(Snapshot::new()));
        let (events, logs) = drive(vec![BuildOutcome::Failed(Some(101))], 1, None, files, None);
        assert_eq!(events, ["build 1"]);
        assert!(
            logs.iter()
                .any(|line| line.contains("nothing is running yet")),
            "{logs:?}"
        );
    }

    #[test]
    fn a_build_made_stale_by_an_edit_is_discarded() {
        let files = Rc::new(RefCell::new(Snapshot::new()));
        let during_build = Rc::clone(&files);
        let (events, logs) = drive(
            Vec::new(),
            2,
            Some(Box::new(move || {
                // The first build runs while the developer saves a file.
                let mut tree = during_build.borrow_mut();
                if tree.is_empty() {
                    edit_main(&mut tree);
                }
            })),
            files,
            None,
        );
        assert_eq!(
            events,
            ["build 1", "build 2", "start /bin/app#2", "stop /bin/app#2"],
            "the stale build 1 is never started"
        );
        assert!(
            logs.iter()
                .any(|line| line.contains("changed while building")),
            "{logs:?}"
        );
    }

    /// An application that starts a helper and exits on its own: the helper
    /// must not outlive it (Linux, for /proc).
    #[cfg(target_os = "linux")]
    #[test]
    fn the_helpers_of_an_application_that_exited_by_itself_are_killed() {
        let directory = tempfile::tempdir().expect("tempdir");
        let pid_file = directory.path().join("pid");
        let mut command = Command::new("sh");
        command
            .args(["-c", "sleep 60 & echo $! > \"$1\"", "sh"])
            .arg(&pid_file)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut running = Running {
            child: OwnedChild::spawn(command, true).expect("spawn"),
            shutdown: Shutdown::new(),
        };
        let began = Instant::now();
        while !running.exited() {
            assert!(began.elapsed() < Duration::from_secs(5), "never exited");
            std::thread::sleep(Duration::from_millis(10));
        }
        let helper: u32 = fs::read_to_string(&pid_file)
            .expect("pid file")
            .trim()
            .parse()
            .expect("pid");
        let alive = |pid: u32| {
            fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .and_then(|stat| {
                    stat.rsplit_once(") ")
                        .and_then(|(_, rest)| rest.chars().next())
                })
                .is_some_and(|state| state != 'Z')
        };
        let waited = Instant::now();
        while alive(helper) && waited.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!alive(helper), "the helper outlived the application");
    }

    #[test]
    fn an_interrupted_build_ends_the_loop_without_starting_anything() {
        let files = Rc::new(RefCell::new(Snapshot::new()));
        let (events, _) = drive(vec![BuildOutcome::Interrupted], 9, None, files, None);
        assert_eq!(events, ["build 1"]);
    }
}
