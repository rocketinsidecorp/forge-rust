//! The message model and its validation.

use std::collections::{BTreeMap, HashSet};

use forge_config::{has_text_problem, is_bidi_control};
use serde::{Deserialize, Serialize};

use crate::{Address, MailError, encoding};

/// Extra headers a [`Message`] may set: ASCII, structured, and unable to
/// alter routing or MIME structure. `Bcc`, `To`, `Subject`, `Content-*`,
/// `Message-ID`, `Return-Path` and anything unknown are rejected.
pub const ALLOWED_HEADERS: &[&str] = &[
    "Auto-Submitted",
    "In-Reply-To",
    "List-Id",
    "List-Unsubscribe",
    "List-Unsubscribe-Post",
    "Precedence",
    "References",
];

const MAX_HEADER_VALUE_BYTES: usize = 900;
const MAX_REPLY_TO: usize = 10;
const MAX_CONTENT_ID_BYTES: usize = 128;
const MAX_FILENAME_BYTES: usize = 255;

/// One outgoing email.
///
/// It is plain data (serializable, so a job queue can carry it): every field
/// is validated when the message is encoded, which every [`Sender`]
/// does before any side effect, so a message built from untrusted input can
/// never inject headers, recipients or MIME parts.
///
/// [`Sender`]: crate::Sender
///
/// ```
/// use forge_mail::{Address, Message};
///
/// let message = Message::new(Address::new("Shop", "shop@example.com")?)
///     .to(Address::new("Ada", "ada@example.org")?)
///     .subject("Your receipt")
///     .text("Thanks for your order.")
///     .html("<p>Thanks for your order.</p>")
///     .header("List-Unsubscribe", "<https://example.com/u/42>");
/// assert!(message.validate().is_ok());
/// # Ok::<(), forge_mail::MailError>(())
/// ```
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct Message {
    /// The sender (header `From`, envelope `MAIL FROM`).
    pub from: Address,
    /// `Reply-To` addresses (at most 10).
    pub reply_to: Vec<Address>,
    /// `To` recipients.
    pub to: Vec<Address>,
    /// `Cc` recipients.
    pub cc: Vec<Address>,
    /// `Bcc` recipients: they receive the message but appear in no header.
    pub bcc: Vec<Address>,
    /// The subject (any text without control or direction-override
    /// characters).
    pub subject: String,
    /// The `text/plain` body. At least one of `text` and `html` is required;
    /// with both the message is `multipart/alternative`.
    pub text: String,
    /// The `text/html` body.
    pub html: String,
    /// Files, or inline images referenced from the HTML as `cid:` when
    /// [`Attachment::content_id`] is set.
    pub attachments: Vec<Attachment>,
    /// Extra headers from [`ALLOWED_HEADERS`] (names are case-insensitive).
    pub headers: BTreeMap<String, String>,
}

/// A file carried by a message.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct Attachment {
    /// Reduced to a base name; names with control or bidirectional-override
    /// characters are rejected.
    pub filename: String,
    /// Defaults to `application/octet-stream`; `multipart/*`, `message/*`
    /// and parameters are rejected.
    pub content_type: String,
    /// The bytes.
    pub data: Vec<u8>,
    /// When set, the attachment is inline and addressable as
    /// `<img src="cid:...">` from the HTML body. Letters, digits and `._-@`.
    pub content_id: String,
}

impl Attachment {
    /// A file attachment.
    #[must_use]
    pub fn new(
        filename: impl Into<String>,
        content_type: impl Into<String>,
        data: impl Into<Vec<u8>>,
    ) -> Self {
        Self {
            filename: filename.into(),
            content_type: content_type.into(),
            data: data.into(),
            content_id: String::new(),
        }
    }

    /// Makes the attachment an inline part addressable as `cid:<id>`.
    #[must_use]
    pub fn inline(mut self, content_id: impl Into<String>) -> Self {
        self.content_id = content_id.into();
        self
    }
}

