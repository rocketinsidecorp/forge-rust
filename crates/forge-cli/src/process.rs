//! Process ownership: a shutdown request set by SIGINT/SIGTERM, and child
//! processes that are always waited for or killed, never left behind.
//!
//! The CLI owns every process it starts for the lifecycle commands and
//! `forge dev`. A signal handler thread (one, stopped and joined when
//! [`Signals`] drops) only sets a flag; the thread that owns a child polls
//! the flag, asks the child to stop (SIGTERM, to its whole process group when
//! it has one), waits a bounded grace period and then kills it.

use std::io;
use std::process::{Child, Command, ExitStatus};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tokio::sync::oneshot;

use crate::CliError;

/// How often a waiting thread looks at its child and at the shutdown flag.
const POLL: Duration = Duration::from_millis(25);

/// How long an application gets to drain after it was asked to stop; it
/// exceeds the framework's default shutdown grace of 15 seconds.
pub(crate) const STOP_GRACE: Duration = Duration::from_secs(35);

/// A request to stop, shared between the signal thread and the owners of
/// child processes. The first signal asks politely; a second one is forceful.
#[derive(Clone, Debug, Default)]
pub(crate) struct Shutdown {
    signals: Arc<AtomicU8>,
}

impl Shutdown {
    /// A flag nobody has set.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Records one request (a signal, or a test).
    pub(crate) fn request(&self) {
        // Saturating: more than two requests mean the same as two.
        let _ = self
            .signals
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |count| {
                Some(count.saturating_add(1))
            });
    }

    /// Whether anyone asked to stop.
    pub(crate) fn requested(&self) -> bool {
        self.signals.load(Ordering::SeqCst) >= 1
    }

    /// Whether the request was repeated: stop waiting for a graceful exit.
    pub(crate) fn forced(&self) -> bool {
        self.signals.load(Ordering::SeqCst) >= 2
    }
}

