//! Cross-replica fanout over PostgreSQL `LISTEN/NOTIFY` (feature `postgres`).
//!
//! Every replica's [`PgBroker`] listens on one notification channel of the
//! database (`forge_broadcast` by default) and delivers what it hears to its
//! own [`MemoryBroker`] subscribers. Publishing delivers locally at once and
//! sends a `NOTIFY`; each broker skips notifications carrying its own origin
//! id, so a message is heard once. A single replica therefore keeps working
//! while its listener reconnects.
//!
//! Limits and behavior worth knowing (the same as Forge-Go's):
//!
//! - `NOTIFY` payloads are limited to 8000 bytes, so a message is split into
//!   chunks of 4500 bytes sent as consecutive notifications of one
//!   transaction and reassembled by the listener with bounded buffers (64
//!   partial messages, the message limit, 30 seconds). Messages are limited
//!   to [`PgOptions::max_message_bytes`] (256 KiB by default).
//! - Delivery is at most once. Notifications sent while a listener is
//!   disconnected are lost; the listener reconnects with 1 to 30 second
//!   backoff and logs the gap.
//! - `NOTIFY` is database-wide and not tenant-aware: the channel key, which
//!   contains the tenant, separates tenants exactly as in memory. Whoever can
//!   `NOTIFY` on the database can publish, so the database role is the trust
//!   boundary; malformed notifications are logged and ignored.
//! - The listener holds one pool connection for its whole life (it counts
//!   against the pool size). `LISTEN` does not work through PgBouncer in
//!   transaction pooling mode; connect directly or use session pooling.
//! - Other backends are refused ([`ensure_fanout_supported`](crate::ensure_fanout_supported)),
//!   never emulated: MySQL/MariaDB and SQLite applications use the in-process
//!   broker.

mod envelope;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use forge_db::PgDatabase;
use sqlx::postgres::{PgConnection, PgListener};
use tracing::{error, warn};

use self::envelope::{Assembler, CHUNK_BYTES, Chunk, MAX_NOTIFY_BYTES, split};
use crate::{BroadcastError, Broker, Channel, MemoryBroker, Message, Subscription};

/// The notification channel brokers share by default.
pub const DEFAULT_NOTIFY_CHANNEL: &str = "forge_broadcast";
/// The largest message a [`PgBroker`] publishes by default (256 KiB).
pub const DEFAULT_MAX_MESSAGE_BYTES: usize = 256 << 10;

const MAX_PENDING: usize = 64;
const PENDING_LIFETIME: Duration = Duration::from_secs(30);
const MIN_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// Origin of transactional publishes: delivered to every replica, this one
/// included, only when the transaction commits.
const TRANSACTION_ORIGIN: &str = "-";

/// Settings of a [`PgBroker`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PgOptions {
    /// The PostgreSQL notification channel: a lowercase identifier of at
    /// most 63 bytes (`[a-z_][a-z0-9_]*`). Every replica of an application
    /// uses the same one; distinct applications sharing a database use
    /// distinct ones.
    pub notify_channel: String,
    /// The largest message published (capped by the local broker's limit).
    pub max_message_bytes: usize,
}

impl Default for PgOptions {
    fn default() -> Self {
        Self {
            notify_channel: DEFAULT_NOTIFY_CHANNEL.to_owned(),
            max_message_bytes: DEFAULT_MAX_MESSAGE_BYTES,
        }
    }
}

/// A [`Broker`] backed by PostgreSQL `LISTEN/NOTIFY` (see the module
/// documentation). Cloning shares the broker.
///
/// [`PgBroker::listen`] is the listener: run it as a lifecycle worker
/// (`Composition::worker`); it returns when the local broker closes, which
/// happens as shutdown begins.
#[derive(Clone, Debug)]
pub struct PgBroker {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    database: PgDatabase,
    local: MemoryBroker,
    notify_channel: String,
    origin: String,
    max_bytes: usize,
    listening: AtomicBool,
}