/// Size bounds of a message.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Limits {
    /// Subject length in characters (default 255).
    pub max_subject_chars: usize,
    /// `To` + `Cc` + `Bcc` (default 100).
    pub max_recipients: usize,
    /// Each body (default 2 MiB).
    pub max_body_bytes: usize,
    /// Attachment count (default 10).
    pub max_attachments: usize,
    /// Each attachment (default 7 MiB).
    pub max_attachment_bytes: usize,
    /// Bodies and attachments together, before encoding (default 10 MiB,
    /// about 13.7 MB once base64 encoded).
    pub max_message_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_subject_chars: 255,
            max_recipients: 100,
            max_body_bytes: 2 << 20,
            max_attachments: 10,
            max_attachment_bytes: 7 << 20,
            max_message_bytes: 10 << 20,
        }
    }
}

impl Message {
    /// A message from `from`, with nothing else set.
    #[must_use]
    pub fn new(from: Address) -> Self {
        Self {
            from,
            ..Self::default()
        }
    }

    /// Adds a `To` recipient.
    #[must_use]
    pub fn to(mut self, address: Address) -> Self {
        self.to.push(address);
        self
    }

    /// Adds a `Cc` recipient.
    #[must_use]
    pub fn cc(mut self, address: Address) -> Self {
        self.cc.push(address);
        self
    }

    /// Adds a `Bcc` recipient (envelope only).
    #[must_use]
    pub fn bcc(mut self, address: Address) -> Self {
        self.bcc.push(address);
        self
    }

    /// Adds a `Reply-To` address.
    #[must_use]
    pub fn reply_to(mut self, address: Address) -> Self {
        self.reply_to.push(address);
        self
    }

    /// Sets the subject.
    #[must_use]
    pub fn subject(mut self, subject: impl Into<String>) -> Self {
        self.subject = subject.into();
        self
    }

    /// Sets the `text/plain` body.
    #[must_use]
    pub fn text(mut self, text: impl Into<String>) -> Self {
        self.text = text.into();
        self
    }

    /// Sets the `text/html` body.
    #[must_use]
    pub fn html(mut self, html: impl Into<String>) -> Self {
        self.html = html.into();
        self
    }

    /// Adds an attachment.
    #[must_use]
    pub fn attach(mut self, attachment: Attachment) -> Self {
        self.attachments.push(attachment);
        self
    }

