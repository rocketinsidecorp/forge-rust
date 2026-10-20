//! Live page updates (ADR 0031): Turbo Streams pushed to browsers over
//! Server-Sent Events, the counterpart of Rails' `turbo_stream_from` and
//! Action Cable broadcasts, with the same names and wire formats as
//! Forge-Go's `broadcast` package.
//!
//! - A page subscribes by rendering [`Signer::turbo_stream_from`]: a
//!   `<turbo-stream-source>` whose URL carries a signed, expiring
//!   [`Channel`] name (HMAC-SHA256). A browser can only subscribe to channels
//!   the server rendered for it.
//! - [`EventStreams`] serves `GET /streams?stream=TOKEN`: it verifies the
//!   token, asks the application's [`StreamAccess`] who the caller is, requires
//!   a tenant channel's tenant to be the caller's authorized tenant
//!   (fail-closed), caps open streams per account and per tenant, and streams
//!   `text/event-stream` with heartbeats.
//! - Server code publishes with [`publish`] after the change it describes is
//!   stored; [`MemoryBroker`] delivers within one process and, with the
//!   `postgres` feature, `postgres::PgBroker` fans out to every replica with
//!   `LISTEN/NOTIFY`.
//!
//! Delivery is best effort and at most once: there is no replay, a
//! subscriber that cannot keep up is dropped (the browser reconnects), and
//! messages published while a browser is disconnected are not delivered.
//! Turbo pages re-render on navigation, so a gap heals on the next visit.
//!
//! Every task has an owner: the broker starts none (publishing is a
//! non-blocking send to bounded per-subscriber queues), each event stream is
//! the body of its own HTTP response on the connection's task, and the
//! PostgreSQL listener is a future the application runs as a lifecycle
//! worker. The broker closes when the lifecycle's shutdown begins, which
//! ends every open stream so the HTTP server can drain.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

mod channel;
mod handler;
mod memory;
#[cfg(feature = "postgres")]
pub mod postgres;
mod signer;

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use bytes::Bytes;
use forge_views::{TurboStreamAction, turbo_stream_html};
use thiserror::Error;

pub use channel::{Channel, MAX_CHANNEL_NAME_BYTES};
pub use handler::{
    ClientAddress, DEFAULT_HEARTBEAT, DEFAULT_MAX_CONNECTION_AGE, DEFAULT_MAX_PER_PRINCIPAL,
    DEFAULT_MAX_PER_TENANT, DEFAULT_RETRY, EventStreams, StreamAccess, StreamDenied, StreamViewer,
    client_ip_address, peer_address,
};
pub use memory::{
    DEFAULT_BUFFER, DEFAULT_MAX_MESSAGE_BYTES, DEFAULT_MAX_SUBSCRIBERS,
    DEFAULT_MAX_SUBSCRIBERS_PER_CHANNEL, MemoryBroker, MemoryOptions, Subscription,
    SubscriptionEnd,
};
pub use signer::{
    DEFAULT_STREAMS_PATH, DEFAULT_TTL, MAX_TTL, MIN_KEY_BYTES, STREAM_PARAMETER, Signer,
};

/// Why a broadcast operation failed.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum BroadcastError {
    /// A channel name or tenant is malformed.
    #[error(
        "invalid channel: names are 1-96 bytes of [a-z0-9_.:-] starting with a letter or digit"
    )]
    InvalidChannel,
    /// A stream token is malformed, forged or signed with an unknown key.
    #[error("invalid stream token")]
    InvalidToken,
    /// A stream token is authentic but expired.
    #[error("stream token expired")]
    ExpiredToken,
    /// The broker was shut down.
    #[error("broadcast broker closed")]
    Closed,
    /// A subscriber limit was reached.
    #[error("too many subscribers")]
    TooManySubscribers,
    /// A message exceeds the broker's size limit.
    #[error("broadcast message too large")]
    MessageTooLarge,
    /// [`publish`] was called without streams.
    #[error("at least one turbo stream is required")]
    NoStreams,
    /// Cross-replica fanout was asked for on a backend without
    /// `LISTEN/NOTIFY`.
    #[error(
        "LISTEN/NOTIFY fanout requires PostgreSQL; the {backend} backend is refused rather than \
         emulated (ADR 0031): use the in-process broker, which reaches one process only"
    )]
    UnsupportedBackend {
        /// The declared backend.
        backend: &'static str,
    },
    /// A signer or broker setting is invalid.
    #[error("invalid broadcast configuration: {0}")]
    Configuration(&'static str),
    /// The operating system's random number generator failed.
    #[error("could not generate random bytes")]
    Random,
    /// Sending a PostgreSQL notification failed.
    #[cfg(feature = "postgres")]
    #[error("could not send the broadcast notification")]
    Notify(#[source] sqlx::Error),
}

/// One payload for the subscribers of a channel: the markup of one or more
/// `<turbo-stream>` elements. Cloning is cheap; the Server-Sent Events
/// framing is computed once and shared by every subscriber.
#[derive(Clone, Debug)]
pub struct Message {
    data: Arc<str>,
    event: Arc<OnceLock<Bytes>>,
}

impl Message {
    /// A message carrying `data`.
    #[must_use]
    pub fn new(data: impl Into<Arc<str>>) -> Self {
        Self {
            data: data.into(),
            event: Arc::new(OnceLock::new()),
        }
    }

    /// The payload.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.data
    }

    /// Payload length in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// Whether the payload is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// The payload as one Server-Sent Event.
    pub(crate) fn event(&self) -> Bytes {
        self.event
            .get_or_init(|| Bytes::from(sse_event(&self.data)))
            .clone()
    }
}

