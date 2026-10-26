//! RFC 5322 / MIME encoding.

use std::time::SystemTime;

use uuid::Uuid;

use crate::{
    Limits, MailError, Message,
    encoding::{self, base64_lines, fold_header, format_media_type, quoted_printable},
    message::{Attachment, attachment_content_type, canonical_header_name, sanitize_filename},
};

/// A message ready for transport: the RFC 5322 bytes plus the SMTP
/// envelope, which differs from the headers (`Bcc` recipients appear only in
/// the envelope).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Encoded {
    /// `<uuidv7@sender-domain>`, angle brackets included.
    pub message_id: String,
    /// The envelope sender (`MAIL FROM`), the `From` address.
    pub sender: String,
    /// The envelope recipients (`RCPT TO`), de-duplicated.
    pub recipients: Vec<String>,
    /// The CRLF-terminated message. It never contains a `Bcc` header.
    pub data: Vec<u8>,
}

/// Validates and encodes messages. [`encode`] uses the defaults; the
/// setters exist for size limits and for deterministic tests.
#[derive(Clone, Debug, Default)]
pub struct Encoder {
    limits: Limits,
    date: Option<SystemTime>,
    message_id: Option<Uuid>,
    boundary_entropy: Option<Vec<u8>>,
}

impl Encoder {
    /// An encoder with the default [`Limits`], the current time, a fresh
    /// UUIDv7 and boundaries from the operating-system CSPRNG.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the size limits.
    #[must_use]
    pub const fn limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// Fixes the `Date` header (tests).
    #[must_use]
    pub const fn date(mut self, date: SystemTime) -> Self {
        self.date = Some(date);
        self
    }

    /// Fixes the UUID of the `Message-ID` (tests).
    #[must_use]
    pub const fn message_id(mut self, id: Uuid) -> Self {
        self.message_id = Some(id);
        self
    }

    /// Takes MIME boundary randomness from `bytes`, 16 bytes per boundary,
    /// instead of the CSPRNG (tests). Encoding fails when it runs out.
    #[must_use]
    pub fn boundary_entropy(mut self, bytes: Vec<u8>) -> Self {
        self.boundary_entropy = Some(bytes);
        self
    }

    /// Validates `message` and renders it.
    ///
    /// Structure, from simplest to richest: `text/plain`; `text/html`;
    /// `multipart/alternative` (text first, HTML last); inline images in
    /// `multipart/related` around the HTML part; file attachments in
    /// `multipart/mixed` around the body. Bodies are quoted-printable UTF-8
    /// and attachments base64, so the output is 7-bit clean with CRLF line
    /// ends. `Date` is UTC, `Message-ID` a UUIDv7 at the sender's domain,
    /// non-ASCII names and subjects are RFC 2047 encoded and long headers are
    /// folded below 78 columns.
    pub fn encode(&self, message: &Message) -> Result<Encoded, MailError> {
        message.validate_with(&self.limits)?;
        let id = self.message_id.unwrap_or_else(Uuid::now_v7);
        let message_id = format!("<{id}@{}>", message.from.domain().to_ascii_lowercase());
        let mut entropy = self.boundary_entropy.clone().map(|bytes| bytes.into_iter());
        let mut boundary = || -> Result<String, MailError> {
            let mut raw = [0_u8; 16];
            match entropy.as_mut() {
                Some(bytes) => {
                    for slot in &mut raw {
                        *slot = bytes.next().ok_or_else(|| {
                            MailError::Configuration("boundary entropy exhausted".to_owned())
                        })?;
                    }
                }
                None => getrandom::fill(&mut raw).map_err(|_| {
                    MailError::Configuration("operating-system randomness unavailable".to_owned())
                })?,
            }
            Ok(format!("forge-{}", hex(&raw)))
        };
        let root = body(message, &mut boundary)?;

        let mut out = Vec::with_capacity(4096 + root.size_hint());
        let mut header = |name: &str, value: &str| {
            out.extend_from_slice(fold_header(name, value).as_bytes());
            out.extend_from_slice(b"\r\n");
        };
        header(
            "Date",
            &encoding::rfc5322_date(self.date.unwrap_or_else(SystemTime::now)),
        );
        header("From", &message.from.to_string());
        for (name, list) in [
            ("Reply-To", &message.reply_to),
            ("To", &message.to),
            ("Cc", &message.cc),
        ] {
            if !list.is_empty() {
                let joined: Vec<String> = list.iter().map(ToString::to_string).collect();
                header(name, &joined.join(", "));
            }
        }
        header("Subject", &encoding::encode_text(&message.subject));
        header("Message-ID", &message_id);
        header("MIME-Version", "1.0");
        let mut extra: Vec<(String, &str)> = message
            .headers
            .iter()
            .map(|(name, value)| (canonical_header_name(name), value.as_str()))
            .collect();
        extra.sort();
        for (name, value) in &extra {
            header(name, value);
        }
        for (name, value) in &root.headers {
            header(name, value);
        }
        out.extend_from_slice(b"\r\n");
        root.write(&mut out);
        if !out.ends_with(b"\r\n") {
            out.extend_from_slice(b"\r\n");
        }
        Ok(Encoded {
            message_id,
            sender: message.from.email.clone(),
            recipients: message.recipients(),
            data: out,
        })
    }
}

