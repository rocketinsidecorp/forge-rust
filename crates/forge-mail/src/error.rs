//! Mail errors. They never contain credentials, recipient addresses, header
//! values or server reply text: only the field and rule, the SMTP stage and
//! the numeric reply code.

use thiserror::Error;

/// Why a message was not built, validated or delivered.
///
/// [`MailError::code`] is Forge-Go's fault code (`mail_invalid`,
/// `mail_unavailable`, ...), used as the `forge.mail.outcome` metric label
/// and in log lines. Only [`MailError::Unavailable`] is worth retrying.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
#[non_exhaustive]
pub enum MailError {
    /// The message is invalid; `field` names the field (`To[0]`,
    /// `Subject`, `Headers.List-Id`), never its value.
    #[error("invalid email message: {field} {reason}")]
    Invalid {
        /// The offending field.
        field: String,
        /// The rule it breaks.
        reason: &'static str,
    },
    /// An address Forge cannot deliver to: non-ASCII characters (SMTPUTF8
    /// and IDNA are not supported). It is never converted to punycode.
    #[error("email address is not supported: {field} {reason}")]
    AddressUnsupported {
        /// The offending field.
        field: String,
        /// The rule it breaks.
        reason: &'static str,
    },
    /// A transient failure (network, timeout, 4xx reply). Retrying later
    /// may succeed.
    #[error("mail delivery is temporarily unavailable (stage {stage}{})", reply_suffix(*reply))]
    Unavailable {
        /// SMTP stage: `connect`, `greeting`, `ehlo`, `starttls`, `auth`,
        /// `mail`, `rcpt`, `data` or `file`.
        stage: &'static str,
        /// The server's reply code, if it answered.
        reply: Option<u16>,
        /// Whether the delivery deadline expired.
        timeout: bool,
    },
    /// A permanent refusal (5xx reply, refused recipient). Retrying the same
    /// message will not help.
    #[error("mail server rejected the message (stage {stage}{})", reply_suffix(*reply))]
    Rejected {
        /// SMTP stage.
        stage: &'static str,
        /// The server's reply code.
        reply: Option<u16>,
    },
    /// The credentials were refused, or no safe way to send them exists
    /// (no TLS, no supported mechanism).
    #[error("mail server authentication failed (stage {stage}{})", reply_suffix(*reply))]
    AuthFailed {
        /// SMTP stage.
        stage: &'static str,
        /// The server's reply code, if it answered.
        reply: Option<u16>,
    },
    /// The server does not offer STARTTLS and plaintext is not allowed;
    /// nothing was sent.
    #[error("mail server does not offer TLS")]
    TlsRequired,
    /// The TLS handshake or certificate verification failed.
    #[error("mail server TLS handshake failed (stage {stage})")]
    TlsFailed {
        /// `connect` (implicit TLS) or `starttls`.
        stage: &'static str,
    },
    /// A template failed to render; nothing was sent.
    #[error("email template failed to render ({part})")]
    Template {
        /// `subject`, `text`, `html` or `body`.
        part: &'static str,
    },
    /// The development file sender could not store the message.
    #[error("could not write the development email file ({kind})")]
    File {
        /// The I/O error kind.
        kind: std::io::ErrorKind,
    },
    /// `FORGE_MAIL_URL` is not set: features that need mail stay disabled.
    #[error("FORGE_MAIL_URL is not configured")]
    NotConfigured,
    /// A sender, handler or adapter was configured wrongly.
    #[error("mail configuration: {0}")]
    Configuration(String),
}

fn reply_suffix(reply: Option<u16>) -> String {
    reply.map_or_else(String::new, |code| format!(", reply {code}"))
}

impl MailError {
    /// Forge-Go's fault code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Invalid { .. } => "mail_invalid",
            Self::AddressUnsupported { .. } => "mail_address_unsupported",
            Self::Unavailable { .. } => "mail_unavailable",
            Self::Rejected { .. } => "mail_rejected",
            Self::AuthFailed { .. } => "mail_auth_failed",
            Self::TlsRequired => "mail_tls_required",
            Self::TlsFailed { .. } => "mail_tls_failed",
            Self::Template { .. } => "mail_template",
            Self::File { .. } => "mail_file_failed",
            Self::NotConfigured => "mail_not_configured",
            Self::Configuration(_) => "mail_configuration",
        }
    }

    /// Whether retrying the same message later may succeed.
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        matches!(self, Self::Unavailable { .. })
    }

    /// The SMTP stage the error happened at, if any.
    #[must_use]
    pub const fn stage(&self) -> Option<&'static str> {
        match self {
            Self::Unavailable { stage, .. }
            | Self::Rejected { stage, .. }
            | Self::AuthFailed { stage, .. }
            | Self::TlsFailed { stage } => Some(stage),
            Self::TlsRequired => Some("starttls"),
            _ => None,
        }
    }

    /// The server's numeric reply code, if it answered.
    #[must_use]
    pub const fn reply_code(&self) -> Option<u16> {
        match self {
            Self::Unavailable { reply, .. }
            | Self::Rejected { reply, .. }
            | Self::AuthFailed { reply, .. } => *reply,
            _ => None,
        }
    }

    pub(crate) fn invalid(field: impl Into<String>, reason: &'static str) -> Self {
        Self::Invalid {
            field: field.into(),
            reason,
        }
    }
}
