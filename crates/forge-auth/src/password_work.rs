//! A process-wide bound on concurrent password hashing.
//!
//! A memory-hard hash holds its whole working set while it runs: Forge's
//! Argon2id policy takes 19 MiB per hash or verification. Tokio's blocking
//! pool allows 512 threads, so running every sign-in attempt on its own
//! blocking thread lets a burst of attempts (unknown logins still hash, to
//! keep timing uniform) reserve gigabytes. The per-account and per-source
//! throttles bound attempts per key, not in total.
//!
//! [`PasswordWorkLimiter`] admits at most a fixed number of hashes at once.
//! An attempt that finds every slot busy waits up to a short deadline and
//! then fails with [`PasswordHashError::Unavailable`] (HTTP `503`), so
//! waiting work cannot pile up either. A slot is held by the blocking work
//! itself, not by the future that awaits it: a request that is cancelled
//! while its hash runs keeps the slot until the hash ends, so cancellation
//! cannot be used to exceed the bound.

use std::{fmt, num::NonZeroUsize, sync::Arc, time::Duration};

use tokio::sync::Semaphore;

use crate::PasswordHashError;

/// Concurrent hashes allowed by [`PasswordWorkLimiter::default`] at most,
/// whatever the CPU count: 16 × 19 MiB is about 300 MiB.
pub const MAX_DEFAULT_PASSWORD_WORK: usize = 16;

/// How long [`PasswordWorkLimiter::default`] lets an attempt wait for a slot.
pub const DEFAULT_PASSWORD_WORK_WAIT: Duration = Duration::from_secs(2);

/// Runs password hashing on Tokio's blocking pool, at most `max_concurrent`
/// at a time. Cheap to clone: clones share the slots.
#[derive(Clone)]
pub struct PasswordWorkLimiter {
    slots: Arc<Semaphore>,
    max_concurrent: usize,
    max_wait: Duration,
}

impl fmt::Debug for PasswordWorkLimiter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PasswordWorkLimiter")
            .field("max_concurrent", &self.max_concurrent)
            .field("max_wait", &self.max_wait)
            .field("available", &self.slots.available_permits())
            .finish()
    }
}

impl Default for PasswordWorkLimiter {
    /// One slot per CPU the process may use (a hash is single-threaded and
    /// CPU-bound, so more would not finish sooner), at most
    /// [`MAX_DEFAULT_PASSWORD_WORK`], waiting up to
    /// [`DEFAULT_PASSWORD_WORK_WAIT`].
    fn default() -> Self {
        let cpus = std::thread::available_parallelism().map_or(1, NonZeroUsize::get);
        let slots =
            NonZeroUsize::new(cpus.min(MAX_DEFAULT_PASSWORD_WORK)).unwrap_or(NonZeroUsize::MIN);
        Self::new(slots, DEFAULT_PASSWORD_WORK_WAIT)
    }
}

impl PasswordWorkLimiter {
    /// A limiter running at most `max_concurrent` hashes at once; an attempt
    /// waits at most `max_wait` for a slot (zero: fail at once when full).
    #[must_use]
    pub fn new(max_concurrent: NonZeroUsize, max_wait: Duration) -> Self {
        let max_concurrent = max_concurrent.get().min(Semaphore::MAX_PERMITS);
        Self {
            slots: Arc::new(Semaphore::new(max_concurrent)),
            max_concurrent,
            max_wait,
        }
    }

    /// The bound on concurrent hashes.
    #[must_use]
    pub const fn max_concurrent(&self) -> usize {
        self.max_concurrent
    }