/// Validates `message` and renders it with the default [`Encoder`].
pub fn encode(message: &Message) -> Result<Encoded, MailError> {
    Encoder::new().encode(message)
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(DIGITS[usize::from(byte >> 4)]));
        out.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    out
}

/// One MIME entity: headers sorted by name, and a body.
struct Entity<'a> {
    headers: Vec<(&'static str, String)>,
    body: Body<'a>,
}

enum Body<'a> {
    Encoded(Vec<u8>),
    Base64(&'a [u8]),
    Multipart {
        boundary: String,
        parts: Vec<Entity<'a>>,
    },
}

impl Entity<'_> {
    fn size_hint(&self) -> usize {
        match &self.body {
            Body::Encoded(bytes) => bytes.len(),
            Body::Base64(data) => data.len() / 3 * 4 + data.len() / 28 + 8,
            Body::Multipart { parts, .. } => {
                256 + parts
                    .iter()
                    .map(|part| part.size_hint() + 512)
                    .sum::<usize>()
            }
        }
    }

    /// Writes the body (not the headers), as `mime/multipart.Writer` does
    /// for multipart bodies, followed by one more CRLF.
    fn write(&self, out: &mut Vec<u8>) {
        match &self.body {
            Body::Encoded(bytes) => out.extend_from_slice(bytes),
            Body::Base64(data) => base64_lines(data, out),
            Body::Multipart { boundary, parts } => {
                for (index, part) in parts.iter().enumerate() {
                    if index > 0 {
                        out.extend_from_slice(b"\r\n");
                    }
                    out.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
                    for (name, value) in &part.headers {
                        out.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
                    }
                    out.extend_from_slice(b"\r\n");
                    part.write(out);
                }
                out.extend_from_slice(format!("\r\n--{boundary}--\r\n\r\n").as_bytes());
            }
        }
    }
}

fn body<'a>(
    message: &'a Message,
    boundary: &mut impl FnMut() -> Result<String, MailError>,
) -> Result<Entity<'a>, MailError> {
    let (inline, files): (Vec<&Attachment>, Vec<&Attachment>) = message
        .attachments
        .iter()
        .partition(|attachment| !attachment.content_id.is_empty());
    let html_part = |boundary: &mut dyn FnMut() -> Result<String, MailError>| {
        let html = quoted_printable_part("text/html", &message.html);
        if inline.is_empty() {
            return Ok(html);
        }
        let mut parts = vec![html];
        parts.extend(
            inline
                .iter()
                .map(|attachment| attachment_part(attachment, true)),
        );
        multipart("related", parts, boundary)
    };
    let content = match (message.text.is_empty(), message.html.is_empty()) {
        (false, false) => {
            let text = quoted_printable_part("text/plain", &message.text);
            let html = html_part(boundary)?;
            multipart("alternative", vec![text, html], boundary)?
        }
        (true, false) => html_part(boundary)?,
        _ => quoted_printable_part("text/plain", &message.text),
    };
    if files.is_empty() {
        return Ok(content);
    }
    let mut parts = vec![content];
    parts.extend(
        files
            .iter()
            .map(|attachment| attachment_part(attachment, false)),
    );
    multipart("mixed", parts, boundary)
}

