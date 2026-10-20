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
        let shutdown = Shutdown::new();
        let (stop, stopped) = oneshot::channel();
        let (ready, registered) = std::sync::mpsc::channel();
        let flag = shutdown.clone();
        let thread = std::thread::Builder::new()
            .name("forge-signals".to_owned())
            .spawn(move || listen(&flag, stopped, &ready))
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

fn listen(
    shutdown: &Shutdown,
    mut stopped: oneshot::Receiver<()>,
    ready: &std::sync::mpsc::Sender<io::Result<()>>,
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

            let registered = signal(SignalKind::terminate())
                .and_then(|terminate| signal(SignalKind::interrupt()).map(|int| (terminate, int)));
            let (mut terminate, mut interrupt) = match registered {
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
                    _ = interrupt.recv() => shutdown.request(),
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
                        if result.is_ok() {
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
        Ok(Self { child, own_group })
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
        let mut command = Command::new("sh");
        command.args(["-c", "trap '' TERM; while :; do sleep 1; done"]);
        let mut child = OwnedChild::spawn(command, true).expect("spawn");
        // Give the shell time to install the trap.
        std::thread::sleep(Duration::from_millis(200));
        let (status, interrupted) = child
            .wait(&shutdown, Duration::from_millis(300))
            .expect("wait");
        assert!(interrupted && !status.success());
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
