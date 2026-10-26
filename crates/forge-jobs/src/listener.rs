//! The enqueue-announcement listener (ADR 0046): one `LISTEN` connection per
//! job runtime that wakes idle queue workers, reconnecting with jittered
//! backoff while polling continues.

use std::{collections::HashMap, time::Duration};

use forge_core::Cancellation;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::{
    JobsError, MAX_QUEUE_NAME,
    backoff::jitter,
    name,
    postgres::{PgJobStore, Subscription},
    worker::Chain,
};

/// First reconnect ceiling: the first retry waits 0.5 to 1 second.
const MIN_BACKOFF: Duration = Duration::from_secs(1);
/// Largest reconnect ceiling: retries wait 15 to 30 seconds at most.
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// One coalescing wake-up signal per queue: a channel of capacity one, so
/// any number of announcements before the worker looks merge into one.
pub(crate) struct Wakes(HashMap<&'static str, mpsc::Sender<()>>);

impl Wakes {
    pub(crate) fn new() -> Self {
        Self(HashMap::new())
    }

    /// Registers `queue` and returns the receiver its worker waits on.
    pub(crate) fn register(&mut self, queue: &'static str) -> mpsc::Receiver<()> {
        let (sender, receiver) = mpsc::channel(1);
        self.0.insert(queue, sender);
        receiver
    }

    /// Wakes the worker of the queue named by `payload` without waiting.
    /// The channel is writable by anyone who can connect to the database:
    /// payloads that are not valid queue names, and queues this runtime does
    /// not run, are ignored.
    pub(crate) fn wake(&self, payload: &str) {
        if !name::is_valid(payload, MAX_QUEUE_NAME) {
            debug!("ignored a job announcement that is not a queue name");
            return;
        }
        if let Some(sender) = self.0.get(payload) {
            // Full: a wake-up is already pending. Closed: the worker stopped.
            let _ = sender.try_send(());
        }
    }

    /// Wakes every queue once: announcements sent while the subscription
    /// was down were missed.
    fn wake_all(&self) {
        for sender in self.0.values() {
            let _ = sender.try_send(());
        }
    }
}

/// How one subscription ended.
enum Ended {
    /// Shutdown.
    Stopped,
    /// It never listened.
    Failed(JobsError),
    /// It listened, then the connection was lost.
    Lost(Option<JobsError>),
}

/// Holds a subscription until `shutdown`, reconnecting after a jittered
/// `[backoff / 2, backoff]`, `backoff` doubling from 1 s to 30 s while
/// subscribing fails and starting over after a subscription that worked.
/// Every (re)subscription wakes each queue once.
pub(crate) async fn run(store: PgJobStore, wakes: Wakes, shutdown: Cancellation) {
    let mut backoff = MIN_BACKOFF;
    loop {
        let mut subscription = store.subscription();
        let ended = forward_until_lost(&mut subscription, &wakes, &shutdown).await;
        subscription.close().await;
        let error = match ended {
            Ended::Stopped => break,
            Ended::Failed(error) => Some(error),
            Ended::Lost(error) => {
                backoff = MIN_BACKOFF;
                error
            }
        };
        let delay = jitter(backoff);
        let retry_in_ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX);
        match &error {
            Some(error) => warn!(
                error = %Chain(error),
                retry_in_ms,
                "job announcement listener disconnected; polling continues"
            ),
            None => warn!(
                retry_in_ms,
                "job announcement listener disconnected; polling continues"
            ),
        }
        tokio::select! {
            () = shutdown.cancelled() => break,
            () = tokio::time::sleep(delay) => {}
        }
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
    info!("job announcement listener stopped");
}

/// Subscribes, then forwards announcements until the connection is lost or
/// `shutdown`.
async fn forward_until_lost(
    subscription: &mut Subscription,
    wakes: &Wakes,
    shutdown: &Cancellation,
) -> Ended {
    let listening = tokio::select! {
        () = shutdown.cancelled() => return Ended::Stopped,
        listening = subscription.listen() => listening,
    };
    if let Err(error) = listening {
        return Ended::Failed(error);
    }
    info!("listening for job announcements");
    wakes.wake_all();
    loop {
        let next = tokio::select! {
            () = shutdown.cancelled() => return Ended::Stopped,
            next = subscription.next() => next,
        };
        match next {
            Ok(Some(payload)) => wakes.wake(&payload),
            Ok(None) => return Ended::Lost(None),
            Err(error) => return Ended::Lost(Some(error)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wake_ups_coalesce_and_ignore_unknown_or_invalid_queues() {
        let mut wakes = Wakes::new();
        let mut mail = wakes.register("mail");
        let mut other = wakes.register("default");

        for payload in ["mail", "mail", "mail"] {
            wakes.wake(payload);
        }
        assert!(mail.try_recv().is_ok(), "an announcement wakes its queue");
        assert!(
            mail.try_recv().is_err(),
            "repeated announcements merge into one wake-up"
        );

        let long = "a".repeat(MAX_QUEUE_NAME + 1);
        for payload in ["", "Mail", "mail; DROP", "unknown", long.as_str()] {
            wakes.wake(payload);
        }
        assert!(mail.try_recv().is_err());
        assert!(other.try_recv().is_err());

        wakes.wake_all();
        assert!(mail.try_recv().is_ok() && other.try_recv().is_ok());

        // A stopped worker is not an error.
        drop(mail);
        wakes.wake("mail");
        wakes.wake_all();
    }

    #[test]
    fn reconnect_waits_start_at_half_a_second_and_cap_at_thirty() {
        let first = jitter(MIN_BACKOFF);
        assert!(first >= Duration::from_millis(500) && first <= Duration::from_secs(1));
        let mut backoff = MIN_BACKOFF;
        for _ in 0..10 {
            backoff = (backoff * 2).min(MAX_BACKOFF);
        }
        assert_eq!(backoff, MAX_BACKOFF);
        let last = jitter(backoff);
        assert!(last >= Duration::from_secs(15) && last <= Duration::from_secs(30));
    }
}
