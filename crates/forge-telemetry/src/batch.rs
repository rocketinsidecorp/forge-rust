//! A bounded queue drained by one task owned by the [`Telemetry`](crate::Telemetry).
//!
//! Producers (request handlers, the log layer) never wait: when the queue is
//! full the item is dropped and counted. The worker exports batches of at
//! most `max_batch` every `interval`, or as soon as a full batch is waiting.
//! On shutdown it exports what is queued within the caller's deadline; what
//! is left when the deadline passes is dropped and counted.

use std::{
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use tokio::{
    sync::{Notify, mpsc, oneshot},
    task::JoinHandle,
    time::Instant,
};

pub(crate) type ExportFuture = Pin<Box<dyn Future<Output = Result<(), String>> + Send>>;
pub(crate) type ExportFn<T> = Arc<dyn Fn(Vec<T>) -> ExportFuture + Send + Sync>;
pub(crate) type FailureFn = Arc<dyn Fn(usize, &str) + Send + Sync>;

/// Export counters of one signal.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Stats {
    /// Items dropped: queue full, export failed, after shutdown or past the
    /// shutdown deadline.
    pub dropped: u64,
    /// Items exported.
    pub exported: u64,
    /// Items in failed exports (included in `dropped`).
    pub failed: u64,
}

pub(crate) struct Batcher<T> {
    shared: Arc<Shared<T>>,
    worker: Mutex<Option<Worker<T>>>,
    flushes: mpsc::Sender<oneshot::Sender<()>>,
    flush_receiver: Mutex<Option<mpsc::Receiver<oneshot::Sender<()>>>>,
    running: Mutex<Option<JoinHandle<()>>>,
}

struct Shared<T> {
    queue: Mutex<Vec<T>>,
    max_queue: usize,
    max_batch: usize,
    closed: AtomicBool,
    full: Notify,
    stop: Notify,
    stopping: AtomicBool,
    dropped: AtomicU64,
    exported: AtomicU64,
    failed: AtomicU64,
}

struct Worker<T> {
    export: ExportFn<T>,
    on_failure: FailureFn,
    interval: Duration,
}

impl<T: Send + 'static> Batcher<T> {
    pub(crate) fn new(
        max_queue: usize,
        max_batch: usize,
        interval: Duration,
        export: ExportFn<T>,
        on_failure: FailureFn,
    ) -> Self {
        let (flushes, flush_receiver) = mpsc::channel(4);
        Self {
            shared: Arc::new(Shared {
                queue: Mutex::new(Vec::new()),
                max_queue,
                max_batch: max_batch.clamp(1, max_queue.max(1)),
                closed: AtomicBool::new(false),
                full: Notify::new(),
                stop: Notify::new(),
                stopping: AtomicBool::new(false),
                dropped: AtomicU64::new(0),
                exported: AtomicU64::new(0),
                failed: AtomicU64::new(0),
            }),
            worker: Mutex::new(Some(Worker {
                export,
                on_failure,
                interval,
            })),
            flushes,
            flush_receiver: Mutex::new(Some(flush_receiver)),
            running: Mutex::new(None),
        }
    }

    /// Queues `item` without waiting; `false` when it was dropped.
    pub(crate) fn enqueue(&self, item: T) -> bool {
        let shared = &self.shared;
        let ready = {
            let mut queue = shared.queue.lock().unwrap_or_else(PoisonError::into_inner);
            if shared.closed.load(Ordering::Acquire) || queue.len() >= shared.max_queue {
                drop(queue);
                shared.dropped.fetch_add(1, Ordering::Relaxed);
                return false;
            }
            queue.push(item);
            queue.len() >= shared.max_batch
        };
        if ready {
            shared.full.notify_one();
        }
        true
    }

    pub(crate) fn stats(&self) -> Stats {
        let failed = self.shared.failed.load(Ordering::Relaxed);
        Stats {
            dropped: self.shared.dropped.load(Ordering::Relaxed) + failed,
            exported: self.shared.exported.load(Ordering::Relaxed),
            failed,
        }
    }

    /// Spawns the worker on the current Tokio runtime; a second call does
    /// nothing. The handle is kept and joined by [`Batcher::shutdown`].
    pub(crate) fn start(&self) {
        let worker = self
            .worker
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let flushes = self
            .flush_receiver
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let (Some(worker), Some(flushes)) = (worker, flushes) else {
            return;
        };
        let handle = tokio::spawn(run(Arc::clone(&self.shared), worker, flushes));
        *self.running.lock().unwrap_or_else(PoisonError::into_inner) = Some(handle);
    }

    /// Exports what is queued now, waiting at most `timeout`.
    pub(crate) async fn flush(&self, timeout: Duration) -> bool {
        let (reply, done) = oneshot::channel();
        tokio::time::timeout(timeout, async {
            self.flushes.send(reply).await.is_ok() && done.await.is_ok()
        })
        .await
        .unwrap_or(false)
    }

    /// Stops accepting items, exports what is queued and stops the worker,
    /// all before `deadline`; at the deadline the export in progress is
    /// cancelled and what is left is dropped and counted. `true` when the
    /// worker finished in time.
    pub(crate) async fn shutdown(&self, deadline: Instant) -> bool {
        self.shared.closed.store(true, Ordering::Release);
        self.shared.stopping.store(true, Ordering::Release);
        self.shared.stop.notify_one();
        let handle = self
            .running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let finished = match handle {
            Some(mut handle) => match tokio::time::timeout_at(deadline, &mut handle).await {
                Ok(_) => true,
                Err(_elapsed) => {
                    handle.abort();
                    let _ = handle.await;
                    false
                }
            },
            None => true,
        };
        self.shared.drop_remaining();
        finished
    }
}

