//! File storage, the equivalent of Rails' Active Storage (ADR 0029): a
//! [`Storage`] port, a local-disk adapter ([`DiskStorage`], Unix), HMAC-signed
//! time-limited links ([`Signer`]), an HTTP [`StorageHandler`] that serves them
//! with safe headers, and a strict multipart upload helper ([`receive`]).
//!
//! Stored files are untrusted input. The helpers therefore never trust a
//! client-declared content type or file name: [`receive`] checks the content
//! against a mandatory allow-list, the handler serves anything a browser could
//! execute or render as a page as a download (`Content-Disposition:
//! attachment`) with `X-Content-Type-Options: nosniff` and a sandboxing CSP,
//! and links are only valid for the key they were signed for until they
//! expire. Keys are restricted **by construction** ([`StorageKey`]): path
//! traversal needs no check at the call site because an unsafe key cannot be
//! built.
//!
//! `forge-http` buffers each request body (up to `FORGE_MAX_BODY_BYTES`) and
//! sends whole responses, so the port moves [`Bytes`](bytes::Bytes) rather than
//! streams; disk writes and reads are bounded by explicit byte limits.
//!
//! There is no S3-compatible adapter yet. A future adapter implements
//! [`Storage`] in its own crate and reuses the signer, handler and upload
//! helper unchanged.

#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use std::{fmt, io, time::SystemTime};

use async_trait::async_trait;
use bytes::Bytes;
use thiserror::Error;

#[cfg(unix)]
mod disk;
mod handler;
mod multipart;
mod signer;
mod sniff;
#[cfg(feature = "testing")]
pub mod testing;
mod upload;

#[cfg(unix)]
pub use disk::DiskStorage;
pub use handler::{HandlerOptions, StorageHandler};
pub use signer::{Link, MAX_LINK_TTL, MIN_KEY_BYTES, SignOptions, Signer};
pub use sniff::sniff_content_type;
pub use upload::{Received, UploadOptions, receive};

/// The content type of anything unidentified.
pub const OCTET_STREAM: &str = "application/octet-stream";

