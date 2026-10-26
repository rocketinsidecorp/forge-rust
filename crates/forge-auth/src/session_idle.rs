//! The session idle timeout (Forge-Go's `auth/postgres.WithSessionIdleTimeout`).
//!
//! Besides its absolute expiry, a session ends when nobody used it for the
//! idle timeout: the session store no longer resolves it, exactly as if it
//! were revoked, and signing in again creates a new one. The store records
//! each use at most once per [`SESSION_TOUCH_INTERVAL`], so most requests
//! only read. `FORGE_AUTH_SESSION_IDLE_TIMEOUT` configures it; there is no
//! value that turns it off (the longest, 720 hours, is the longest session
//! lifetime, where it no longer shortens anything).

use std::time::Duration;

use thiserror::Error;

/// How stale a session's recorded last use may get before resolving it
/// records the use again: at most one write per session and interval,
/// however many requests the session makes.
pub const SESSION_TOUCH_INTERVAL: Duration = Duration::from_secs(60);

/// How long a session may go unused, between [`SessionIdleTimeout::MIN`] and
/// [`SessionIdleTimeout::MAX`]; two hours by default.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct SessionIdleTimeout(Duration);

impl SessionIdleTimeout {
    /// The default: two hours.
    pub const DEFAULT: Self = Self(Duration::from_secs(2 * 3600));
    /// The shortest accepted timeout: five minutes.
    pub const MIN: Duration = Duration::from_secs(5 * 60);
    /// The longest accepted timeout: 30 days (720 hours), the longest
    /// session lifetime.
    pub const MAX: Duration = Duration::from_secs(30 * 24 * 3600);

    /// Accepts a timeout between [`Self::MIN`] and [`Self::MAX`] inclusive.
    pub fn new(timeout: Duration) -> Result<Self, SessionIdleTimeoutError> {
        if (Self::MIN..=Self::MAX).contains(&timeout) {
            Ok(Self(timeout))
        } else {
            Err(SessionIdleTimeoutError)
        }
    }

    /// The timeout.
    #[must_use]
    pub const fn get(self) -> Duration {
        self.0
    }

    /// The timeout in whole microseconds, for binding to SQL as
    /// `$n::bigint * interval '1 microsecond'` (at most 2.6e12, so it always
    /// fits an `i64`).
    #[must_use]
    pub fn as_micros(self) -> i64 {
        // MAX bounds the value far below i64::MAX.
        i64::try_from(self.0.as_micros()).unwrap_or(i64::MAX)
    }
}

impl Default for SessionIdleTimeout {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// A session idle timeout outside its bounds.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[error("session idle timeout must be between 5 minutes and 30 days")]
pub struct SessionIdleTimeoutError;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_timeout_is_two_hours_by_default_and_bounded() {
        assert_eq!(
            SessionIdleTimeout::default().get(),
            Duration::from_secs(7200)
        );
        for (timeout, valid) in [
            (Duration::ZERO, false),
            (Duration::from_secs(4 * 60 + 59), false),
            (Duration::from_secs(30 * 24 * 3600 + 1), false),
            (Duration::from_secs(5 * 60), true),
            (Duration::from_secs(2 * 3600), true),
            (Duration::from_secs(30 * 24 * 3600), true),
        ] {
            let result = SessionIdleTimeout::new(timeout);
            assert_eq!(result.is_ok(), valid, "{timeout:?}");
            if let Ok(accepted) = result {
                assert_eq!(accepted.get(), timeout);
            }
        }
        assert_eq!(
            SessionIdleTimeoutError.to_string(),
            "session idle timeout must be between 5 minutes and 30 days"
        );
        assert_eq!(
            SessionIdleTimeout::new(SessionIdleTimeout::MAX)
                .map(SessionIdleTimeout::as_micros)
                .ok(),
            Some(2_592_000_000_000)
        );
        assert_eq!(SESSION_TOUCH_INTERVAL, Duration::from_secs(60));
    }
}