impl<T> Shared<T> {
    fn take_batch(&self, full_only: bool) -> Option<Vec<T>> {
        let mut queue = self.queue.lock().unwrap_or_else(PoisonError::into_inner);
        let n = queue.len().min(self.max_batch);
        if n == 0 || (full_only && n < self.max_batch) {
            return None;
        }
        Some(queue.drain(..n).collect())
    }

    fn drop_remaining(&self) {
        let n = {
            let mut queue = self.queue.lock().unwrap_or_else(PoisonError::into_inner);
            let n = queue.len();
            queue.clear();
            n
        };
        self.dropped.fetch_add(n as u64, Ordering::Relaxed);
    }
}

async fn run<T: Send + 'static>(
    shared: Arc<Shared<T>>,
    worker: Worker<T>,
    mut flushes: mpsc::Receiver<oneshot::Sender<()>>,
) {
    let mut ticker = tokio::time::interval(worker.interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ticker.tick().await;
    loop {
        if shared.stopping.load(Ordering::Acquire) {
            drain(&shared, &worker, false).await;
            return;
        }
        tokio::select! {
            () = shared.stop.notified() => {}
            _ = ticker.tick() => drain(&shared, &worker, false).await,
            () = shared.full.notified() => drain(&shared, &worker, true).await,
            Some(reply) = flushes.recv() => {
                drain(&shared, &worker, false).await;
                let _ = reply.send(());
            }
        }
    }
}

async fn drain<T>(shared: &Shared<T>, worker: &Worker<T>, full_only: bool) {
    while let Some(batch) = shared.take_batch(full_only) {
        let n = batch.len();
        match (worker.export)(batch).await {
            Ok(()) => {
                shared.exported.fetch_add(n as u64, Ordering::Relaxed);
            }
            Err(error) => {
                shared.failed.fetch_add(n as u64, Ordering::Relaxed);
                (worker.on_failure)(n, &error);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;

    fn counting(exported: Arc<AtomicUsize>, delay: Duration) -> ExportFn<u32> {
        Arc::new(move |batch: Vec<u32>| {
            let exported = Arc::clone(&exported);
            Box::pin(async move {
                // real-clock: simulated export latency of the exporter under test
                tokio::time::sleep(delay).await;
                exported.fetch_add(batch.len(), Ordering::SeqCst);
                Ok(())
            })
        })
    }

    #[tokio::test]
    async fn full_queue_drops_and_counts_without_waiting() {
        let exported = Arc::new(AtomicUsize::new(0));
        let batcher = Batcher::new(
            4,
            2,
            Duration::from_secs(3600),
            counting(Arc::clone(&exported), Duration::ZERO),
            Arc::new(|_, _| {}),
        );
        // Not started: nothing drains, so the bound is reached.
        let started = std::time::Instant::now();
        let accepted = (0..10).filter(|item| batcher.enqueue(*item)).count();
        assert!(started.elapsed() < Duration::from_millis(50));
        assert_eq!(accepted, 4);
        assert_eq!(batcher.stats().dropped, 6);
    }

    #[tokio::test]
    async fn flush_exports_and_shutdown_drains() {
        let exported = Arc::new(AtomicUsize::new(0));
        let batcher = Batcher::new(
            100,
            10,
            Duration::from_secs(3600),
            counting(Arc::clone(&exported), Duration::ZERO),
            Arc::new(|_, _| {}),
        );
        batcher.start();
        for item in 0..5 {
            batcher.enqueue(item);
        }
        assert!(batcher.flush(Duration::from_secs(5)).await);
        assert_eq!(exported.load(Ordering::SeqCst), 5);
        for item in 0..25 {
            batcher.enqueue(item);
        }
        assert!(
            batcher
                .shutdown(Instant::now() + Duration::from_secs(5))
                .await
        );
        assert_eq!(exported.load(Ordering::SeqCst), 30);
        assert!(!batcher.enqueue(1), "closed after shutdown");
        assert_eq!(batcher.stats().exported, 30);
    }

    #[tokio::test]
    async fn shutdown_is_bounded_by_its_deadline() {
        let exported = Arc::new(AtomicUsize::new(0));
        let batcher = Batcher::new(
            100,
            5,
            Duration::from_secs(3600),
            counting(Arc::clone(&exported), Duration::from_secs(60)),
            Arc::new(|_, _| {}),
        );
        batcher.start();
        for item in 0..20 {
            batcher.enqueue(item);
        }
        let started = std::time::Instant::now();
        let finished = batcher
            .shutdown(Instant::now() + Duration::from_millis(200))
            .await;
        assert!(!finished);
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(exported.load(Ordering::SeqCst), 0);
        // The batch in flight was cancelled; the queue was dropped and counted.
        assert!(batcher.stats().dropped >= 15, "{:?}", batcher.stats());
    }

    #[tokio::test]
    async fn failures_are_reported_and_counted() {
        let failures = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&failures);
        let batcher: Batcher<u32> = Batcher::new(
            10,
            10,
            Duration::from_secs(3600),
            Arc::new(|_| Box::pin(async { Err("down".to_owned()) })),
            Arc::new(move |n, error| {
                assert_eq!(error, "down");
                seen.fetch_add(n, Ordering::SeqCst);
            }),
        );
        batcher.start();
        batcher.enqueue(1);
        batcher.enqueue(2);
        assert!(batcher.flush(Duration::from_secs(5)).await);
        assert_eq!(failures.load(Ordering::SeqCst), 2);
        assert_eq!(batcher.stats().failed, 2);
        assert_eq!(batcher.stats().dropped, 2);
    }
}