/// The installed signal handlers. Dropping it stops and joins the thread.
pub(crate) struct Signals {
    shutdown: Shutdown,
    stop: Option<oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Signals {
    /// Installs the handlers for SIGINT and SIGTERM (Ctrl-C elsewhere) and
    /// returns once they are registered.
    pub(crate) fn install() -> Result<Self, CliError> {
        Self::install_for(Shutdown::new())
    }

    /// [`install`](Self::install), setting an existing flag.
    pub(crate) fn install_for(shutdown: Shutdown) -> Result<Self, CliError> {
        Self::install_mode(shutdown, Mode::Stop)
    }

    /// Handlers for an interactive client (`forge db console`): hang-up, quit
    /// and terminate ask to stop, while Ctrl-C is swallowed. The terminal
    /// delivers it to the client too (same foreground process group), which
    /// uses it to cancel a query; ending the CLI there would end the session
    /// and skip the clean-up of the credential file.
    pub(crate) fn install_for_client(shutdown: Shutdown) -> Result<Self, CliError> {
        Self::install_mode(shutdown, Mode::Client)
    }

    fn install_mode(shutdown: Shutdown, mode: Mode) -> Result<Self, CliError> {
        let (stop, stopped) = oneshot::channel();
        let (ready, registered) = std::sync::mpsc::channel();
        let flag = shutdown.clone();
        let thread = std::thread::Builder::new()
            .name("forge-signals".to_owned())
            .spawn(move || listen(&flag, stopped, &ready, mode))
            .map_err(CliError::Signals)?;
        match registered.recv() {
            Ok(Ok(())) => Ok(Self {
                shutdown,
                stop: Some(stop),
                thread: Some(thread),
            }),
            Ok(Err(source)) => {
                let _ = thread.join();
                Err(CliError::Signals(source))
            }
            Err(_) => {
                let _ = thread.join();
                Err(CliError::Signals(io::Error::other(
                    "the signal thread stopped before it was ready",
                )))
            }
        }
    }

    /// The flag the handlers set.
    pub(crate) fn shutdown(&self) -> Shutdown {
        self.shutdown.clone()
    }
}

impl Drop for Signals {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            // The thread may already be gone; nothing is left to stop then.
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// What the signal thread does with Ctrl-C.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Mode {
    /// SIGINT and SIGTERM ask to stop.
    Stop,
    /// SIGTERM, SIGHUP and SIGQUIT ask to stop; SIGINT is swallowed.
    Client,
}

/// The next delivery of an optional signal; never, when it is not registered.
#[cfg(unix)]
async fn received(stream: &mut Option<tokio::signal::unix::Signal>) {
    match stream {
        Some(stream) => {
            stream.recv().await;
        }
        None => std::future::pending().await,
    }
}

fn listen(
    shutdown: &Shutdown,
    mut stopped: oneshot::Receiver<()>,
    ready: &std::sync::mpsc::Sender<io::Result<()>>,
    mode: Mode,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    runtime.block_on(async {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};

            let registered = (|| {
                let terminate = signal(SignalKind::terminate())?;
                let interrupt = signal(SignalKind::interrupt())?;
                // Hang-up and quit keep their default meaning (they end the
                // CLI) unless a client runs and must be told.
                let (hangup, quit) = if mode == Mode::Client {
                    (
                        Some(signal(SignalKind::hangup())?),
                        Some(signal(SignalKind::quit())?),
                    )
                } else {
                    (None, None)
                };
                Ok::<_, io::Error>((terminate, interrupt, hangup, quit))
            })();
            let (mut terminate, mut interrupt, mut hangup, mut quit) = match registered {
                Ok(streams) => streams,
                Err(error) => {
                    let _ = ready.send(Err(error));
                    return;
                }
            };
            let _ = ready.send(Ok(()));
            loop {
                tokio::select! {
                    _ = terminate.recv() => shutdown.request(),
                    _ = interrupt.recv() => {
                        if mode == Mode::Stop {
                            shutdown.request();
                        }
                    }
                    () = received(&mut hangup) => shutdown.request(),
                    () = received(&mut quit) => shutdown.request(),
                    _ = &mut stopped => return,
                }
            }
        }
        #[cfg(not(unix))]
        {
            let _ = ready.send(Ok(()));
            loop {
                tokio::select! {
                    result = tokio::signal::ctrl_c() => {
                        if result.is_ok() && mode == Mode::Stop {
                            shutdown.request();
                        }
                    }
                    _ = &mut stopped => return,
                }
            }
        }
    });
}

/// A child process that cannot outlive its owner: dropping it kills and reaps
/// it when it is still running.
pub(crate) struct OwnedChild {
    child: Child,
    own_group: bool,
    /// Whether the group was already swept after the child exited: its id
    /// may be reused by then, so it is signalled once, right after the exit.
    group_swept: bool,
}

impl OwnedChild {
    /// Starts `command`. With `own_group` (Unix) the child leads a new process
    /// group, so a stop reaches the helpers it started too, and a terminal's
    /// Ctrl-C reaches only the CLI, which then stops the group itself. A
    /// child that reads the terminal (a password prompt) must not have one.
    pub(crate) fn spawn(mut command: Command, own_group: bool) -> Result<Self, CliError> {
        #[cfg(unix)]
        if own_group {
            use std::os::unix::process::CommandExt as _;

            command.process_group(0);
        }
        let program = command.get_program().to_string_lossy().into_owned();
        let child = command
            .spawn()
            .map_err(|source| CliError::SpawnProcess { program, source })?;
        Ok(Self {
            child,
            own_group,
            group_swept: false,
        })
    }

    /// Takes the child's piped standard input.
    pub(crate) fn take_stdin(&mut self) -> Option<std::process::ChildStdin> {
        self.child.stdin.take()
    }

    /// Takes the child's piped standard output.
    pub(crate) fn take_stdout(&mut self) -> Option<std::process::ChildStdout> {
        self.child.stdout.take()
    }

    /// The exit status when the child has exited, without waiting.
    pub(crate) fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    /// Asks the child (its process group, when it has one) to stop.
    pub(crate) fn terminate(&self) {
        #[cfg(unix)]
        self.signal(rustix::process::Signal::TERM);
        #[cfg(not(unix))]
        let _ = &self.child;
    }