impl PgBroker {
    /// A broker over `database` delivering to `local`'s subscribers.
    pub fn new(
        database: PgDatabase,
        local: MemoryBroker,
        options: PgOptions,
    ) -> Result<Self, BroadcastError> {
        if !valid_notify_channel(&options.notify_channel) {
            return Err(BroadcastError::Configuration(
                "the notify channel must be a lowercase identifier of at most 63 bytes",
            ));
        }
        if options.max_message_bytes == 0 {
            return Err(BroadcastError::Configuration(
                "the message limit must be positive",
            ));
        }
        let max_bytes = options.max_message_bytes.min(local.max_message_bytes());
        Ok(Self {
            inner: Arc::new(Inner {
                database,
                local,
                notify_channel: options.notify_channel,
                origin: random_hex()?,
                max_bytes,
                listening: AtomicBool::new(false),
            }),
        })
    }

    /// Whether the listener currently holds its `LISTEN` connection, for
    /// readiness checks and tests.
    #[must_use]
    pub fn listening(&self) -> bool {
        self.inner.listening.load(Ordering::SeqCst)
    }

    /// The local broker.
    #[must_use]
    pub fn local(&self) -> &MemoryBroker {
        &self.inner.local
    }

    /// Queues `message` in the caller's transaction: it is delivered to every
    /// replica, this one included, only if the transaction commits, and
    /// never if it rolls back. Use it to broadcast a change atomically with
    /// the write that caused it.
    pub async fn publish_in(
        &self,
        connection: &mut PgConnection,
        channel: &Channel,
        message: &Message,
    ) -> Result<(), BroadcastError> {
        if message.len() > self.inner.max_bytes {
            return Err(BroadcastError::MessageTooLarge);
        }
        let chunks = self.chunks(TRANSACTION_ORIGIN, channel, message)?;
        self.notify(connection, &chunks).await
    }

    /// Listens until the local broker closes, reconnecting with 1 to 30
    /// second backoff. Run it as a lifecycle worker; it starts no tasks.
    pub async fn listen(&self) {
        let closed = self.inner.local.closed_signal();
        let mut backoff = MIN_BACKOFF;
        loop {
            let outcome = tokio::select! {
                () = closed.cancelled() => return,
                outcome = self.listen_once() => outcome,
            };
            self.inner.listening.store(false, Ordering::SeqCst);
            if let Listened::Connected = outcome {
                backoff = MIN_BACKOFF;
            }
            tokio::select! {
                () = closed.cancelled() => return,
                () = tokio::time::sleep(backoff) => {}
            }
            backoff = (backoff * 2).min(MAX_BACKOFF);
        }
    }

    /// Holds one `LISTEN` connection until it fails.
    async fn listen_once(&self) -> Listened {
        let mut listener = match PgListener::connect_with(self.inner.database.pool()).await {
            Ok(listener) => listener,
            Err(failure) => {
                error!(error = %failure, "broadcast listener could not connect");
                return Listened::Failed;
            }
        };
        // The channel name was validated as a plain identifier; sqlx quotes it.
        if let Err(failure) = listener.listen(&self.inner.notify_channel).await {
            error!(error = %failure, "broadcast listener could not LISTEN");
            return Listened::Failed;
        }
        self.inner.listening.store(true, Ordering::SeqCst);
        let mut assembler = Assembler::new(MAX_PENDING, self.inner.max_bytes, PENDING_LIFETIME);
        let max_chunks = self.inner.max_bytes.div_ceil(CHUNK_BYTES).max(1);
        loop {
            match listener.try_recv().await {
                Ok(Some(notification)) => {
                    self.deliver(&mut assembler, notification.payload(), max_chunks);
                }
                // sqlx reconnected and listens again; what was sent
                // meanwhile is gone.
                Ok(None) => {
                    warn!("broadcast listener reconnected; messages sent meanwhile are lost");
                }
                Err(failure) => {
                    error!(
                        error = %failure,
                        "broadcast listener disconnected; messages sent meanwhile are lost"
                    );
                    return Listened::Connected;
                }
            }
        }
    }

