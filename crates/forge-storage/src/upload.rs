use std::collections::{BTreeMap, HashSet};

use forge_http::{Request, header};

use crate::{
    Info, OCTET_STREAM, PutOptions, Storage, StorageError, StorageKey, multipart,
    normalize_content_type, sanitize_filename,
    sniff::{SNIFF_LENGTH, sniff_content_type},
};

/// Slack for the headers and boundaries of a legitimate request.
const MULTIPART_SLACK: u64 = 64 << 10;
const DEFAULT_MAX_FIELDS: usize = 32;
const DEFAULT_MAX_FIELD_BYTES: u64 = 64 << 10;
const MAX_FILES_LIMIT: usize = 1000;

/// How [`receive`] accepts an upload. Everything is mandatory or bounded:
/// there is no allow-everything default.
#[derive(Clone, Debug)]
pub struct UploadOptions {
    field: String,
    allowed_types: Vec<String>,
    max_file_bytes: u64,
    max_files: usize,
    prefix: Option<String>,
    max_fields: usize,
    max_field_bytes: u64,
    optional_file: bool,
}

impl UploadOptions {
    /// Accepts files in the multipart field `field`, each at most
    /// `max_file_bytes`, whose **detected** content type is one of
    /// `allowed_types` (such as `image/png`; no wildcards, at least one).
    /// Files in any other field are rejected.
    pub fn new(
        field: impl Into<String>,
        allowed_types: &[&str],
        max_file_bytes: u64,
    ) -> Result<Self, StorageError> {
        let field = field.into();
        if field.is_empty() || field.len() > multipart::MAX_NAME {
            return Err(StorageError::Configuration(
                "storage upload field is required",
            ));
        }
        if max_file_bytes == 0 {
            return Err(StorageError::Configuration(
                "storage upload max file bytes must be positive",
            ));
        }
        if allowed_types.is_empty() {
            return Err(StorageError::Configuration(
                "storage upload needs at least one allowed content type",
            ));
        }
        let mut allowed = Vec::with_capacity(allowed_types.len());
        for allowed_type in allowed_types {
            let normalized = normalize_content_type(allowed_type).map_err(|_| {
                StorageError::Configuration("storage upload allowed content type is invalid")
            })?;
            if allowed_type.trim().is_empty()
                || normalized.ends_with("/*")
                || normalized.contains('*')
            {
                return Err(StorageError::Configuration(
                    "storage upload allowed content type is invalid",
                ));
            }
            allowed.push(normalized);
        }
        Ok(Self {
            field,
            allowed_types: allowed,
            max_file_bytes,
            max_files: 1,
            prefix: None,
            max_fields: DEFAULT_MAX_FIELDS,
            max_field_bytes: DEFAULT_MAX_FIELD_BYTES,
            optional_file: false,
        })
    }

    /// Accepts up to `max_files` files in the field (`1..=1000`, default 1).
    #[must_use]
    pub const fn max_files(mut self, max_files: usize) -> Self {
        self.max_files = max_files;
        self
    }

    /// A key prefix for the stored files (for example `avatars`), validated like
    /// a key. Keys are `prefix/<uuidv7>`.
    #[must_use]
    pub fn prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = Some(prefix.into());
        self
    }

    /// Bounds the other (text) field values **in total**, counting every
    /// repetition of a name (default 32), and each at `max_field_bytes`
    /// (default 64 KiB). The total bytes of all text fields are bounded by
    /// `max_fields x max_field_bytes`.
    #[must_use]
    pub const fn text_fields(mut self, max_fields: usize, max_field_bytes: u64) -> Self {
        self.max_fields = max_fields;
        self.max_field_bytes = max_field_bytes;
        self
    }

    /// Returns `Ok` with no files instead of [`StorageError::NoFile`] when the
    /// submission carries none (a browser form sent without choosing a file),
    /// so the text fields can still be read.
    #[must_use]
    pub const fn optional_file(mut self, optional: bool) -> Self {
        self.optional_file = optional;
        self
    }

    fn validated(&self) -> Result<(), StorageError> {
        if !(1..=MAX_FILES_LIMIT).contains(&self.max_files) || self.max_field_bytes == 0 {
            return Err(StorageError::Configuration(
                "storage upload limits are invalid",
            ));
        }
        if let Some(prefix) = &self.prefix {
            StorageKey::parse(&format!("{prefix}/x"))
                .map_err(|_| StorageError::Configuration("storage upload prefix is invalid"))?;
        }
        Ok(())
    }

    /// The most bytes a request this configuration accepts can have.
    fn body_limit(&self) -> u64 {
        (self.max_files as u64)
            .saturating_mul(self.max_file_bytes)
            .saturating_add((self.max_fields as u64).saturating_mul(self.max_field_bytes))
            .saturating_add(MULTIPART_SLACK)
    }
}