    #[cfg(unix)]
    fn signal(&self, signal: rustix::process::Signal) {
        use rustix::process::{Pid, kill_process, kill_process_group};

        let Some(pid) = i32::try_from(self.child.id()).ok().and_then(Pid::from_raw) else {
            return;
        };
        // The child may have exited already; there is nothing to signal then.
        let _ = if self.own_group {
            kill_process_group(pid, signal)
        } else {
            kill_process(pid, signal)
        };
    }

    /// Kills what an exited child left running in its process group (workers
    /// and helpers it started), so a restart never finds orphans holding its
    /// port. Does nothing for a child that is still running, one without a
    /// group of its own, or a group already swept. Call it right after the
    /// exit was seen: the group id is the child's pid, which the system may
    /// reuse once the group is empty.
    pub(crate) fn sweep_group(&mut self) {
        if !self.own_group || self.group_swept || !matches!(self.child.try_wait(), Ok(Some(_))) {
            return;
        }
        self.group_swept = true;
        #[cfg(unix)]
        self.signal(rustix::process::Signal::KILL);
    }

    /// Kills the child and waits for it.
    pub(crate) fn kill(&mut self) {
        #[cfg(unix)]
        self.signal(rustix::process::Signal::KILL);
        // Also covers platforms without process groups. An error means the
        // child is already gone.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    /// Asks the child to stop and waits for it: it gets `grace` to exit, and
    /// is killed after that or as soon as the shutdown request is repeated.
    pub(crate) fn stop(&mut self, shutdown: &Shutdown, grace: Duration) -> io::Result<ExitStatus> {
        self.terminate();
        let began = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait()? {
                return Ok(status);
            }
            if shutdown.forced() || began.elapsed() >= grace {
                self.kill();
                return self.child.wait();
            }
            std::thread::sleep(POLL);
        }
    }

    /// Waits for the child. When a shutdown is requested meanwhile it is
    /// stopped as [`stop`](Self::stop) does. Returns the status, and whether a
    /// shutdown ended the wait.
    pub(crate) fn wait(
        &mut self,
        shutdown: &Shutdown,
        grace: Duration,
    ) -> io::Result<(ExitStatus, bool)> {
        loop {
            if let Some(status) = self.child.try_wait()? {
                return Ok((status, false));
            }
            if shutdown.requested() {
                return self.stop(shutdown, grace).map(|status| (status, true));
            }
            std::thread::sleep(POLL);
        }
    }
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            self.kill();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_request_is_forceful() {
        let shutdown = Shutdown::new();
        assert!(!shutdown.requested() && !shutdown.forced());
        shutdown.request();
        assert!(shutdown.requested() && !shutdown.forced());
        shutdown.clone().request();
        assert!(shutdown.forced());
    }

    #[cfg(unix)]
    fn sleeper(seconds: &str) -> Command {
        let mut command = Command::new("sleep");
        command.arg(seconds);
        command
    }

    #[cfg(unix)]
    #[test]
    fn a_finished_child_reports_its_status() {
        let mut child = OwnedChild::spawn(Command::new("true"), false).expect("spawn");
        let (status, interrupted) = child
            .wait(&Shutdown::new(), Duration::from_secs(5))
            .expect("wait");
        assert!(status.success() && !interrupted);
    }

    #[cfg(unix)]
    #[test]
    fn a_shutdown_stops_the_child_and_its_group() {
        let shutdown = Shutdown::new();
        let mut child = OwnedChild::spawn(sleeper("30"), true).expect("spawn");
        let began = Instant::now();
        let requester = {
            let shutdown = shutdown.clone();
            std::thread::spawn(move || {
                // real-clock: the shutdown request must arrive while the child process is running; the requester is a plain thread
                std::thread::sleep(Duration::from_millis(100));
                shutdown.request();
            })
        };
        let (status, interrupted) = child
            .wait(&shutdown, Duration::from_secs(10))
            .expect("wait");
        requester.join().expect("join");
        assert!(interrupted && !status.success(), "{status:?}");
        assert!(began.elapsed() < Duration::from_secs(10));
    }

