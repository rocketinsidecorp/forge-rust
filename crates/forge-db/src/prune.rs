//! Pacing of inline pruning for database-backed caches and limiters.

use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

/// Bounds how many batches one prune removes, so a call is bounded work (at
/// most `MAX_PRUNE_ROUNDS x batch` rows per kind) yet keeps up with a sustained
/// stream of new keys.
pub const MAX_PRUNE_ROUNDS: u32 = 20;

/// Decides when a store prunes inline after a write or hit: when the prune interval has
/// passed, or when `batch` hits have happened since the last prune, whichever
/// comes first, so expired buckets cannot pile up faster than they are
/// removed. Cheap (two atomics) and lock-free; the stores own one each.
#[derive(Debug)]
pub struct PruneGate {
    epoch: Instant,
    last_nanos: AtomicU64,
    writes: AtomicU64,
}

impl Default for PruneGate {
    fn default() -> Self {
        Self::new()
    }
}

impl PruneGate {
    /// A gate whose interval starts now.
    #[must_use]
    pub fn new() -> Self {
        Self {
            epoch: Instant::now(),
            last_nanos: AtomicU64::new(0),
            writes: AtomicU64::new(0),
        }
    }

    /// Records one hit and reports whether the caller should prune now.
    /// `interval` of `None` disables inline pruning.
    pub fn due(&self, interval: Option<Duration>, batch: u32) -> bool {
        let Some(interval) = interval else {
            return false;
        };
        let hits = self.writes.fetch_add(1, Ordering::Relaxed) + 1;
        let now = u64::try_from(self.epoch.elapsed().as_nanos()).unwrap_or(u64::MAX);
        let last = self.last_nanos.load(Ordering::Relaxed);
        let interval_nanos = u64::try_from(interval.as_nanos()).unwrap_or(u64::MAX);
        if hits < u64::from(batch) && now.saturating_sub(last) < interval_nanos {
            return false;
        }
        // Only the caller that wins the exchange prunes.
        if self
            .last_nanos
            .compare_exchange(last, now, Ordering::AcqRel, Ordering::Relaxed)
            .is_err()
        {
            return false;
        }
        self.writes.store(0, Ordering::Relaxed);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prunes_after_every_batch_of_hits_even_with_a_long_interval() {
        let gate = PruneGate::new();
        let interval = Some(Duration::from_secs(3600));
        let due: Vec<bool> = (0..9).map(|_| gate.due(interval, 3)).collect();
        assert_eq!(
            due,
            vec![false, false, true, false, false, true, false, false, true]
        );
    }

    #[test]
    fn prunes_after_the_interval_and_never_when_disabled() {
        let gate = PruneGate::new();
        assert!(!gate.due(Some(Duration::from_secs(3600)), 1_000_000));
        // real-clock: PruneGate measures std::time::Instant and takes no clock, so the interval must really elapse
        std::thread::sleep(Duration::from_millis(5));
        assert!(gate.due(Some(Duration::from_millis(1)), 1_000_000));
        for _ in 0..100 {
            assert!(!gate.due(None, 1));
        }
    }
}
