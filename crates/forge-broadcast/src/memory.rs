//! The in-process broker: bounded queues, no tasks.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};

use async_trait::async_trait;
use forge_core::Cancellation;
use tokio::sync::mpsc::{self, error::TrySendError};

use crate::{BroadcastError, Broker, Channel, Message};

/// Default limit of live subscriptions in one process.
pub const DEFAULT_MAX_SUBSCRIBERS: usize = 10_000;
/// Default limit of live subscriptions to one channel.
pub const DEFAULT_MAX_SUBSCRIBERS_PER_CHANNEL: usize = 1_000;
/// Default number of undelivered messages one subscriber may have queued.
pub const DEFAULT_BUFFER: usize = 64;
/// Default size limit of one message (1 MiB).
pub const DEFAULT_MAX_MESSAGE_BYTES: usize = 1 << 20;

/// Limits of a [`MemoryBroker`]. The defaults are Forge-Go's.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MemoryOptions {
    /// Live subscriptions in the process.
    pub max_subscribers: usize,
    /// Live subscriptions to one channel.
    pub max_subscribers_per_channel: usize,
    /// Undelivered messages one subscriber may have queued. A subscriber
    /// whose queue is full when a message arrives is dropped.
    pub buffer: usize,
    /// Size of one message, in bytes.
    pub max_message_bytes: usize,
}

impl Default for MemoryOptions {
    fn default() -> Self {
        Self {
            max_subscribers: DEFAULT_MAX_SUBSCRIBERS,
            max_subscribers_per_channel: DEFAULT_MAX_SUBSCRIBERS_PER_CHANNEL,
            buffer: DEFAULT_BUFFER,
            max_message_bytes: DEFAULT_MAX_MESSAGE_BYTES,
        }
    }
}

/// A [`Broker`] for one process.
///
/// It starts no tasks. Publishing does a non-blocking send to each
/// subscriber's bounded queue ([`MemoryOptions::buffer`]); a subscriber whose
/// queue is full is **dropped** (its stream ends with
/// [`SubscriptionEnd::SlowConsumer`] after the messages already queued, and
/// the browser reconnects) rather than slowing the publisher or growing
/// memory. Subscriptions are capped per channel and per process.
///
/// The broker closes when the [`Cancellation`] given to
/// [`MemoryBroker::new`] is cancelled (pass the lifecycle's shutdown-start
/// signal) or on [`MemoryBroker::close`]: every subscription then ends at
/// once, so open event streams do not hold up the HTTP server's drain.
/// It does not reach other replicas; use `postgres::PgBroker` for that.
///
/// Cloning shares the broker.
#[derive(Clone, Debug)]
pub struct MemoryBroker {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    options: MemoryOptions,
    closed: Cancellation,
    state: Mutex<State>,
    dropped: AtomicU64,
}

#[derive(Debug, Default)]
struct State {
    next_id: u64,
    total: usize,
    channels: HashMap<String, HashMap<u64, Subscriber>>,
}

#[derive(Debug)]
struct Subscriber {
    sender: mpsc::Sender<Message>,
    end: Arc<OnceLock<SubscriptionEnd>>,
}

/// Why a [`Subscription`] ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubscriptionEnd {
    /// The broker closed (shutdown).
    Closed,
    /// The subscriber was dropped because its queue was full: it did not read
    /// fast enough.
    SlowConsumer,
}

impl MemoryBroker {
    /// A broker that closes when `shutdown` is cancelled. Zero limits are
    /// refused.
    pub fn new(options: MemoryOptions, shutdown: &Cancellation) -> Result<Self, BroadcastError> {
        if options.max_subscribers == 0
            || options.max_subscribers_per_channel == 0
            || options.buffer == 0
            || options.max_message_bytes == 0
        {
            return Err(BroadcastError::Configuration(
                "memory broker limits must be positive",
            ));
        }
        Ok(Self {
            inner: Arc::new(Inner {
                options,
                // A child: closing the broker never cancels the lifecycle.
                closed: shutdown.child(),
                state: Mutex::new(State::default()),
                dropped: AtomicU64::new(0),
            }),
        })
    }

    /// Ends every subscription and refuses new ones and new messages. It is
    /// idempotent.
    pub fn close(&self) {
        self.inner.closed.cancel();
        if let Ok(mut state) = self.inner.state.lock() {
            state.channels.clear();
            state.total = 0;
        }
    }

    /// Cancelled when the broker closes.
    #[cfg_attr(not(feature = "postgres"), allow(dead_code))]
    pub(crate) fn closed_signal(&self) -> Cancellation {
        self.inner.closed.clone()
    }