    /// Handles one notification. Anything malformed is logged and dropped:
    /// whoever can connect to the database can `NOTIFY`, and a bad payload
    /// must never stop the listener.
    fn deliver(&self, assembler: &mut Assembler, payload: &str, max_chunks: usize) {
        let Ok(chunk) = Chunk::parse(payload, max_chunks) else {
            warn!("ignored a malformed broadcast notification");
            return;
        };
        if chunk.origin == self.inner.origin {
            return; // Already delivered locally by publish.
        }
        let assembled = match assembler.add(chunk, Instant::now()) {
            Ok(Some(assembled)) => assembled,
            Ok(None) => return,
            Err(_) => {
                warn!("dropped a broadcast message with inconsistent chunks");
                return;
            }
        };
        let Ok(channel) = Channel::parse_key(&assembled.key) else {
            warn!("ignored a broadcast notification for an invalid channel");
            return;
        };
        let Ok(text) = String::from_utf8(assembled.data) else {
            warn!("ignored a broadcast notification that is not UTF-8");
            return;
        };
        if let Err(failure) = self.inner.local.deliver(&channel, &Message::new(text))
            && !self.inner.local.is_closed()
        {
            warn!(error = %failure, "could not deliver a broadcast message");
        }
    }

    fn chunks(
        &self,
        origin: &str,
        channel: &Channel,
        message: &Message,
    ) -> Result<Vec<Chunk>, BroadcastError> {
        Ok(split(
            origin,
            &random_hex()?,
            &channel.key(),
            message.as_str().as_bytes(),
        ))
    }

    async fn notify(
        &self,
        connection: &mut PgConnection,
        chunks: &[Chunk],
    ) -> Result<(), BroadcastError> {
        for chunk in chunks {
            let payload = chunk.encode();
            if payload.len() > MAX_NOTIFY_BYTES {
                return Err(BroadcastError::MessageTooLarge);
            }
            sqlx::query("SELECT pg_notify($1, $2)")
                .bind(&self.inner.notify_channel)
                .bind(payload)
                .execute(&mut *connection)
                .await
                .map_err(BroadcastError::Notify)?;
        }
        Ok(())
    }
}

/// How one listening attempt ended.
enum Listened {
    /// It never listened.
    Failed,
    /// It listened, then lost the connection.
    Connected,
}

#[async_trait]
impl Broker for PgBroker {
    /// Delivers to this replica's subscribers immediately and to every other
    /// replica through `NOTIFY`, all chunks in one transaction. If the
    /// notification fails the error is returned although local subscribers
    /// already have the message.
    async fn publish(&self, channel: &Channel, message: Message) -> Result<(), BroadcastError> {
        if message.len() > self.inner.max_bytes {
            return Err(BroadcastError::MessageTooLarge);
        }
        self.inner.local.deliver(channel, &message)?;
        let chunks = self.chunks(&self.inner.origin, channel, &message)?;
        let mut transaction = self
            .inner
            .database
            .pool()
            .begin()
            .await
            .map_err(BroadcastError::Notify)?;
        self.notify(&mut transaction, &chunks).await?;
        transaction.commit().await.map_err(BroadcastError::Notify)
    }

    fn subscribe(&self, channel: &Channel) -> Result<Subscription, BroadcastError> {
        self.inner.local.subscribe(channel)
    }
}

/// `[a-z_][a-z0-9_]{0,62}`: an identifier that needs no quoting rules.
fn valid_notify_channel(name: &str) -> bool {
    let mut bytes = name.bytes();
    name.len() <= 63
        && bytes
            .next()
            .is_some_and(|first| first.is_ascii_lowercase() || first == b'_')
        && bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

fn random_hex() -> Result<String, BroadcastError> {
    let mut bytes = [0_u8; 8];
    getrandom::fill(&mut bytes).map_err(|_| BroadcastError::Random)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notify_channels_are_plain_lowercase_identifiers() {
        for valid in ["forge_broadcast", "_x", "a1", &"a".repeat(63)] {
            assert!(valid_notify_channel(valid), "{valid}");
        }
        for invalid in [
            "",
            "Forge",
            "1abc",
            "a-b",
            "a b",
            "a\"; DROP TABLE x; --",
            &"a".repeat(64),
        ] {
            assert!(!valid_notify_channel(invalid), "{invalid:?}");
        }
        assert_eq!(random_hex().expect("random").len(), 16);
    }
}