    /// Runs `work` on the blocking pool once a slot is free.
    ///
    /// # Errors
    ///
    /// [`PasswordHashError::Unavailable`] when no slot frees up within the
    /// wait, or the work panicked or could not be started.
    pub async fn run<T, F>(&self, work: F) -> Result<T, PasswordHashError>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let slot =
            match tokio::time::timeout(self.max_wait, self.slots.clone().acquire_owned()).await {
                Ok(Ok(slot)) => slot,
                // Timed out, or the semaphore was closed (never done here).
                Ok(Err(_)) | Err(_) => return Err(PasswordHashError::Unavailable),
            };
        tokio::task::spawn_blocking(move || {
            // Released when the work ends, even if the caller stopped
            // waiting for it.
            let _slot = slot;
            work()
        })
        .await
        .map_err(|_| PasswordHashError::Unavailable)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    };

    use super::*;

    fn slots(count: usize) -> NonZeroUsize {
        NonZeroUsize::new(count).expect("positive")
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_work_never_exceeds_the_bound() {
        let limiter = PasswordWorkLimiter::new(slots(3), Duration::from_secs(30));
        let running = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for _ in 0..24 {
            let limiter = limiter.clone();
            let running = running.clone();
            let peak = peak.clone();
            tasks.push(tokio::spawn(async move {
                limiter
                    .run(move || {
                        let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                        peak.fetch_max(now, Ordering::SeqCst);
                        std::thread::sleep(Duration::from_millis(20));
                        running.fetch_sub(1, Ordering::SeqCst);
                    })
                    .await
            }));
        }
        for task in tasks {
            task.await
                .expect("join")
                .expect("every attempt eventually runs");
        }
        assert_eq!(
            peak.load(Ordering::SeqCst),
            3,
            "the bound is reached, never passed"
        );
        assert_eq!(running.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_full_limiter_fails_fast_after_its_wait() {
        let limiter = PasswordWorkLimiter::new(slots(1), Duration::from_millis(50));
        let (release, wait) = mpsc::channel::<()>();
        let holder = {
            let limiter = limiter.clone();
            tokio::spawn(async move {
                limiter
                    .run(move || {
                        let _ = wait.recv_timeout(Duration::from_secs(10));
                    })
                    .await
            })
        };
        // Let the holder take the only slot.
        while limiter.slots.available_permits() != 0 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let started = std::time::Instant::now();
        assert_eq!(
            limiter.run(|| ()).await,
            Err(PasswordHashError::Unavailable)
        );
        assert!(started.elapsed() < Duration::from_secs(5));
        release.send(()).expect("release");
        holder.await.expect("join").expect("holder ran");
        assert_eq!(
            limiter.run(|| 7).await,
            Ok(7),
            "a freed slot is usable again"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_cancelled_caller_keeps_its_slot_until_the_work_ends() {
        let limiter = PasswordWorkLimiter::new(slots(1), Duration::ZERO);
        let (release, wait) = mpsc::channel::<()>();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
        let caller = {
            let limiter = limiter.clone();
            tokio::spawn(async move {
                limiter
                    .run(move || {
                        let _ = started_tx.send(());
                        let _ = wait.recv_timeout(Duration::from_secs(10));
                    })
                    .await
            })
        };
        started_rx.await.expect("work started");
        caller.abort();
        let _ = caller.await;
        // The hash is still running: the slot is still taken.
        assert_eq!(
            limiter.run(|| ()).await,
            Err(PasswordHashError::Unavailable)
        );
        release.send(()).expect("release");
        let mut freed = false;
        for _ in 0..1000 {
            if limiter.slots.available_permits() == 1 {
                freed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(freed, "the slot returns when the work ends");
    }

    #[tokio::test]
    async fn a_panicking_hash_is_unavailable_and_frees_its_slot() {
        let limiter = PasswordWorkLimiter::new(slots(1), Duration::from_secs(5));
        let result: Result<(), _> = limiter.run(|| panic!("hash failed")).await;
        assert_eq!(result, Err(PasswordHashError::Unavailable));
        assert_eq!(limiter.run(|| 1).await, Ok(1));
    }

    #[test]
    fn the_default_bound_follows_the_cpus_within_limits() {
        let limiter = PasswordWorkLimiter::default();
        assert!((1..=MAX_DEFAULT_PASSWORD_WORK).contains(&limiter.max_concurrent()));
        assert_eq!(limiter.max_wait, DEFAULT_PASSWORD_WORK_WAIT);
        let rendered = format!("{limiter:?}");
        assert!(rendered.contains("max_concurrent"), "{rendered}");
    }
}
