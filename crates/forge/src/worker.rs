//! Background workers owned by the application's lifecycle.
//!
//! A composition hook registers a worker with
//! [`Composition::worker`](crate::Composition::worker) or a bounded queue with
//! [`Composition::work_queue`](crate::Composition::work_queue). Forge starts
//! the workers when the server starts, tells them to stop through
//! [`Shutdown`] once the server has drained its connections, waits for them
//! for a bounded time and aborts what is left. There are no detached tasks:
//! every worker is spawned into a set the process lifecycle owns.

use std::{future::Future, pin::Pin, time::Duration};

use thiserror::Error;
use tokio::{
    sync::{mpsc, watch},
    task::JoinSet,
};
use tracing::{debug, warn};

/// Boxed body of a worker.
pub(crate) type WorkerFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

/// A registered worker: its name and its body.
pub(crate) struct Worker {
    pub(crate) name: &'static str,
    pub(crate) run: Box<dyn FnOnce(Shutdown) -> WorkerFuture + Send>,
}

/// How long stopping workers may take before they are aborted.
pub(crate) const STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// Tells a worker that the process is shutting down.
#[derive(Clone, Debug)]
pub struct Shutdown(watch::Receiver<bool>);

impl Shutdown {
    /// Completes once shutdown was requested.
    pub async fn requested(&mut self) {
        // An error means the lifecycle is gone, which also means stop.
        let _ = self.0.wait_for(|stop| *stop).await;
    }

    /// Whether shutdown was requested.
    #[must_use]
    pub fn is_requested(&self) -> bool {
        *self.0.borrow()
    }
}

/// Running workers; [`RunningWorkers::stop`] ends them. Dropping the value
/// aborts them.
#[derive(Debug)]
pub struct RunningWorkers {
    stop: watch::Sender<bool>,
    tasks: JoinSet<&'static str>,
}

impl RunningWorkers {
    /// Spawns `workers` on the current Tokio runtime.
    pub(crate) fn start(workers: Vec<Worker>) -> Self {
        let (stop, receiver) = watch::channel(false);
        let mut tasks = JoinSet::new();
        for worker in workers {
            let body = (worker.run)(Shutdown(receiver.clone()));
            let name = worker.name;
            tasks.spawn(async move {
                body.await;
                name
            });
        }
        Self { stop, tasks }
    }

    /// Requests shutdown and waits up to `timeout` for the workers to
    /// finish; workers still running then are aborted.
    pub async fn stop_within(mut self, timeout: Duration) {
        let _ = self.stop.send(true);
        let finished = tokio::time::timeout(timeout, async {
            while let Some(joined) = self.tasks.join_next().await {
                match joined {
                    Ok(name) => debug!(worker = name, "worker stopped"),
                    Err(error) => warn!(error = %error, "worker failed"),
                }
            }
        })
        .await;
        if finished.is_err() {
            warn!(?timeout, "workers did not stop in time; aborting them");
            self.tasks.abort_all();
        }
    }

    /// Requests shutdown and waits for the workers (at most 5 seconds).
    pub async fn stop(self) {
        self.stop_within(STOP_TIMEOUT).await;
    }
}

/// Sends items to a worker's bounded queue (see
/// [`Composition::work_queue`](crate::Composition::work_queue)). Cloning
/// shares the queue.
#[derive(Debug)]
pub struct WorkQueue<T> {
    name: &'static str,
    sender: mpsc::Sender<T>,
}

impl<T> Clone for WorkQueue<T> {
    fn clone(&self) -> Self {
        Self {
            name: self.name,
            sender: self.sender.clone(),
        }
    }
}

impl<T> WorkQueue<T> {
    /// Queues `item` without waiting. A full or stopped queue refuses it;
    /// the caller decides whether that matters (log and drop, or answer
    /// `503`).
    pub fn submit(&self, item: T) -> Result<(), SubmitError> {
        self.sender.try_send(item).map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => SubmitError::Full { queue: self.name },
            mpsc::error::TrySendError::Closed(_) => SubmitError::Stopped { queue: self.name },
        })
    }
}

/// An item was not queued.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum SubmitError {
    /// The queue is at capacity.
    #[error("work queue {queue} is full")]
    Full {
        /// The queue's name.
        queue: &'static str,
    },
    /// The worker has stopped.
    #[error("work queue {queue} has stopped")]
    Stopped {
        /// The queue's name.
        queue: &'static str,
    },
}

/// A bounded queue and the worker that drains it one item at a time until
/// shutdown; items still queued then are dropped (and counted in the log).
pub(crate) fn queue<T, F, Fut>(
    name: &'static str,
    capacity: usize,
    handle: F,
) -> (WorkQueue<T>, Worker)
where
    T: Send + 'static,
    F: Fn(T) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let (sender, mut receiver) = mpsc::channel::<T>(capacity.max(1));
    let worker = Worker {
        name,
        run: Box::new(move |mut shutdown: Shutdown| {
            Box::pin(async move {
                loop {
                    tokio::select! {
                        biased;
                        () = shutdown.requested() => break,
                        item = receiver.recv() => match item {
                            Some(item) => handle(item).await,
                            None => break,
                        },
                    }
                }
                receiver.close();
                let mut dropped = 0_usize;
                while receiver.try_recv().is_ok() {
                    dropped += 1;
                }
                if dropped > 0 {
                    warn!(queue = name, dropped, "work queue stopped with items left");
                }
            })
        }),
    };
    (WorkQueue { name, sender }, worker)
}

#[cfg(test)]
mod worker_tests;

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use super::*;

    #[tokio::test]
    async fn a_queue_runs_items_refuses_when_full_and_stops() {
        let handled = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&handled);
        let (queue, worker) = queue("test", 2, move |amount: usize| {
            let counter = Arc::clone(&counter);
            async move {
                counter.fetch_add(amount, Ordering::SeqCst);
            }
        });
        // Not started yet: two items fit, the third is refused.
        queue.submit(1).expect("queued");
        queue.submit(2).expect("queued");
        assert_eq!(queue.submit(4), Err(SubmitError::Full { queue: "test" }));

        let running = RunningWorkers::start(vec![worker]);
        for _ in 0..100 {
            if handled.load(Ordering::SeqCst) == 3 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(handled.load(Ordering::SeqCst), 3);
        running.stop().await;
        assert_eq!(queue.submit(8), Err(SubmitError::Stopped { queue: "test" }));
    }

    #[tokio::test]
    async fn a_worker_that_ignores_shutdown_is_aborted() {
        let worker = Worker {
            name: "stubborn",
            run: Box::new(|_| Box::pin(std::future::pending::<()>())),
        };
        let running = RunningWorkers::start(vec![worker]);
        running.stop_within(Duration::from_millis(10)).await;
    }

    #[tokio::test]
    async fn shutdown_is_observable() {
        let (stop, receiver) = watch::channel(false);
        let mut shutdown = Shutdown(receiver);
        assert!(!shutdown.is_requested());
        let _ = stop.send(true);
        shutdown.requested().await;
        assert!(shutdown.is_requested());
    }
}
