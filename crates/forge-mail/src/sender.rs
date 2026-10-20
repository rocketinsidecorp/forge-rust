//! The delivery port.

use std::sync::Arc;

use async_trait::async_trait;

use crate::{MailError, Message};

/// Delivers one message: the port application code depends on.
///
/// Implementations must be safe for concurrent use, validate the message
/// (encoding does) before any side effect, and stop when the future is
/// dropped. `Ok` means the message was accepted for delivery, not that it
/// reached the inbox. A queue that delivers later is just another `Sender`
/// (docs/mail.md).
#[async_trait]
pub trait Sender: Send + Sync {
    /// Sends `message`.
    async fn send(&self, message: &Message) -> Result<(), MailError>;
}

impl std::fmt::Debug for dyn Sender {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("dyn Sender")
    }
}

#[async_trait]
impl<T: Sender + ?Sized> Sender for Arc<T> {
    async fn send(&self, message: &Message) -> Result<(), MailError> {
        (**self).send(message).await
    }
}

#[async_trait]
impl<T: Sender + ?Sized> Sender for Box<T> {
    async fn send(&self, message: &Message) -> Result<(), MailError> {
        (**self).send(message).await
    }
}