    /// Sets an extra header from [`ALLOWED_HEADERS`].
    #[must_use]
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.insert(name.into(), value.into());
        self
    }

    /// The SMTP envelope recipients (`To`, `Cc`, `Bcc`), de-duplicated
    /// case-insensitively, in order of first appearance.
    #[must_use]
    pub fn recipients(&self) -> Vec<String> {
        let mut seen = HashSet::new();
        self.to
            .iter()
            .chain(&self.cc)
            .chain(&self.bcc)
            .filter(|address| seen.insert(address.email.to_ascii_lowercase()))
            .map(|address| address.email.clone())
            .collect()
    }

    /// Checks the message against the default [`Limits`].
    pub fn validate(&self) -> Result<(), MailError> {
        self.validate_with(&Limits::default())
    }

    /// Checks the message against `limits`. Errors name the field and the
    /// rule, never the offending value, so logs cannot be forged through
    /// them.
    pub fn validate_with(&self, limits: &Limits) -> Result<(), MailError> {
        self.from.validate_field("From")?;
        for (name, list) in [
            ("ReplyTo", &self.reply_to),
            ("To", &self.to),
            ("Cc", &self.cc),
            ("Bcc", &self.bcc),
        ] {
            for (index, address) in list.iter().enumerate() {
                address.validate_field(&format!("{name}[{index}]"))?;
            }
        }
        if self.reply_to.len() > MAX_REPLY_TO {
            return Err(MailError::invalid("ReplyTo", "has too many addresses"));
        }
        if self.recipients().is_empty() {
            return Err(MailError::invalid(
                "To",
                "at least one recipient is required",
            ));
        }
        if self.to.len() + self.cc.len() + self.bcc.len() > limits.max_recipients {
            return Err(MailError::invalid(
                "recipients",
                "exceed the configured limit",
            ));
        }
        if has_text_problem(&self.subject) {
            return Err(MailError::invalid(
                "Subject",
                "contains a control, line-break or direction-override character",
            ));
        }
        if self.subject.chars().count() > limits.max_subject_chars {
            return Err(MailError::invalid("Subject", "is too long"));
        }
        if self.text.is_empty() && self.html.is_empty() {
            return Err(MailError::invalid("Text", "or HTML body is required"));
        }
        if self.text.len() > limits.max_body_bytes || self.html.len() > limits.max_body_bytes {
            return Err(MailError::invalid("body", "exceeds the configured limit"));
        }
        validate_headers(&self.headers)?;
        self.validate_attachments(limits)
    }

    fn validate_attachments(&self, limits: &Limits) -> Result<(), MailError> {
        if self.attachments.len() > limits.max_attachments {
            return Err(MailError::invalid(
                "Attachments",
                "exceed the configured count",
            ));
        }
        let mut total = self.text.len() + self.html.len();
        let mut ids = HashSet::new();
        for (index, attachment) in self.attachments.iter().enumerate() {
            let field = format!("Attachments[{index}]");
            if sanitize_filename(&attachment.filename).is_err() {
                return Err(MailError::invalid(
                    format!("{field}.Filename"),
                    "is not acceptable",
                ));
            }
            if attachment_content_type(&attachment.content_type).is_none() {
                return Err(MailError::invalid(
                    format!("{field}.ContentType"),
                    "is not acceptable",
                ));
            }
            if attachment.data.is_empty() {
                return Err(MailError::invalid(format!("{field}.Data"), "is empty"));
            }
            if attachment.data.len() > limits.max_attachment_bytes {
                return Err(MailError::invalid(
                    format!("{field}.Data"),
                    "exceeds the configured limit",
                ));
            }
            total += attachment.data.len();
            if !attachment.content_id.is_empty() {
                if !valid_content_id(&attachment.content_id)
                    || !ids.insert(attachment.content_id.as_str())
                {
                    return Err(MailError::invalid(
                        format!("{field}.ContentID"),
                        "is invalid or repeated",
                    ));
                }
                if self.html.is_empty() {
                    return Err(MailError::invalid(
                        format!("{field}.ContentID"),
                        "requires an HTML body",
                    ));
                }
            }
        }
        if total > limits.max_message_bytes {
            return Err(MailError::invalid("message", "exceeds the configured size"));
        }
        Ok(())
    }
}

/// `textproto.CanonicalMIMEHeaderKey`: `list-id` → `List-Id`. Names with
/// characters outside a header token are returned unchanged.
pub(crate) fn canonical_header_name(name: &str) -> String {
    let token = !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte));
    if !token {
        return name.to_owned();
    }
    let mut upper = true;
    name.chars()
        .map(|character| {
            let out = if upper {
                character.to_ascii_uppercase()
            } else {
                character.to_ascii_lowercase()
            };
            upper = character == '-';
            out
        })
        .collect()
}

fn validate_headers(headers: &BTreeMap<String, String>) -> Result<(), MailError> {
    let mut seen = HashSet::new();
    for (name, value) in headers {
        let canonical = canonical_header_name(name);
        if !ALLOWED_HEADERS.contains(&canonical.as_str()) {
            return Err(MailError::invalid(
                "Headers",
                "contains a header that is not allowed",
            ));
        }
        if !seen.insert(canonical.clone()) {
            return Err(MailError::invalid("Headers", "repeats a header"));
        }
        if value.is_empty() || value.len() > MAX_HEADER_VALUE_BYTES {
            return Err(MailError::invalid(
                format!("Headers.{canonical}"),
                "value is empty or too long",
            ));
        }
        if !value.bytes().all(|byte| (0x20..=0x7e).contains(&byte)) {
            return Err(MailError::invalid(
                format!("Headers.{canonical}"),
                "value must be printable ASCII",
            ));
        }
    }
    Ok(())
}

fn valid_content_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_CONTENT_ID_BYTES
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-@".contains(&byte))
}

