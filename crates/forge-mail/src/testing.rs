//! An in-memory [`Sender`] for application tests, the equivalent of
//! `ActionMailer::Base.deliveries` and Forge-Go's `mailtest.Recorder`.
//!
//! The recorder runs the real encoder on every message, so a test fails on
//! exactly the messages production would refuse (header injection, missing
//! recipients, oversized attachments) rather than recording invalid mail.

use std::{
    sync::{Mutex, MutexGuard, PoisonError},
    time::SystemTime,
};

use async_trait::async_trait;

use crate::{MailError, Message, Sender, encode};

/// One recorded message.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Delivery {
    /// A copy of what was sent.
    pub message: Message,
    /// The envelope sender.
    pub sender: String,
    /// The envelope recipients, `Bcc` included.
    pub recipients: Vec<String>,
    /// `<uuidv7@domain>`.
    pub message_id: String,
    /// The exact RFC 5322 bytes the SMTP sender would transmit.
    pub raw: Vec<u8>,
    /// When it was recorded.
    pub at: SystemTime,
}

impl Delivery {
    /// The unfolded value of the first header called `name`
    /// (case-insensitive), read from [`Delivery::raw`].
    #[must_use]
    pub fn header(&self, name: &str) -> Option<String> {
        let text = String::from_utf8_lossy(&self.raw);
        let head = text.split("\r\n\r\n").next().unwrap_or_default();
        let mut found: Option<String> = None;
        for line in head.split("\r\n") {
            if line.starts_with([' ', '\t']) {
                if let Some(value) = found.as_mut() {
                    value.push_str(line);
                }
                continue;
            }
            if found.is_some() {
                break;
            }
            if let Some((key, value)) = line.split_once(':')
                && key.eq_ignore_ascii_case(name)
            {
                found = Some(value.trim_start().to_owned());
            }
        }
        found
    }
}

/// Collects deliveries; safe for concurrent use.
#[derive(Debug, Default)]
pub struct Recorder {
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    deliveries: Vec<Delivery>,
    failure: Option<MailError>,
}

impl Recorder {
    /// An empty recorder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Everything recorded, oldest first.
    #[must_use]
    pub fn deliveries(&self) -> Vec<Delivery> {
        self.state().deliveries.clone()
    }

    /// How many messages were recorded.
    #[must_use]
    pub fn len(&self) -> usize {
        self.state().deliveries.len()
    }

    /// Whether nothing was recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The most recent delivery.
    #[must_use]
    pub fn last(&self) -> Option<Delivery> {
        self.state().deliveries.last().cloned()
    }

    /// Makes every following send fail with `failure` without recording
    /// (an outage); `None` restores normal behaviour.
    pub fn fail_with(&self, failure: Option<MailError>) {
        self.state().failure = failure;
    }

    /// Forgets all deliveries and any configured failure.
    pub fn reset(&self) {
        *self.state() = State::default();
    }
}

#[async_trait]
impl Sender for Recorder {
    async fn send(&self, message: &Message) -> Result<(), MailError> {
        let encoded = encode(message)?;
        let mut state = self.state();
        if let Some(failure) = &state.failure {
            return Err(failure.clone());
        }
        state.deliveries.push(Delivery {
            message: message.clone(),
            sender: encoded.sender,
            recipients: encoded.recipients,
            message_id: encoded.message_id,
            raw: encoded.data,
            at: SystemTime::now(),
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Address;

    fn message() -> Message {
        Message::new(Address::new("App", "app@example.com").expect("valid"))
            .to(Address::new("Ada", "ada@example.org").expect("valid"))
            .subject("A subject long enough to be folded when it is written, as headers are at seventy-six")
            .text("Body")
    }

    #[tokio::test]
    async fn records_encoded_messages_and_simulates_outages() {
        let recorder = Recorder::new();
        assert!(recorder.is_empty());
        recorder.send(&message()).await.expect("recorded");
        let delivery = recorder.last().expect("one");
        assert_eq!(delivery.recipients, ["ada@example.org"]);
        assert_eq!(
            delivery.header("to").as_deref(),
            Some("\"Ada\" <ada@example.org>")
        );
        assert_eq!(
            delivery.header("Subject").as_deref(),
            Some(message().subject.as_str())
        );
        assert_eq!(delivery.header("X-Missing"), None);

        let mut bad = message();
        bad.subject = "a\nb".to_owned();
        assert!(recorder.send(&bad).await.is_err());
        recorder.fail_with(Some(MailError::Unavailable {
            stage: "connect",
            reply: None,
            timeout: false,
        }));
        assert!(
            recorder
                .send(&message())
                .await
                .expect_err("outage")
                .is_retryable()
        );
        assert_eq!(recorder.len(), 1);
        recorder.reset();
        assert!(recorder.is_empty());
        recorder.send(&message()).await.expect("recorded again");
        assert_eq!(recorder.deliveries().len(), 1);
    }
}