impl PartialEq for Message {
    fn eq(&self, other: &Self) -> bool {
        self.data == other.data
    }
}

impl Eq for Message {}

/// Fans messages out to subscribers.
///
/// Implementations bound their memory and never let a slow subscriber block
/// a publisher.
#[async_trait]
pub trait Broker: Send + Sync {
    /// Delivers `message` to the current subscribers of `channel`. It never
    /// waits for subscribers; with none it does nothing.
    async fn publish(&self, channel: &Channel, message: Message) -> Result<(), BroadcastError>;

    /// Starts receiving messages published to `channel` from now on.
    fn subscribe(&self, channel: &Channel) -> Result<Subscription, BroadcastError>;
}

/// Renders `actions` and publishes them to `channel`: the usual way to
/// broadcast, after the change the actions describe has been stored.
///
/// The same markup goes to every subscriber, so it must not contain anything
/// specific to the publishing request (CSRF tokens, the current user). Build
/// the templates with views that take no request.
pub async fn publish(
    broker: &dyn Broker,
    channel: &Channel,
    actions: &[TurboStreamAction],
) -> Result<(), BroadcastError> {
    if actions.is_empty() {
        return Err(BroadcastError::NoStreams);
    }
    broker
        .publish(channel, Message::new(turbo_stream_html(actions)))
        .await
}

/// Refuses cross-replica fanout on every backend but PostgreSQL: MySQL,
/// MariaDB and SQLite have no `LISTEN/NOTIFY`, and Forge does not emulate it
/// (by polling a table, for example).
///
/// ```
/// use forge_config::DatabaseScheme;
///
/// assert!(forge_broadcast::ensure_fanout_supported(DatabaseScheme::Postgres).is_ok());
/// let refused = forge_broadcast::ensure_fanout_supported(DatabaseScheme::Sqlite)
///     .expect_err("SQLite has no LISTEN/NOTIFY");
/// assert!(refused.to_string().contains("refused rather than emulated"));
/// ```
pub fn ensure_fanout_supported(scheme: forge_config::DatabaseScheme) -> Result<(), BroadcastError> {
    match scheme {
        forge_config::DatabaseScheme::Postgres => Ok(()),
        other => Err(BroadcastError::UnsupportedBackend {
            backend: other.name(),
        }),
    }
}

/// Formats `data` as one Server-Sent Event. Every line, split on any line
/// terminator the SSE parser recognizes (`\n`, `\r\n`, `\r`), becomes its own
/// `data:` field, so a payload cannot end the event early, add `id:` or
/// `event:` fields, or smuggle comments.
fn sse_event(data: &str) -> String {
    let mut out = String::with_capacity(data.len() + 16);
    let mut rest = data;
    loop {
        match rest.find(['\r', '\n']) {
            Some(index) => {
                out.push_str("data: ");
                out.push_str(&rest[..index]);
                out.push('\n');
                let skip = if rest[index..].starts_with("\r\n") {
                    2
                } else {
                    1
                };
                rest = &rest[index + skip..];
            }
            None => {
                out.push_str("data: ");
                out.push_str(rest);
                out.push_str("\n\n");
                return out;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_frame_every_line_and_cannot_be_ended_early() {
        assert_eq!(sse_event("one"), "data: one\n\n");
        assert_eq!(sse_event(""), "data: \n\n");
        assert_eq!(
            sse_event("a\nb\r\nc\rd"),
            "data: a\ndata: b\ndata: c\ndata: d\n\n"
        );
        // An attacker-controlled payload cannot inject fields or end the
        // event: every line is prefixed with "data: ".
        let framed = sse_event("x\n\nid: 7\nevent: evil\n: comment\r\rretry: 1");
        for line in framed.trim_end_matches('\n').split('\n') {
            assert!(line.starts_with("data: "), "{framed:?}");
        }
        assert!(framed.ends_with("\n\n") && !framed[..framed.len() - 2].contains("\n\n"));
    }

    #[test]
    fn messages_share_their_framing() {
        let message = Message::new("<turbo-stream></turbo-stream>\n");
        let copy = message.clone();

        assert_eq!(
            message.event(),
            Bytes::from_static(b"data: <turbo-stream></turbo-stream>\ndata: \n\n")
        );
        assert_eq!(copy, message);
        assert_eq!(message.len(), 30);
        assert!(!message.is_empty());
        assert_eq!(message.as_str(), copy.as_str());
    }
}