/// Why a storage operation was refused or failed.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum StorageError {
    /// There is no such file.
    #[error("file not found")]
    NotFound,
    /// A file or request is over its limit.
    #[error("file too large")]
    TooLarge,
    /// The key is not a valid [`StorageKey`].
    #[error("invalid storage key")]
    InvalidKey,
    /// The content type is malformed or not allowed.
    #[error("file type not allowed")]
    UnsupportedType,
    /// The request is not a `multipart/form-data` request.
    #[error("request body must be multipart/form-data")]
    NotMultipart,
    /// The link is missing, malformed, forged, for another key or binding.
    #[error("invalid or tampered link")]
    InvalidLink,
    /// The link is authentic but its time ran out.
    #[error("link expired")]
    ExpiredLink,
    /// The upload is malformed or exceeds a structural rule (the reason never
    /// contains client data).
    #[error("{0}")]
    InvalidUpload(&'static str),
    /// The multipart request carried no file in the expected field (for
    /// example a browser form submitted without choosing one). Use
    /// [`UploadOptions::optional_file`] to receive the text fields of such a
    /// submission instead.
    #[error("no file was uploaded")]
    NoFile,
    /// An option or argument is outside its bounds.
    #[error("invalid storage configuration: {0}")]
    Configuration(&'static str),
    /// The backing store failed. The source never appears in client
    /// responses; it can name a path, so it is for server logs only.
    #[error("storage I/O failed")]
    Io(#[source] io::Error),
}

impl StorageError {
    /// Stable machine-readable code (the `error.code` of JSON responses).
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::NotFound => "storage_not_found",
            Self::TooLarge => "storage_too_large",
            Self::InvalidKey => "storage_invalid_key",
            Self::UnsupportedType | Self::NotMultipart => "unsupported_media_type",
            Self::InvalidLink => "link_invalid",
            Self::ExpiredLink => "link_expired",
            Self::InvalidUpload(_) => "invalid_upload",
            Self::NoFile => "upload_missing",
            Self::Configuration(_) | Self::Io(_) => "storage_unavailable",
        }
    }

    /// The HTTP status a response to this error carries.
    #[must_use]
    pub fn status(&self) -> forge_http::StatusCode {
        use forge_http::StatusCode;
        match self {
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Self::InvalidKey | Self::InvalidUpload(_) | Self::NoFile => StatusCode::BAD_REQUEST,
            Self::UnsupportedType | Self::NotMultipart => StatusCode::UNSUPPORTED_MEDIA_TYPE,
            Self::InvalidLink | Self::ExpiredLink => StatusCode::FORBIDDEN,
            Self::Configuration(_) | Self::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// A JSON error response (`{"error":{"code","message","request_id"}}`) for
    /// `request`. The message is safe for clients; I/O detail is not included.
    #[must_use]
    pub fn into_response(&self, request: &forge_http::Request) -> forge_http::Response {
        let message = match self {
            Self::Configuration(_) | Self::Io(_) => "storage is temporarily unavailable".to_owned(),
            other => other.to_string(),
        };
        let body = serde_json::json!({
            "error": {
                "code": self.code(),
                "message": message,
                "request_id": request.request_id().to_string(),
            }
        });
        let status = self.status();
        forge_http::Response::json(status, &body)
            .unwrap_or_else(|_| forge_http::Response::new(status, message))
    }
}

/// A storage key: slash-separated segments of ASCII letters, digits, `.`, `_`
/// and `-`, at most 512 bytes, 8 segments and 128 bytes per segment, with no
/// empty segment and **no segment starting with a dot**.
///
/// A valid key can therefore never contain `..`, `.`, a hidden-file name, a
/// backslash, a drive letter, a percent escape, a NUL or a non-ASCII byte, nor
/// be an absolute path, in every adapter and before any file-system call. Keys
/// for user content are UUIDv7 ([`StorageKey::unique`]); the original file name
/// is metadata, never part of the key.
///
/// ```
/// use forge_storage::StorageKey;
/// assert!(StorageKey::parse("avatars/0192b2c4-7d1e-7000-8000-000000000000").is_ok());
/// for bad in ["../etc/passwd", "a/../b", ".hidden", "/abs", "a//b", "a\\b", "a%2fb", "é"] {
///     assert!(StorageKey::parse(bad).is_err(), "{bad}");
/// }
/// ```
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct StorageKey(String);

/// Longest key in bytes.
pub const MAX_KEY_LENGTH: usize = 512;
const MAX_SEGMENT_LENGTH: usize = 128;
const MAX_KEY_DEPTH: usize = 8;

impl StorageKey {
    /// Validates `key`. The error never contains it.
    pub fn parse(key: &str) -> Result<Self, StorageError> {
        if key.is_empty() || key.len() > MAX_KEY_LENGTH {
            return Err(StorageError::InvalidKey);
        }
        let mut segments = 1;
        let mut segment_length = 0_usize;
        for byte in key.bytes() {
            match byte {
                b'/' => {
                    if segment_length == 0 {
                        return Err(StorageError::InvalidKey); // leading, trailing or doubled slash
                    }
                    segments += 1;
                    segment_length = 0;
                }
                b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'-' => segment_length += 1,
                b'.' => {
                    if segment_length == 0 {
                        return Err(StorageError::InvalidKey); // ".", "..", ".hidden"
                    }
                    segment_length += 1;
                }
                _ => return Err(StorageError::InvalidKey),
            }
            if segment_length > MAX_SEGMENT_LENGTH {
                return Err(StorageError::InvalidKey);
            }
        }
        if segment_length == 0 || segments > MAX_KEY_DEPTH {
            return Err(StorageError::InvalidKey);
        }
        Ok(Self(key.to_owned()))
    }

    /// A fresh UUIDv7 key, below `prefix` when given (validated like a key).
    pub fn unique(prefix: Option<&str>) -> Result<Self, StorageError> {
        let id = uuid::Uuid::now_v7().to_string();
        match prefix {
            Some(prefix) => Self::parse(&format!("{prefix}/{id}")),
            None => Self::parse(&id),
        }
    }

    /// The key as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The segments of the key, in order.
    pub fn segments(&self) -> impl Iterator<Item = &str> {
        self.0.split('/')
    }
}

impl fmt::Display for StorageKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Describes a stored blob.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Info {
    /// The blob's key.
    pub key: StorageKey,
    /// Size in bytes.
    pub size: u64,
    /// Normalized media type (`application/octet-stream` when unknown).
    pub content_type: String,
    /// Sanitized original file name, for downloads.
    pub filename: Option<String>,
    /// Hex SHA-256 of the content, when the adapter knows it.
    pub sha256: Option<String>,
    /// Last modification time.
    pub modified: SystemTime,
}

/// A stored blob with its content.
#[derive(Clone, Debug)]
pub struct Blob {
    /// What is stored.
    pub info: Info,
    /// The content.
    pub body: Bytes,
}

/// What to store alongside the content.
#[derive(Clone, Debug)]
pub struct PutOptions {
    content_type: Option<String>,
    filename: Option<String>,
    max_bytes: u64,
}

impl PutOptions {
    /// Options with the required size limit (positive): there is no unbounded
    /// `put`.
    pub fn new(max_bytes: u64) -> Result<Self, StorageError> {
        if max_bytes == 0 {
            return Err(StorageError::Configuration(
                "storage put needs a positive max_bytes",
            ));
        }
        Ok(Self {
            content_type: None,
            filename: None,
            max_bytes,
        })
    }

    /// A media type such as `image/png` (parameters are dropped); default
    /// `application/octet-stream`.
    #[must_use]
    pub fn content_type(mut self, content_type: impl Into<String>) -> Self {
        self.content_type = Some(content_type.into());
        self
    }

    /// The original file name; it is sanitized before it is kept.
    #[must_use]
    pub fn filename(mut self, filename: impl Into<String>) -> Self {
        self.filename = Some(filename.into());
        self
    }

    /// The size limit.
    #[must_use]
    pub const fn max_bytes(&self) -> u64 {
        self.max_bytes
    }

    /// The declared content type, if any.
    #[must_use]
    pub fn declared_content_type(&self) -> Option<&str> {
        self.content_type.as_deref()
    }

    /// The declared file name, if any.
    #[must_use]
    pub fn declared_filename(&self) -> Option<&str> {
        self.filename.as_deref()
    }
}

/// Keeps named blobs. Implementations are safe for concurrent use.
///
/// A `put` replaces any existing blob of the key. Callers generate unguessable
/// keys for user content ([`receive`] uses UUIDv7) and keep the original file
/// name in [`Info::filename`], not in the key.
#[async_trait]
pub trait Storage: Send + Sync {
    /// Stores `body` under `key` and returns what was stored. Fails with
    /// [`StorageError::TooLarge`], storing nothing, when `body` is longer than
    /// `options.max_bytes()`. A failed `put` leaves no partial blob and does not
    /// replace an existing one.
    async fn put(
        &self,
        key: &StorageKey,
        body: Bytes,
        options: PutOptions,
    ) -> Result<Info, StorageError>;

    /// Reads the content of `key`, failing with [`StorageError::TooLarge`] when
    /// it is longer than `max_bytes` (so serving a blob has a memory bound).
    async fn open(&self, key: &StorageKey, max_bytes: u64) -> Result<Blob, StorageError>;

    /// Describes `key` without reading it.
    async fn stat(&self, key: &StorageKey) -> Result<Info, StorageError>;

    /// Removes `key`. Deleting a missing key is not an error.
    async fn delete(&self, key: &StorageKey) -> Result<(), StorageError>;
}

/// The lower-case `type/subtype` of a media type, without parameters, or an
/// error when `value` is not a media type. Empty is `application/octet-stream`.
pub fn normalize_content_type(value: &str) -> Result<String, StorageError> {
    let essence = value.split(';').next().unwrap_or("").trim();
    if essence.is_empty() {
        return if value.trim().is_empty() {
            Ok(OCTET_STREAM.to_owned())
        } else {
            Err(StorageError::UnsupportedType)
        };
    }
    let lower = essence.to_ascii_lowercase();
    let token = |part: &str| {
        !part.is_empty()
            && part.bytes().all(|byte| {
                byte.is_ascii_alphanumeric()
                    || matches!(
                        byte,
                        b'!' | b'#' | b'$' | b'&' | b'^' | b'_' | b'.' | b'+' | b'-'
                    )
            })
    };
    match lower.split_once('/') {
        Some((kind, subtype)) if lower.len() <= 127 && token(kind) && token(subtype) => Ok(lower),
        _ => Err(StorageError::UnsupportedType),
    }
}

/// Reduces a client-supplied file name to something safe to show and to put in
/// a `Content-Disposition` header: no directories, control or
/// bidirectional-override characters, quotes or leading dots, at most 255
/// bytes. It may return an empty string.
#[must_use]
pub fn sanitize_filename(name: &str) -> String {
    let name = name.rsplit(['/', '\\']).next().unwrap_or("");
    let mut cleaned = String::with_capacity(name.len().min(255));
    for character in name.chars() {
        if character == char::REPLACEMENT_CHARACTER
            || character.is_control()
            || character == '"'
            || is_formatting(character)
        {
            continue;
        }
        if cleaned.len() + character.len_utf8() > 255 {
            break;
        }
        cleaned.push(character);
    }
    cleaned.trim().trim_start_matches('.').to_owned()
}

/// Line and paragraph separators and bidirectional controls: invisible
/// characters that reorder or hide text, which a file name must not carry.
fn is_formatting(character: char) -> bool {
    matches!(
        u32::from(character),
        0x2028 | 0x2029 | 0x200e | 0x200f | 0x061c | 0x202a..=0x202e | 0x2066..=0x2069
    )
}

/// The content types that browsers render without running anything of the
/// file's own: raster images. Everything else, notably HTML, SVG, XML, scripts
/// and PDF (whose viewers run embedded code), is served as a download.
const INLINE_SAFE: &[&str] = &[
    "image/png",
    "image/jpeg",
    "image/gif",
    "image/webp",
    "image/avif",
    "image/bmp",
    "image/x-icon",
];

/// Whether files of the content type may be displayed inline by default.
#[must_use]
pub fn is_inline_safe(content_type: &str) -> bool {
    INLINE_SAFE.contains(&content_type)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_restricted_by_construction() {
        for good in [
            "a",
            "a/b/c.txt",
            "v1.2/file-name_3.tar.gz",
            "avatars/0192b2c4-7d1e-7000-8000-000000000000",
            &"a".repeat(128),
            &["s"; 8].join("/"),
        ] {
            assert!(StorageKey::parse(good).is_ok(), "{good}");
        }
        let long_segment = "a".repeat(129);
        let too_deep = ["s"; 9].join("/");
        let too_long = format!(
            "{}/{}/{}/{}",
            "a".repeat(128),
            "b".repeat(128),
            "c".repeat(128),
            "d".repeat(128)
        );
        for bad in [
            "",
            ".",
            "..",
            "../x",
            "a/../b",
            "a/./b",
            ".hidden",
            "a/.hidden",
            "/abs",
            "trailing/",
            "a//b",
            "a\\b",
            "C:\\x",
            "c:/x",
            "a%2e%2e/b",
            "a b",
            "a\0b",
            "ключ",
            "a?b",
            "a#b",
            long_segment.as_str(),
            too_deep.as_str(),
            too_long.as_str(),
        ] {
            assert!(
                matches!(StorageKey::parse(bad), Err(StorageError::InvalidKey)),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn unique_keys_are_uuidv7_below_a_validated_prefix() {
        let key = StorageKey::unique(Some("avatars")).expect("key");
        let (prefix, id) = key.as_str().split_once('/').expect("prefix");
        assert_eq!(prefix, "avatars");
        let parsed = uuid::Uuid::parse_str(id).expect("uuid");
        assert_eq!(parsed.get_version_num(), 7);
        assert_ne!(key, StorageKey::unique(Some("avatars")).expect("key"));
        assert!(StorageKey::unique(Some("../x")).is_err());
        assert!(StorageKey::unique(Some(".hidden")).is_err());
        assert!(StorageKey::unique(None).is_ok());
    }

    #[test]
    fn content_types_are_normalized() {
        assert_eq!(normalize_content_type("").expect("empty"), OCTET_STREAM);
        assert_eq!(
            normalize_content_type("Image/PNG; charset=utf-8").expect("png"),
            "image/png"
        );
        assert_eq!(
            normalize_content_type("text/html").expect("html"),
            "text/html"
        );
        for bad in [
            "nope",
            "a/",
            "/b",
            "a b/c",
            "text/html/x",
            ";charset=x",
            "a/b\nc",
        ] {
            assert!(
                matches!(
                    normalize_content_type(bad),
                    Err(StorageError::UnsupportedType)
                ),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn file_names_are_sanitized() {
        for (input, expected) in [
            ("me.png", "me.png"),
            ("../../etc/passwd", "passwd"),
            ("C:\\dir\\evil.exe", "evil.exe"),
            (".htaccess", "htaccess"),
            ("..hidden", "hidden"),
            ("a\"b.png", "ab.png"),
            ("line\r\nbreak.txt", "linebreak.txt"),
            ("nul\0.txt", "nul.txt"),
            ("  spaced.txt  ", "spaced.txt"),
            ("relatório.pdf", "relatório.pdf"),
            ("evil\u{202e}gpj.exe", "evilgpj.exe"),
            ("a\u{2028}b", "ab"),
            ("dir/", ""),
            ("", ""),
        ] {
            assert_eq!(sanitize_filename(input), expected, "{input:?}");
        }
        assert_eq!(sanitize_filename(&"x".repeat(1000)).len(), 255);
        assert_eq!(sanitize_filename(&"é".repeat(1000)).len(), 254);
    }

    #[test]
    fn only_raster_images_are_inline_safe() {
        for safe in [
            "image/png",
            "image/jpeg",
            "image/gif",
            "image/webp",
            "image/avif",
            "image/bmp",
            "image/x-icon",
        ] {
            assert!(is_inline_safe(safe));
        }
        for unsafe_type in [
            "text/html",
            "image/svg+xml",
            "application/pdf",
            "application/xml",
            "text/xml",
            "text/plain",
            "application/javascript",
            OCTET_STREAM,
        ] {
            assert!(!is_inline_safe(unsafe_type), "{unsafe_type}");
        }
    }

    #[test]
    fn put_options_require_a_positive_limit() {
        assert!(PutOptions::new(0).is_err());
        let options = PutOptions::new(10)
            .expect("options")
            .content_type("image/png")
            .filename("a.png");
        assert_eq!(options.max_bytes(), 10);
        assert_eq!(options.declared_content_type(), Some("image/png"));
        assert_eq!(options.declared_filename(), Some("a.png"));
    }
}
