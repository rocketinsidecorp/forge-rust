//! Email for Forge applications, the counterpart of Rails' Action Mailer
//! and the port of Forge-Go's `mail` package (ADR 0035, docs/mail.md).
//!
//! A [`Message`] is plain data that [`encode`] validates and renders as
//! MIME, so header injection, recipient smuggling and malformed MIME are
//! rejected before any byte reaches a transport. Delivery goes through the
//! [`Sender`] port:
//!
//! - [`SmtpSender`]: STARTTLS (587) or implicit TLS (465), certificates
//!   always verified, credentials only over TLS, bounded deadlines;
//! - [`FileSender`]: development `.eml` files, refused in production;
//! - `testing::Recorder` (feature `testing`): in-memory deliveries for
//!   tests, like `ActionMailer::Base.deliveries`.
//!
//! [`from_config`] builds the sender `FORGE_MAIL_URL` selects.
//! [`Templates`] renders bodies from askama templates, [`PreviewHandler`]
//! shows messages in the browser during development, [`Instrumented`]
//! records `forge.mail.send.duration`, and [`PasswordResetMailer`] delivers
//! password reset links for `forge-auth`.

mod address;
mod encode;
mod encoding;
mod error;
mod factory;
mod file;
mod instrument;
mod message;
mod password_reset;
mod preview;
mod sender;
mod smtp;
mod template;
#[cfg(feature = "testing")]
pub mod testing;

pub use address::Address;
pub use encode::{Encoded, Encoder, encode};
pub use error::MailError;
pub use factory::{default_from, from_config};
pub use file::FileSender;
pub use instrument::{Instrumented, instrument};
pub use message::{ALLOWED_HEADERS, Attachment, Limits, Message, sanitize_filename};
pub use password_reset::{
    PasswordResetEmail, PasswordResetEmails, PasswordResetMailer, password_reset_notifier,
};
pub use preview::{Preview, PreviewHandler};
pub use sender::Sender;
pub use smtp::{SmtpConfig, SmtpSender};
pub use template::{Rendered, Templates};
