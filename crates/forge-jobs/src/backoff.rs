//! Retry delays: capped exponential backoff with jitter.

use std::time::Duration;

/// Delay before retrying a failed attempt.
///
/// Attempt `n` (1-based) has a ceiling of `base * 2^(n-1)`, capped at `max`.
/// With jitter (the default) the delay is drawn uniformly from
/// `[ceiling / 2, ceiling]` ("equal jitter"), so retries of jobs that failed
/// together spread out instead of hammering a recovering dependency at the
/// same instant, while every retry still waits at least half the ceiling.
///
/// ```
/// use std::time::Duration;
/// use forge_jobs::Backoff;
///
/// let backoff = Backoff::exponential(Duration::from_secs(1), Duration::from_secs(60));
/// assert_eq!(backoff.ceiling(1), Duration::from_secs(1));
/// assert_eq!(backoff.ceiling(4), Duration::from_secs(8));
/// assert_eq!(backoff.ceiling(30), Duration::from_secs(60));
/// let delay = backoff.delay(4);
/// assert!(delay >= Duration::from_secs(4) && delay <= Duration::from_secs(8));
/// ```
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Backoff {
    base: Duration,
    max: Duration,
    jitter: bool,
}

impl Backoff {
    /// 5 seconds doubling up to one hour, with jitter.
    pub const DEFAULT: Self = Self::exponential(Duration::from_secs(5), Duration::from_secs(3600));

    /// Exponential backoff from `base` up to `max` (`max` is raised to
    /// `base` when smaller), with jitter.
    #[must_use]
    pub const fn exponential(base: Duration, max: Duration) -> Self {
        let max = if max.as_nanos() < base.as_nanos() {
            base
        } else {
            max
        };
        Self {
            base,
            max,
            jitter: true,
        }
    }

    /// The same delay before every retry, with jitter.
    #[must_use]
    pub const fn fixed(delay: Duration) -> Self {
        Self::exponential(delay, delay)
    }

    /// Disables jitter: every delay equals its ceiling.
    #[must_use]
    pub const fn without_jitter(mut self) -> Self {
        self.jitter = false;
        self
    }

    /// The upper bound of the delay after failed attempt `attempt` (1-based;
    /// 0 is treated as 1).
    #[must_use]
    pub fn ceiling(&self, attempt: u32) -> Duration {
        let exponent = attempt.saturating_sub(1).min(63);
        let factor = 1_u64 << exponent;
        let factor = u32::try_from(factor).unwrap_or(u32::MAX);
        self.base
            .checked_mul(factor)
            .map_or(self.max, |delay| delay.min(self.max))
    }

    /// The delay after failed attempt `attempt`, jittered when enabled.
    #[must_use]
    pub fn delay(&self, attempt: u32) -> Duration {
        let ceiling = self.ceiling(attempt);
        if self.jitter {
            jitter(ceiling)
        } else {
            ceiling
        }
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// A uniformly random duration in `[ceiling / 2, ceiling]`.
///
/// Randomness comes from the operating system; if it is unavailable the
/// ceiling is returned (jitter is a load-spreading measure, not a security
/// property).
pub(crate) fn jitter(ceiling: Duration) -> Duration {
    let half = ceiling / 2;
    let span = u64::try_from((ceiling - half).as_micros()).unwrap_or(u64::MAX);
    match getrandom::u64() {
        Ok(random) if span > 0 => half + Duration::from_micros(random % (span + 1)),
        _ => ceiling,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ceilings_double_and_cap() {
        let backoff = Backoff::exponential(Duration::from_secs(2), Duration::from_secs(100));
        let ceilings: Vec<u64> = (0..=8).map(|n| backoff.ceiling(n).as_secs()).collect();
        assert_eq!(ceilings, [2, 2, 4, 8, 16, 32, 64, 100, 100]);
        assert_eq!(backoff.ceiling(u32::MAX), Duration::from_secs(100));
    }

    #[test]
    fn jittered_delays_stay_within_bounds_and_vary() {
        let backoff = Backoff::exponential(Duration::from_millis(100), Duration::from_secs(60));
        let mut distinct = std::collections::HashSet::new();
        for attempt in 1..=12 {
            let ceiling = backoff.ceiling(attempt);
            for _ in 0..200 {
                let delay = backoff.delay(attempt);
                assert!(
                    delay >= ceiling / 2 && delay <= ceiling,
                    "{delay:?} {ceiling:?}"
                );
                distinct.insert(delay);
            }
        }
        assert!(distinct.len() > 100, "jitter must spread delays");
    }

    #[test]
    fn jitter_can_be_disabled_and_fixed_delays_are_constant() {
        let fixed = Backoff::fixed(Duration::from_secs(3)).without_jitter();
        assert!((1..10).all(|n| fixed.delay(n) == Duration::from_secs(3)));
        let inverted = Backoff::exponential(Duration::from_secs(5), Duration::from_secs(1));
        assert_eq!(inverted.ceiling(3), Duration::from_secs(5));
        assert_eq!(jitter(Duration::ZERO), Duration::ZERO);
    }
}