    #[cfg(unix)]
    #[test]
    fn a_child_that_ignores_the_request_is_killed_after_the_grace() {
        let shutdown = Shutdown::new();
        shutdown.request();
        let directory = tempfile::tempdir().expect("tempdir");
        let trapped = directory.path().join("trapped");
        let mut command = Command::new("sh");
        command
            .args([
                "-c",
                "trap '' TERM; : > \"$1\"; while :; do sleep 1; done",
                "sh",
            ])
            .arg(&trapped);
        let mut child = OwnedChild::spawn(command, true).expect("spawn");
        // The shell creates the file once the trap is installed: wait for it
        // instead of for a duration, or a slow start would be killed by the
        // request itself and the grace never exercised.
        let waited = Instant::now();
        while !trapped.exists() {
            assert!(
                waited.elapsed() < Duration::from_secs(20),
                "the shell never installed its trap"
            );
            // real-clock: polls a child process (an OS resource a fake clock cannot advance) until a deadline (the shell installs its trap)
            std::thread::sleep(Duration::from_millis(5));
        }
        let (status, interrupted) = child
            .wait(&shutdown, Duration::from_millis(300))
            .expect("wait");
        assert!(interrupted && !status.success());
    }

    /// Whether a process exists and is not a zombie (Linux).
    #[cfg(target_os = "linux")]
    fn running(pid: u32) -> bool {
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|stat| {
                stat.rsplit_once(") ")
                    .and_then(|(_, rest)| rest.chars().next())
            })
            .is_some_and(|state| state != 'Z')
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn descendants_of_a_child_that_exited_on_its_own_are_swept() {
        let directory = tempfile::tempdir().expect("tempdir");
        let pid_file = directory.path().join("pid");
        // The shell leaves a background `sleep` behind and exits at once.
        let mut command = Command::new("sh");
        command
            .args(["-c", "sleep 60 & echo $! > \"$1\"", "sh"])
            .arg(&pid_file)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let mut child = OwnedChild::spawn(command, true).expect("spawn");
        let (status, interrupted) = child
            .wait(&Shutdown::new(), Duration::from_secs(5))
            .expect("wait");
        assert!(status.success() && !interrupted);
        let orphan: u32 = std::fs::read_to_string(&pid_file)
            .expect("pid file")
            .trim()
            .parse()
            .expect("pid");
        assert!(
            running(orphan),
            "the premise: the helper outlives its parent"
        );

        child.sweep_group();

        let began = Instant::now();
        while running(orphan) && began.elapsed() < Duration::from_secs(5) {
            // real-clock: polls a child process (an OS resource a fake clock cannot advance) until a deadline
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!running(orphan), "the helper was killed");
    }

    #[cfg(unix)]
    #[test]
    fn a_running_child_or_one_without_a_group_is_never_swept() {
        let mut child = OwnedChild::spawn(sleeper("30"), true).expect("spawn");
        child.sweep_group();
        assert!(
            matches!(child.try_wait(), Ok(None)),
            "a running child is left alone"
        );
        assert!(!child.group_swept);
        child.kill();

        // Without a group of its own the child's pid is not a group id.
        let mut shared = OwnedChild::spawn(Command::new("true"), false).expect("spawn");
        let _ = shared.wait(&Shutdown::new(), Duration::from_secs(5));
        shared.sweep_group();
        assert!(!shared.group_swept);
    }

    #[cfg(unix)]
    #[test]
    fn dropping_a_running_child_kills_it() {
        let child = OwnedChild::spawn(sleeper("30"), true).expect("spawn");
        let pid = child.child.id();
        drop(child);
        let alive = std::path::Path::new(&format!("/proc/{pid}")).exists();
        // A reaped child has no /proc entry (Linux); elsewhere /proc is absent.
        assert!(!alive);
    }

    #[test]
    fn signal_handlers_install_and_stop_cleanly() {
        let signals = Signals::install().expect("install");
        assert!(!signals.shutdown().requested());
        drop(signals);
    }
}