/// What [`receive`] stored and read.
#[derive(Debug, Default)]
pub struct Received {
    /// The stored files, in order.
    pub files: Vec<Info>,
    /// The text fields of the form, in order, repeated names included
    /// (including a CSRF token field: send the token in a header, or verify it
    /// from here).
    pub fields: BTreeMap<String, Vec<String>>,
}

/// Reads a `multipart/form-data` upload from `request` and stores its files in
/// `store`, rejecting anything outside `options`.
///
/// Strictness (each case is a negative test):
///
/// - the request `Content-Type` must be `multipart/form-data` with a valid
///   boundary ([`StorageError::NotMultipart`], 415);
/// - the body is capped at `max_files x max_file_bytes + max_fields x
///   max_field_bytes + 64 KiB` ([`StorageError::TooLarge`], 413) on top of the
///   server-wide `FORGE_HTTP_MAX_BODY_BYTES` that bounded its buffering;
/// - one expected file field; files in other fields, more files than allowed,
///   empty files, parts without a name, non-identity transfer encodings and
///   every structural deviation of [`multipart`](self) are
///   [`StorageError::InvalidUpload`] (400);
/// - text fields are bounded in **count** (every repetition of a name counts)
///   and in **bytes** (the total), and each at `max_field_bytes`;
/// - `allowed_types` decides on the type **detected from the bytes**
///   ([`sniff_content_type`]), never on the client's label: a file whose
///   content is recognizably an image, PDF, archive, HTML or XML must be one
///   of the allowed types, so an HTML page labelled `image/png` is refused; a
///   file with no signature keeps its declared type if allowed, except that
///   types that must carry a signature (images, audio, video, fonts, PDF,
///   archives) and text types (which need text content) are refused when the
///   content does not match ([`StorageError::UnsupportedType`], 415);
/// - a rejected upload removes the files it had already stored;
/// - keys are UUIDv7 under the optional prefix; the client's file name sanitized
///   ([`sanitize_filename`]) and kept only as metadata.
///
/// Authenticate, authorize and CSRF-check **before** calling it.
pub async fn receive(
    request: &Request,
    store: &dyn Storage,
    options: &UploadOptions,
) -> Result<Received, StorageError> {
    options.validated()?;
    let content_type = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .ok_or(StorageError::NotMultipart)?;
    let boundary = multipart::boundary(content_type)?;
    let body = request.body();
    if body.len() as u64 > options.body_limit() {
        return Err(StorageError::TooLarge);
    }
    let parts = multipart::parse(body, &boundary)?;

    // Validate everything that does not need the store before storing a byte.
    let allowed: HashSet<&str> = options.allowed_types.iter().map(String::as_str).collect();
    let mut received = Received::default();
    let (mut field_count, mut field_bytes, mut file_count) = (0_usize, 0_u64, 0_usize);
    let mut files = Vec::new();
    for part in &parts {
        let content = &body[part.content.clone()];
        let Some(filename) = &part.filename else {
            // The budget counts values, not distinct names: one name repeated
            // many times is many fields, and all of them stay in memory.
            field_count += 1;
            if field_count > options.max_fields {
                return Err(StorageError::InvalidUpload("too many form fields"));
            }
            field_bytes += content.len() as u64;
            if content.len() as u64 > options.max_field_bytes
                || field_bytes > (options.max_fields as u64).saturating_mul(options.max_field_bytes)
            {
                return Err(StorageError::TooLarge);
            }
            let value = String::from_utf8(content.to_vec())
                .map_err(|_| StorageError::InvalidUpload("form field is not valid UTF-8"))?;
            received
                .fields
                .entry(part.name.clone())
                .or_default()
                .push(value);
            continue;
        };
        if part.name != options.field {
            return Err(StorageError::InvalidUpload("unexpected file field"));
        }
        file_count += 1;
        if file_count > options.max_files {
            return Err(StorageError::InvalidUpload("too many files"));
        }
        if content.is_empty() {
            return Err(StorageError::InvalidUpload("the uploaded file is empty"));
        }
        if content.len() as u64 > options.max_file_bytes {
            return Err(StorageError::TooLarge);
        }
        let sniffed = sniff_content_type(&content[..content.len().min(SNIFF_LENGTH)]);
        let resolved = resolve_type(part.content_type.as_deref(), sniffed, &allowed)?;
        files.push((part.content.clone(), filename.clone(), resolved));
    }
    if files.is_empty() {
        return if options.optional_file {
            Ok(received)
        } else {
            Err(StorageError::NoFile)
        };
    }

    for (range, filename, content_type) in files {
        let key = StorageKey::unique(options.prefix.as_deref())?;
        let put = PutOptions::new(options.max_file_bytes)?
            .content_type(content_type)
            .filename(sanitize_filename(&filename));
        match store.put(&key, body.slice(range), put).await {
            Ok(info) => received.files.push(info),
            Err(error) => {
                cleanup(store, &received.files).await;
                return Err(error);
            }
        }
    }
    Ok(received)
}