    /// Whether the broker is closed.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.inner.closed.is_cancelled()
    }

    /// The number of live subscriptions.
    #[must_use]
    pub fn subscribers(&self) -> usize {
        self.inner.lock().map_or(0, |state| state.total)
    }

    /// How many subscriptions were dropped because their queue was full.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.inner.dropped.load(Ordering::Relaxed)
    }

    /// The largest message [`Broker::publish`] accepts.
    #[must_use]
    pub fn max_message_bytes(&self) -> usize {
        self.inner.options.max_message_bytes
    }

    /// Delivers `message` without waiting (the synchronous core of
    /// [`Broker::publish`]).
    pub fn deliver(&self, channel: &Channel, message: &Message) -> Result<(), BroadcastError> {
        if message.len() > self.inner.options.max_message_bytes {
            return Err(BroadcastError::MessageTooLarge);
        }
        if self.is_closed() {
            return Err(BroadcastError::Closed);
        }
        let key = channel.key();
        let mut state = self.inner.lock()?;
        let Some(subscribers) = state.channels.get_mut(&key) else {
            return Ok(());
        };
        let mut gone = Vec::new();
        for (id, subscriber) in subscribers.iter() {
            match subscriber.sender.try_send(message.clone()) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    let _ = subscriber.end.set(SubscriptionEnd::SlowConsumer);
                    self.inner.dropped.fetch_add(1, Ordering::Relaxed);
                    gone.push(*id);
                }
                // The subscription is being dropped; its Drop unregisters it.
                Err(TrySendError::Closed(_)) => gone.push(*id),
            }
        }
        // Removing the sender ends the subscription once its queue drains.
        for id in &gone {
            subscribers.remove(id);
        }
        let empty = subscribers.is_empty();
        state.total -= gone.len();
        if empty {
            state.channels.remove(&key);
        }
        Ok(())
    }
}

impl Inner {
    fn lock(&self) -> Result<MutexGuard<'_, State>, BroadcastError> {
        // A poisoned registry is never used again: fail closed.
        self.state.lock().map_err(|_| BroadcastError::Closed)
    }

    fn unregister(&self, key: &str, id: u64) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        let Some(subscribers) = state.channels.get_mut(key) else {
            return;
        };
        if subscribers.remove(&id).is_some() {
            let empty = subscribers.is_empty();
            state.total -= 1;
            if empty {
                state.channels.remove(key);
            }
        }
    }
}

#[async_trait]
impl Broker for MemoryBroker {
    async fn publish(&self, channel: &Channel, message: Message) -> Result<(), BroadcastError> {
        self.deliver(channel, &message)
    }

    fn subscribe(&self, channel: &Channel) -> Result<Subscription, BroadcastError> {
        if self.is_closed() {
            return Err(BroadcastError::Closed);
        }
        let options = self.inner.options;
        let key = channel.key();
        let mut state = self.inner.lock()?;
        let on_channel = state.channels.get(&key).map_or(0, HashMap::len);
        if state.total >= options.max_subscribers
            || on_channel >= options.max_subscribers_per_channel
        {
            return Err(BroadcastError::TooManySubscribers);
        }
        let (sender, receiver) = mpsc::channel(options.buffer);
        let end = Arc::new(OnceLock::new());
        let id = state.next_id;
        state.next_id += 1;
        state.total += 1;
        state.channels.entry(key.clone()).or_default().insert(
            id,
            Subscriber {
                sender,
                end: Arc::clone(&end),
            },
        );
        Ok(Subscription {
            id,
            key,
            receiver,
            end,
            closed: self.inner.closed.clone(),
            broker: Arc::downgrade(&self.inner),
        })
    }
}

/// A live subscription to one channel. Dropping it unsubscribes.
#[derive(Debug)]
pub struct Subscription {
    id: u64,
    key: String,
    receiver: mpsc::Receiver<Message>,
    end: Arc<OnceLock<SubscriptionEnd>>,
    closed: Cancellation,
    broker: Weak<Inner>,
}