fn multipart<'a>(
    subtype: &str,
    parts: Vec<Entity<'a>>,
    boundary: &mut dyn FnMut() -> Result<String, MailError>,
) -> Result<Entity<'a>, MailError> {
    let boundary = boundary()?;
    let content_type = format!("multipart/{subtype}; boundary={boundary}");
    Ok(Entity {
        headers: vec![("Content-Type", content_type)],
        body: Body::Multipart { boundary, parts },
    })
}

fn quoted_printable_part<'a>(media_type: &str, text: &str) -> Entity<'a> {
    let normalized = text
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('\n', "\r\n");
    let mut encoded = quoted_printable(normalized.as_bytes());
    if !encoded.ends_with(b"\r\n") {
        encoded.extend_from_slice(b"\r\n");
    }
    Entity {
        headers: vec![
            ("Content-Transfer-Encoding", "quoted-printable".to_owned()),
            ("Content-Type", format!("{media_type}; charset=utf-8")),
        ],
        body: Body::Encoded(encoded),
    }
}

fn attachment_part(attachment: &Attachment, inline: bool) -> Entity<'_> {
    // Validated by `Message::validate_with`; the fallbacks cannot occur.
    let filename =
        sanitize_filename(&attachment.filename).unwrap_or_else(|_| "attachment".to_owned());
    let content_type = attachment_content_type(&attachment.content_type)
        .unwrap_or_else(|| "application/octet-stream".to_owned());
    let disposition = if inline { "inline" } else { "attachment" };
    let mut headers = vec![
        (
            "Content-Disposition",
            format_media_type(disposition, &[("filename", &filename)]).unwrap_or_default(),
        ),
        ("Content-Transfer-Encoding", "base64".to_owned()),
        (
            "Content-Type",
            format_media_type(&content_type, &[("name", &filename)]).unwrap_or_default(),
        ),
    ];
    if !attachment.content_id.is_empty() {
        headers.insert(1, ("Content-Id", format!("<{}>", attachment.content_id)));
    }
    Entity {
        headers,
        body: Body::Base64(&attachment.data),
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use super::*;
    use crate::Address;

    fn message() -> Message {
        Message::new(Address::new("Forge App", "app@Example.com").expect("valid"))
            .to(Address::new("Alice", "alice@example.org").expect("valid"))
            .bcc(Address::new("", "audit@example.org").expect("valid"))
            .subject("Welcome")
            .text("Hello Alice\nSecond line")
    }

    /// Whether the header block of `wire` has a header called `name` (names
    /// are case-insensitive). Only header names count: the `Message-ID` is
    /// random hex and can spell `bcc` by chance.
    fn has_header(wire: &str, name: &str) -> bool {
        let head = wire.split("\r\n\r\n").next().expect("header block");
        head.split("\r\n")
            .filter(|line| !line.starts_with([' ', '\t']))
            .filter_map(|line| line.split_once(':'))
            .any(|(header, _)| header.trim_end().eq_ignore_ascii_case(name))
    }

    #[test]
    fn header_scan_ignores_values_and_continuation_lines() {
        let wire = "Subject: bcc: nope\r\nX-Id: <bccbccbcc@example.com>\r\n bcc: folded\r\n\r\nBcc: body\r\n";
        assert!(!has_header(wire, "bcc"));
        assert!(has_header("To: a\r\nBCC: b\r\n\r\n", "bcc"));
        assert!(has_header("To: a\r\nbcc : b\r\n\r\n", "bcc"));
    }

    #[test]
    fn a_message_id_spelling_bcc_is_not_a_bcc_header() {
        let id = Uuid::parse_str("0bccbccb-cbcc-7bcc-8bcc-bccbccbccbcc").expect("uuid");
        let encoded = Encoder::new()
            .message_id(id)
            .encode(&message())
            .expect("valid");
        let text = String::from_utf8(encoded.data).expect("7-bit");
        assert!(text.to_ascii_lowercase().contains("bcc"), "{text}");
        assert!(!has_header(&text, "bcc"), "{text}");
        assert!(!text.contains("audit@"), "{text}");
    }

    #[test]
    fn a_thousand_random_ids_never_leak_the_bcc_recipient() {
        let message = message();
        for _ in 0..1000 {
            let encoded = encode(&message).expect("valid");
            let text = String::from_utf8(encoded.data).expect("7-bit");
            assert!(!has_header(&text, "bcc"), "{text}");
            assert!(!text.contains("audit@"), "{text}");
        }
    }

    #[test]
    fn bcc_is_envelope_only_and_ids_use_the_sender_domain() {
        let encoded = encode(&message()).expect("valid");
        let text = String::from_utf8(encoded.data).expect("7-bit");
        assert!(!has_header(&text, "bcc"), "{text}");
        assert!(!text.contains("audit@"), "{text}");
        assert_eq!(
            encoded.recipients,
            ["alice@example.org", "audit@example.org"]
        );
        assert_eq!(encoded.sender, "app@Example.com");
        assert!(encoded.message_id.ends_with("@example.com>"));
        let id = encoded
            .message_id
            .trim_start_matches('<')
            .split('@')
            .next()
            .and_then(|id| Uuid::parse_str(id).ok())
            .expect("uuid");
        assert_eq!(id.get_version_num(), 7);
        assert!(text.lines().all(|line| line.len() <= 998));
        assert!(text.bytes().all(|byte| byte.is_ascii()));
    }

    #[test]
    fn boundaries_are_random_and_unique() {
        let html = message().html("<p>Hi</p>");
        let first = encode(&html).expect("valid");
        let second = encode(&html).expect("valid");
        let boundary = |encoded: &Encoded| {
            let text = String::from_utf8_lossy(&encoded.data).into_owned();
            let start = text.find("boundary=").expect("multipart") + 9;
            text[start..start + 38].to_owned()
        };
        assert_ne!(boundary(&first), boundary(&second));
        assert!(boundary(&first).starts_with("forge-"));
    }

    #[test]
    fn deterministic_options_and_exhausted_entropy() {
        let encoder = Encoder::new()
            .date(UNIX_EPOCH + Duration::from_secs(1_792_001_045))
            .message_id(Uuid::nil())
            .boundary_entropy(vec![0xab; 16]);
        let encoded = encoder.encode(&message().html("<p>x</p>")).expect("valid");
        let text = String::from_utf8(encoded.data).expect("ascii");
        assert!(
            text.starts_with("Date: Wed, 14 Oct 2026 18:04:05 +0000\r\n"),
            "{text}"
        );
        assert!(text.contains(&format!("boundary=forge-{}", "ab".repeat(16))));
        assert_eq!(
            encoded.message_id,
            "<00000000-0000-0000-0000-000000000000@example.com>"
        );
        let failing = Encoder::new().boundary_entropy(vec![1; 15]);
        assert!(failing.encode(&message().html("<p>x</p>")).is_err());
    }

    #[test]
    fn invalid_messages_produce_no_output() {
        let mut bad = message();
        bad.subject = "x\r\nBcc: eve@example.com".to_owned();
        assert!(matches!(encode(&bad), Err(MailError::Invalid { .. })));
    }
}