/// Decides the content type to store. The sniffed type is what the bytes are;
/// the declared one is what the client says they are. A file whose bytes
/// identify a specific type (an image, a PDF, an archive, HTML...) is stored as
/// that type, which must be allowed; the client's claim then does not matter,
/// and the label never upgrades anything. A file with no signature (plain text,
/// CSV, JSON, SVG source...) keeps its declared type if that is allowed, except
/// that a type which must have a signature is refused and a text type needs
/// text content.
fn resolve_type(
    declared: Option<&str>,
    sniffed: &str,
    allowed: &HashSet<&str>,
) -> Result<String, StorageError> {
    let declared = declared.filter(|value| !value.trim().is_empty());
    if sniffed == "text/xml"
        && declared.is_some_and(|value| {
            value
                .split(';')
                .next()
                .unwrap_or("")
                .trim()
                .eq_ignore_ascii_case("image/svg+xml")
        })
        && allowed.contains("image/svg+xml")
    {
        // SVG with an XML prolog is sniffed as XML. It is stored as SVG, which
        // the handler never displays inline.
        return Ok("image/svg+xml".to_owned());
    }
    if sniffed != "text/plain" && sniffed != OCTET_STREAM {
        return if allowed.contains(sniffed) {
            Ok(sniffed.to_owned())
        } else {
            Err(StorageError::UnsupportedType)
        };
    }
    let mut claimed = match declared {
        Some(value) => normalize_content_type(value)?,
        None => OCTET_STREAM.to_owned(),
    };
    if claimed == OCTET_STREAM {
        claimed = sniffed.to_owned();
    }
    if has_signature(&claimed) || !allowed.contains(claimed.as_str()) {
        return Err(StorageError::UnsupportedType);
    }
    if is_textual(&claimed) && sniffed != "text/plain" {
        return Err(StorageError::UnsupportedType); // binary content under a text label
    }
    Ok(claimed)
}

/// Whether files of the type are text, so binary content cannot be one.
fn is_textual(content_type: &str) -> bool {
    content_type.starts_with("text/")
        || content_type == "application/json"
        || content_type == "application/xml"
        || content_type.ends_with("+json")
        || content_type.ends_with("+xml")
}

/// Whether files of the type can be recognized by their first bytes, so a file
/// without the signature is not of that type.
fn has_signature(content_type: &str) -> bool {
    if content_type == "image/svg+xml" {
        return false;
    }
    ["image/", "audio/", "video/", "font/"]
        .iter()
        .any(|prefix| content_type.starts_with(prefix))
        || matches!(
            content_type,
            "application/pdf"
                | "application/zip"
                | "application/gzip"
                | "application/x-gzip"
                | "application/wasm"
                | "application/x-rar-compressed"
                | "application/ogg"
                | "application/postscript"
                | "text/html"
                | "text/xml"
        )
}

