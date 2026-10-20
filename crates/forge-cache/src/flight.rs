use std::{
    collections::HashMap,
    sync::{Mutex, MutexGuard, PoisonError},
};

use tokio::sync::watch;

use crate::fetch::Outcome;

type Slot = watch::Receiver<Option<Outcome>>;

/// Collapses concurrent computations of the same key in one process into one:
/// the first caller computes, the others wait for its result.
///
/// It owns no tasks: the computation runs on the first caller's own future and
/// waiters wait on a watch channel. Cancellation is dropping a future. If the
/// computing caller (the leader) is dropped before it finishes, its waiters do
/// **not** inherit that cancellation: the channel closes without a result and
/// one of them takes over and computes for itself. A leader whose computation
/// panics is released the same way.
///
/// Every cache owns one; the default value is ready to use.
#[derive(Debug, Default)]
pub struct Flight {
    calls: Mutex<HashMap<String, Slot>>,
}

/// The role a caller plays for one key.
pub(crate) enum Role<'a> {
    Lead(Lead<'a>),
    Wait(Slot),
}

/// The leader's handle. Dropping it without [`Lead::finish`] abandons the call
/// and releases the waiters to take over.
pub(crate) struct Lead<'a> {
    flight: &'a Flight,
    key: String,
    sender: watch::Sender<Option<Outcome>>,
    slot: Slot,
}

impl Flight {
    fn lock(&self) -> MutexGuard<'_, HashMap<String, Slot>> {
        self.calls.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Becomes the leader for `key`, or returns the call to wait on.
    pub(crate) fn join(&self, key: &str) -> Role<'_> {
        let mut calls = self.lock();
        if let Some(slot) = calls.get(key) {
            return Role::Wait(slot.clone());
        }
        let (sender, slot) = watch::channel(None);
        calls.insert(key.to_owned(), slot.clone());
        Role::Lead(Lead {
            flight: self,
            key: key.to_owned(),
            sender,
            slot,
        })
    }
}

impl Lead<'_> {
    /// Publishes the outcome to the waiters and ends the call.
    pub(crate) fn finish(self, outcome: Outcome) {
        self.sender.send_replace(Some(outcome));
    }
}

impl Drop for Lead<'_> {
    fn drop(&mut self) {
        let mut calls = self.flight.lock();
        // Only remove our own call: a successor may already have replaced it.
        if calls
            .get(&self.key)
            .is_some_and(|slot| slot.same_channel(&self.slot))
        {
            calls.remove(&self.key);
        }
        // The sender drops next: waiters of an unfinished call see a closed
        // channel and take over.
    }
}

/// Waits for the leader's outcome; `None` when the leader went away without
/// one.
pub(crate) async fn wait(mut slot: Slot) -> Option<Outcome> {
    let published = slot.wait_for(Option::is_some).await.ok()?;
    published.clone()
}