/// Reduces `name` to a safe base name: directory components
/// (`../../etc/passwd`, `C:\x\y.txt`) are dropped and leading dots and
/// surrounding spaces trimmed. Names with control characters, line breaks or
/// bidirectional overrides (which can disguise `fdp.exe` as `.pdf`) are
/// rejected rather than repaired, as is a name that is empty after
/// sanitising or longer than 255 bytes.
///
/// ```
/// use forge_mail::sanitize_filename;
///
/// assert_eq!(sanitize_filename("../../etc/passwd")?, "passwd");
/// assert_eq!(sanitize_filename("C:\\Users\\ada\\report.pdf")?, "report.pdf");
/// assert!(sanitize_filename("invoice\u{202E}fdp.exe").is_err());
/// # Ok::<(), forge_mail::MailError>(())
/// ```
pub fn sanitize_filename(name: &str) -> Result<String, MailError> {
    if name.is_empty() {
        return Err(MailError::invalid(
            "filename",
            "is empty or not valid UTF-8",
        ));
    }
    if name.chars().any(|character| {
        character.is_control()
            || is_bidi_control(character)
            || matches!(character, '\u{2028}' | '\u{2029}' | '\u{FFFD}')
    }) {
        return Err(MailError::invalid(
            "filename",
            "contains a control or direction-override character",
        ));
    }
    let slashed = name.replace('\\', "/");
    let base = slashed
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or_default();
    let trimmed = base.trim_matches(|character| character == ' ' || character == '.');
    if trimmed.is_empty() || trimmed.len() > MAX_FILENAME_BYTES {
        return Err(MailError::invalid(
            "filename",
            "is empty or too long after sanitising",
        ));
    }
    Ok(trimmed.to_owned())
}