/// Removes files stored by a rejected upload. Failures are logged without the
/// key; a leftover blob has an unguessable key and is garbage, not exposure.
async fn cleanup(store: &dyn Storage, files: &[Info]) {
    for file in files {
        if let Err(error) = store.delete(&file.key).await {
            tracing::warn!(%error, "could not remove a file of a rejected upload");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allowed<'a>(types: &[&'a str]) -> HashSet<&'a str> {
        types.iter().copied().collect()
    }

    #[test]
    fn the_bytes_decide_not_the_label() {
        let png_only = allowed(&["image/png"]);
        // Recognizable content must itself be allowed.
        assert_eq!(
            resolve_type(Some("image/png"), "image/png", &png_only).expect("png"),
            "image/png"
        );
        for (declared, sniffed) in [
            (Some("image/png"), "text/html"),
            (Some("image/png"), "text/xml"),
            (Some("image/png"), "application/pdf"),
            (Some("image/png"), "application/zip"),
            (Some("text/html"), "text/html"),
            (None, "image/gif"),
        ] {
            assert!(
                matches!(
                    resolve_type(declared, sniffed, &png_only),
                    Err(StorageError::UnsupportedType)
                ),
                "{declared:?} {sniffed}"
            );
        }
        // A true PDF labelled as a PNG is stored as a PDF, and only when PDF is allowed.
        let both = allowed(&["image/png", "application/pdf"]);
        assert_eq!(
            resolve_type(Some("image/png"), "application/pdf", &both).expect("pdf"),
            "application/pdf"
        );
    }

    #[test]
    fn undetectable_content_keeps_its_declared_type_only_when_allowed_and_plausible() {
        let csv = allowed(&["text/csv"]);
        assert_eq!(
            resolve_type(Some("text/csv"), "text/plain", &csv).expect("csv"),
            "text/csv"
        );
        // Not allowed, no label, or the label needs a signature the bytes lack.
        assert!(
            resolve_type(Some("text/plain"), "text/plain", &csv).is_err(),
            "unlisted text label"
        );
        assert!(
            resolve_type(None, "text/plain", &csv).is_err(),
            "no label means text/plain, which is unlisted"
        );
        assert!(
            resolve_type(Some("image/png"), "text/plain", &allowed(&["image/png"])).is_err(),
            "junk claiming png"
        );
        assert!(
            resolve_type(Some("image/png"), OCTET_STREAM, &allowed(&["image/png"])).is_err(),
            "exe claiming png"
        );
        assert!(
            resolve_type(Some("text/csv"), OCTET_STREAM, &csv).is_err(),
            "binary claiming csv"
        );
        assert!(
            resolve_type(None, OCTET_STREAM, &csv).is_err(),
            "unlabelled binary"
        );
        assert!(
            resolve_type(Some("nope"), "text/plain", &csv).is_err(),
            "malformed label"
        );
        // Unlabelled text is text/plain and may be allowed explicitly.
        assert_eq!(
            resolve_type(None, "text/plain", &allowed(&["text/plain"])).expect("text"),
            "text/plain"
        );
        assert_eq!(
            resolve_type(None, OCTET_STREAM, &allowed(&[OCTET_STREAM]))
                .expect("explicit octet-stream"),
            OCTET_STREAM
        );
    }

    #[test]
    fn svg_with_a_prolog_is_stored_as_svg_only_when_allowed() {
        let svg = allowed(&["image/svg+xml"]);
        assert_eq!(
            resolve_type(Some("image/svg+xml"), "text/xml", &svg).expect("svg"),
            "image/svg+xml"
        );
        assert!(
            resolve_type(Some("image/png"), "text/xml", &svg).is_err(),
            "an svg labelled png is xml"
        );
        assert!(resolve_type(Some("image/svg+xml"), "text/xml", &allowed(&["image/png"])).is_err());
        // SVG without a prolog is plain text to the sniffer: it keeps its label.
        assert_eq!(
            resolve_type(Some("image/svg+xml"), "text/plain", &svg).expect("bare svg"),
            "image/svg+xml"
        );
    }

    #[test]
    fn options_are_validated() {
        assert!(UploadOptions::new("", &["image/png"], 10).is_err());
        assert!(UploadOptions::new("f", &[], 10).is_err());
        assert!(UploadOptions::new("f", &["image/png"], 0).is_err());
        for wildcard in ["*/*", "image/*", "*", ""] {
            assert!(
                UploadOptions::new("f", &[wildcard], 10).is_err(),
                "{wildcard:?}"
            );
        }
        assert!(UploadOptions::new("f", &["nope"], 10).is_err());
        let ok = UploadOptions::new("f", &["IMAGE/PNG; q=1"], 10).expect("options");
        assert_eq!(ok.allowed_types, vec!["image/png"]);
        assert!(ok.clone().max_files(0).validated().is_err());
        assert!(ok.clone().max_files(1001).validated().is_err());
        assert!(ok.clone().prefix("../x").validated().is_err());
        assert!(ok.clone().prefix(".hidden").validated().is_err());
        assert!(ok.clone().text_fields(4, 0).validated().is_err());
        assert!(ok.max_files(2).prefix("avatars").validated().is_ok());
    }
}