impl Subscription {
    /// The next message, in publish order, or why the subscription ended.
    /// Cancel-safe: dropping the future loses no message.
    pub async fn next(&mut self) -> Result<Message, SubscriptionEnd> {
        if self.closed.is_cancelled() {
            return Err(SubscriptionEnd::Closed);
        }
        tokio::select! {
            biased;
            () = self.closed.cancelled() => Err(SubscriptionEnd::Closed),
            received = self.receiver.recv() => received.ok_or_else(|| {
                self.end.get().copied().unwrap_or(SubscriptionEnd::Closed)
            }),
        }
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        if let Some(broker) = self.broker.upgrade() {
            broker.unregister(&self.key, self.id);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use forge_tenancy::TenantId;

    use super::*;

    fn broker(options: MemoryOptions) -> (MemoryBroker, Cancellation) {
        let shutdown = Cancellation::new();
        let broker = MemoryBroker::new(options, &shutdown).expect("valid options");
        (broker, shutdown)
    }

    fn posts() -> Channel {
        Channel::global("posts").expect("valid")
    }

    async fn next(subscription: &mut Subscription) -> Result<Message, SubscriptionEnd> {
        tokio::time::timeout(Duration::from_secs(5), subscription.next())
            .await
            .expect("the subscription must answer")
    }

    #[tokio::test]
    async fn subscribers_receive_messages_in_order_and_only_their_channel() {
        let (broker, _shutdown) = broker(MemoryOptions::default());
        let tenant = TenantId::new();
        let mine = Channel::for_tenant(tenant, "posts").expect("valid");
        let mut global = broker.subscribe(&posts()).expect("subscribes");
        let mut tenant_posts = broker.subscribe(&mine).expect("subscribes");
        let mut elsewhere = broker
            .subscribe(&Channel::for_tenant(TenantId::new(), "posts").expect("valid"))
            .expect("subscribes");

        broker
            .publish(&mine, Message::new("one"))
            .await
            .expect("publishes");
        broker
            .publish(&mine, Message::new("two"))
            .await
            .expect("publishes");
        broker
            .publish(&posts(), Message::new("everyone"))
            .await
            .expect("publishes");

        assert_eq!(next(&mut tenant_posts).await, Ok(Message::new("one")));
        assert_eq!(next(&mut tenant_posts).await, Ok(Message::new("two")));
        assert_eq!(next(&mut global).await, Ok(Message::new("everyone")));
        // Another tenant's channel of the same name hears nothing.
        assert!(
            tokio::time::timeout(Duration::from_millis(50), elsewhere.next())
                .await
                .is_err()
        );
        assert_eq!(broker.subscribers(), 3);
        drop(elsewhere);
        assert_eq!(broker.subscribers(), 2);
    }

    #[tokio::test]
    async fn a_slow_consumer_is_dropped_without_blocking_the_publisher() {
        let (broker, _shutdown) = broker(MemoryOptions {
            buffer: 2,
            ..MemoryOptions::default()
        });
        let mut slow = broker.subscribe(&posts()).expect("subscribes");
        let mut fast = broker.subscribe(&posts()).expect("subscribes");

        for index in 0..2 {
            broker
                .publish(&posts(), Message::new(format!("m{index}")))
                .await
                .expect("publishes");
            assert_eq!(next(&mut fast).await, Ok(Message::new(format!("m{index}"))));
        }
        // The third message finds the slow queue full: dropped, not awaited.
        broker
            .publish(&posts(), Message::new("m2"))
            .await
            .expect("publishing never waits");

        assert_eq!(broker.dropped(), 1);
        assert_eq!(broker.subscribers(), 1);
        assert_eq!(next(&mut slow).await, Ok(Message::new("m0")));
        assert_eq!(next(&mut slow).await, Ok(Message::new("m1")));
        assert_eq!(next(&mut slow).await, Err(SubscriptionEnd::SlowConsumer));
        assert_eq!(next(&mut fast).await, Ok(Message::new("m2")));
    }

    #[tokio::test]
    async fn subscriber_limits_apply_per_channel_and_per_process() {
        let (broker, _shutdown) = broker(MemoryOptions {
            max_subscribers: 3,
            max_subscribers_per_channel: 2,
            ..MemoryOptions::default()
        });
        let first = broker.subscribe(&posts()).expect("subscribes");
        let _second = broker.subscribe(&posts()).expect("subscribes");
        assert!(matches!(
            broker.subscribe(&posts()),
            Err(BroadcastError::TooManySubscribers)
        ));
        let other = Channel::global("comments").expect("valid");
        let _third = broker.subscribe(&other).expect("subscribes");
        assert!(matches!(
            broker.subscribe(&other),
            Err(BroadcastError::TooManySubscribers)
        ));
        drop(first);
        assert!(broker.subscribe(&posts()).is_ok(), "a slot was freed");
    }

    #[tokio::test]
    async fn oversized_messages_are_refused() {
        let (broker, _shutdown) = broker(MemoryOptions {
            max_message_bytes: 4,
            ..MemoryOptions::default()
        });
        assert!(matches!(
            broker.publish(&posts(), Message::new("12345")).await,
            Err(BroadcastError::MessageTooLarge)
        ));
        assert!(broker.publish(&posts(), Message::new("1234")).await.is_ok());
        assert!(
            MemoryBroker::new(
                MemoryOptions {
                    buffer: 0,
                    ..MemoryOptions::default()
                },
                &Cancellation::new()
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn shutdown_ends_every_subscription_and_refuses_new_ones() {
        let (broker, shutdown) = broker(MemoryOptions::default());
        let mut waiting = broker.subscribe(&posts()).expect("subscribes");
        let pending = tokio::spawn(async move { waiting.next().await });
        tokio::task::yield_now().await;

        shutdown.cancel();

        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), pending)
                .await
                .expect("ends promptly")
                .expect("no panic"),
            Err(SubscriptionEnd::Closed)
        );
        assert!(broker.is_closed());
        assert!(matches!(
            broker.subscribe(&posts()),
            Err(BroadcastError::Closed)
        ));
        assert!(matches!(
            broker.publish(&posts(), Message::new("x")).await,
            Err(BroadcastError::Closed)
        ));
    }

    #[tokio::test]
    async fn closing_the_broker_does_not_cancel_the_lifecycle() {
        let (broker, shutdown) = broker(MemoryOptions::default());
        let mut subscription = broker.subscribe(&posts()).expect("subscribes");

        broker.close();
        broker.close();

        assert_eq!(next(&mut subscription).await, Err(SubscriptionEnd::Closed));
        assert!(!shutdown.is_cancelled());
        assert_eq!(broker.subscribers(), 0);
    }
}