/// The canonical media type of an attachment, or `None` when it is
/// malformed, has parameters or is `multipart/*` or `message/*`.
pub(crate) fn attachment_content_type(value: &str) -> Option<String> {
    if value.is_empty() {
        return Some("application/octet-stream".to_owned());
    }
    let media_type = encoding::parse_media_type(value)?;
    if media_type.starts_with("multipart/") || media_type.starts_with("message/") {
        return None;
    }
    Some(media_type)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Message {
        Message::new(Address::new("Forge App", "app@example.com").expect("valid"))
            .to(Address::new("Alice", "alice@example.org").expect("valid"))
            .subject("Welcome")
            .text("Hello")
    }

    fn invalid_field(message: &Message) -> String {
        match message.validate().expect_err("invalid") {
            MailError::Invalid { field, .. } | MailError::AddressUnsupported { field, .. } => field,
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn header_injection_is_refused_everywhere() {
        let mut message = base();
        message.subject = "Hi\r\nBcc: eve@example.com".to_owned();
        assert_eq!(invalid_field(&message), "Subject");
        for subject in [
            "a\nb",
            "a\rb",
            "a\u{0}b",
            "a\u{2028}b",
            "a\u{202E}b",
            "a\u{85}b",
        ] {
            let mut message = base();
            message.subject = subject.to_owned();
            assert_eq!(invalid_field(&message), "Subject", "{subject:?}");
        }
        let mut message = base();
        message.to[0].name = "Alice\r\nBcc: eve@example.com".to_owned();
        assert_eq!(invalid_field(&message), "To[0]");
        let mut message = base();
        message.cc.push(Address {
            name: String::new(),
            email: "x@example.com>\r\nRCPT TO:<eve@example.com".to_owned(),
        });
        assert_eq!(invalid_field(&message), "Cc[0]");
        let message = base().header("List-Id", "<list.example.com>\r\nBcc: eve@example.com");
        assert_eq!(invalid_field(&message), "Headers.List-Id");
        let message = base().header("List-Id", "café");
        assert_eq!(invalid_field(&message), "Headers.List-Id");
    }

    #[test]
    fn only_allow_listed_headers_once() {
        for name in [
            "Bcc",
            "To",
            "Subject",
            "Content-Type",
            "Message-ID",
            "Return-Path",
            "X-Custom",
            "Bad Name",
        ] {
            let message = base().header(name, "value");
            assert_eq!(invalid_field(&message), "Headers", "{name}");
        }
        let message = base().header("list-id", "<a>").header("List-Id", "<b>");
        assert_eq!(invalid_field(&message), "Headers");
        let message = base().header("list-unsubscribe", "<https://example.com/u>");
        assert!(message.validate().is_ok());
        assert_eq!(
            invalid_field(&base().header("Precedence", "")),
            "Headers.Precedence"
        );
    }

    #[test]
    fn structure_and_limits() {
        let mut message = base();
        message.to.clear();
        assert_eq!(invalid_field(&message), "To");
        let mut message = base();
        message.text.clear();
        assert_eq!(invalid_field(&message), "Text");
        let mut message = base();
        message.subject = "s".repeat(256);
        assert_eq!(invalid_field(&message), "Subject");
        let mut message = base();
        message.reply_to = vec![message.from.clone(); 11];
        assert_eq!(invalid_field(&message), "ReplyTo");
        let mut message = base();
        message.bcc = vec![message.from.clone(); 100];
        assert_eq!(invalid_field(&message), "recipients");
        let mut message = base();
        message.text = "x".repeat((2 << 20) + 1);
        assert_eq!(invalid_field(&message), "body");
        let mut message = base();
        message.attachments = vec![Attachment::new("a.txt", "text/plain", "x"); 11];
        assert_eq!(invalid_field(&message), "Attachments");
        let message = base().attach(Attachment::new("a.bin", "", vec![0; (7 << 20) + 1]));
        assert_eq!(invalid_field(&message), "Attachments[0].Data");
        let message = base()
            .attach(Attachment::new("a.bin", "", vec![0; 6 << 20]))
            .attach(Attachment::new("b.bin", "", vec![0; 6 << 20]));
        assert_eq!(invalid_field(&message), "message");
        let message = base().attach(Attachment::new("a.bin", "", Vec::new()));
        assert_eq!(invalid_field(&message), "Attachments[0].Data");
        let limits = Limits {
            max_recipients: 1,
            ..Limits::default()
        };
        assert!(base().cc(base().from).validate_with(&limits).is_err());
    }

    #[test]
    fn attachments_are_checked() {
        for content_type in [
            "multipart/mixed",
            "message/rfc822",
            "text/plain; charset=x",
            "a b",
        ] {
            let message = base().attach(Attachment::new("a", content_type, "x"));
            assert_eq!(
                invalid_field(&message),
                "Attachments[0].ContentType",
                "{content_type}"
            );
        }
        for filename in ["", "..", "invoice\u{202E}fdp.exe", "a\r\nb", "/"] {
            let message = base().attach(Attachment::new(filename, "", "x"));
            assert_eq!(
                invalid_field(&message),
                "Attachments[0].Filename",
                "{filename:?}"
            );
        }
        let message = base().attach(Attachment::new("logo.png", "image/png", "x").inline("logo"));
        assert_eq!(invalid_field(&message), "Attachments[0].ContentID");
        let message = base()
            .html("<img src=\"cid:logo\">")
            .attach(Attachment::new("logo.png", "image/png", "x").inline("lo go"));
        assert_eq!(invalid_field(&message), "Attachments[0].ContentID");
        let message = base()
            .html("<img src=\"cid:logo\">")
            .attach(Attachment::new("a.png", "image/png", "x").inline("logo"))
            .attach(Attachment::new("b.png", "image/png", "x").inline("logo"));
        assert_eq!(invalid_field(&message), "Attachments[1].ContentID");
        assert_eq!(sanitize_filename("dir/").as_deref(), Ok("dir"));
        assert_eq!(sanitize_filename(" .hidden ").as_deref(), Ok("hidden"));
    }

    #[test]
    fn recipients_are_deduplicated_case_insensitively() {
        let message = base()
            .cc(Address::new("", "ALICE@example.org").expect("valid"))
            .bcc(Address::new("", "bob@example.org").expect("valid"));
        assert_eq!(
            message.recipients(),
            ["alice@example.org", "bob@example.org"]
        );
    }

    #[test]
    fn messages_round_trip_through_json() {
        let message = base()
            .html("<p>Hi</p>")
            .attach(Attachment::new("a.txt", "text/plain", "x"));
        let json = serde_json::to_string(&message).expect("serializable");
        let back: Message = serde_json::from_str(&json).expect("deserializable");
        assert_eq!(back, message);
    }

    #[test]
    fn canonical_header_names() {
        assert_eq!(
            canonical_header_name("list-unsubscribe-post"),
            "List-Unsubscribe-Post"
        );
        assert_eq!(canonical_header_name("IN-REPLY-TO"), "In-Reply-To");
        assert_eq!(canonical_header_name("bad name"), "bad name");
    }
}
