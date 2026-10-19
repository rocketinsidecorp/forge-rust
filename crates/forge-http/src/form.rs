//! Strict `application/x-www-form-urlencoded` request bodies (HTML forms).
//!
//! The body grammar is the query-string grammar of [`crate::query`]: pairs
//! split on `&`, `+` is a space, `%XX` escapes must form UTF-8, and empty
//! segments, pairs without `=` and empty names are rejected. On top of it, form
//! bodies follow HTML form semantics:
//!
//! - a name may repeat only when its field is a sequence (`Vec<T>`, for a
//!   group of checkboxes or a multiple `<select>`); a repeated scalar field is
//!   [`FormErrorKind::DuplicateField`];
//! - `bool` accepts `true`, `on` (what a checked checkbox without a `value`
//!   sends) and `1`, or `false` and `0`. An unchecked checkbox sends nothing,
//!   so declare checkbox fields `#[serde(default)] bool`;
//! - an empty value (a blank `<input>`) is `None` for an `Option` field;
//! - the framework fields [`CSRF_FIELD_NAME`] and [`METHOD_FIELD_NAME`] are
//!   consumed by the CSRF and method-override layer and never reach the
//!   application's type, so `#[serde(deny_unknown_fields)]` keeps working.
//!
//! The body is bounded by the server's `max_body_bytes` before a handler runs
//! and by [`MAX_FORM_FIELDS`] here.

use http::{HeaderMap, HeaderValue, header};
use serde::de::DeserializeOwned;
use thiserror::Error;

use crate::query::{self, Mode, Pairs, QueryErrorKind};

/// Media type of an HTML form body.
pub const FORM_CONTENT_TYPE: &str = "application/x-www-form-urlencoded";

/// Maximum number of `name=value` pairs in one form body.
pub const MAX_FORM_FIELDS: usize = 1000;

/// Form field that carries the CSRF token of an HTML form submission.
pub const CSRF_FIELD_NAME: &str = "csrf_token";

/// Form field that tunnels `PATCH`, `PUT` and `DELETE` through `POST`.
pub const METHOD_FIELD_NAME: &str = "_method";

/// Fields consumed by the framework before [`crate::Request::form`] decodes
/// the application's type.
pub const RESERVED_FORM_FIELDS: [&str; 2] = [CSRF_FIELD_NAME, METHOD_FIELD_NAME];

/// What was wrong with a form body.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum FormErrorKind {
    /// The request does not carry exactly one
    /// `Content-Type: application/x-www-form-urlencoded` (an optional
    /// `charset` must be UTF-8).
    UnsupportedMediaType,
    /// The body is not strict `application/x-www-form-urlencoded` UTF-8.
    Malformed,
    /// The body has more than [`MAX_FORM_FIELDS`] pairs.
    TooManyFields,
    /// A non-sequence field appears more than once.
    DuplicateField,
    /// A field the target type does not accept.
    UnknownField,
    /// A required field is absent.
    MissingField,
    /// A value does not parse as its field's type, or the body does not fit
    /// the target type at all.
    InvalidValue,
}

impl FormErrorKind {
    /// A caller-safe description that never includes request input.
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::UnsupportedMediaType => {
                "request body must be application/x-www-form-urlencoded (UTF-8)"
            }
            Self::Malformed => "form body is not valid application/x-www-form-urlencoded",
            Self::TooManyFields => "form body has too many fields",
            Self::DuplicateField => "form field must not be repeated",
            Self::UnknownField => "unknown form field",
            Self::MissingField => "missing required form field",
            Self::InvalidValue => "form field has an invalid value",
        }
    }
}

impl From<QueryErrorKind> for FormErrorKind {
    fn from(kind: QueryErrorKind) -> Self {
        match kind {
            QueryErrorKind::Malformed => Self::Malformed,
            QueryErrorKind::DuplicateParameter => Self::DuplicateField,
            QueryErrorKind::UnknownParameter => Self::UnknownField,
            QueryErrorKind::MissingParameter => Self::MissingField,
            QueryErrorKind::InvalidValue => Self::InvalidValue,
        }
    }
}

/// A form body that could not be decoded into the requested type.
///
/// [`FormError::kind`] and [`FormError::field`] are safe to show to the user
/// (for example next to the offending input when a form is rendered again
/// with `422`); the `Display` output may include decoder detail and is meant
/// for logs.
#[derive(Debug, Error)]
#[error("{}{}", kind.message(), detail.as_deref().map(|detail| format!(": {detail}")).unwrap_or_default())]
pub struct FormError {
    kind: FormErrorKind,
    field: Option<String>,
    detail: Option<String>,
}

impl FormError {
    fn new(kind: FormErrorKind) -> Self {
        Self {
            kind,
            field: None,
            detail: None,
        }
    }

    /// What was wrong.
    #[must_use]
    pub fn kind(&self) -> FormErrorKind {
        self.kind
    }

    /// The offending field name, truncated to 64 characters, when the
    /// failure concerns one field.
    #[must_use]
    pub fn field(&self) -> Option<&str> {
        self.field.as_deref()
    }
}

impl From<query::QueryError> for FormError {
    fn from(error: query::QueryError) -> Self {
        let (kind, field, detail) = error.into_parts();
        Self {
            kind: kind.into(),
            field,
            detail,
        }
    }
}

/// Whether `headers` declare exactly one form-encoded UTF-8 body.
pub(crate) fn is_form(headers: &HeaderMap) -> bool {
    let mut values = headers.get_all(header::CONTENT_TYPE).iter();
    match (values.next(), values.next()) {
        (Some(value), None) => is_form_media_type(value),
        _ => false,
    }
}

/// `application/x-www-form-urlencoded`, case-insensitive, with at most a
/// `charset` parameter whose value is UTF-8.
fn is_form_media_type(value: &HeaderValue) -> bool {
    let Ok(text) = value.to_str() else {
        return false;
    };
    let mut parts = text.split(';');
    let media_type = parts.next().unwrap_or_default().trim();
    if !media_type.eq_ignore_ascii_case(FORM_CONTENT_TYPE) {
        return false;
    }
    parts.all(|parameter| match parameter.split_once('=') {
        Some((name, value)) => {
            let value = value.trim();
            let value = value
                .strip_prefix('"')
                .and_then(|inner| inner.strip_suffix('"'))
                .unwrap_or(value);
            name.trim().eq_ignore_ascii_case("charset") && value.eq_ignore_ascii_case("utf-8")
        }
        None => false,
    })
}

/// Checks the media type, UTF-8 and field bound, then splits the body.
fn pairs<'b>(headers: &HeaderMap, body: &'b [u8], skip: &[&str]) -> Result<Pairs<'b>, FormError> {
    if !is_form(headers) {
        return Err(FormError::new(FormErrorKind::UnsupportedMediaType));
    }
    let text = std::str::from_utf8(body).map_err(|_| FormError::new(FormErrorKind::Malformed))?;
    if !text.is_empty() && text.split('&').count() > MAX_FORM_FIELDS {
        return Err(FormError::new(FormErrorKind::TooManyFields));
    }
    Ok(query::parse(text, Mode::Form, skip)?)
}

/// Decodes a form body into `T`, skipping [`RESERVED_FORM_FIELDS`].
pub(crate) fn decode<T: DeserializeOwned>(
    headers: &HeaderMap,
    body: &[u8],
) -> Result<T, FormError> {
    let pairs = pairs(headers, body, &RESERVED_FORM_FIELDS)?;
    Ok(query::deserialize(&pairs, Mode::Form)?)
}

/// The single decoded value of `name`, if present.
pub(crate) fn value(
    headers: &HeaderMap,
    body: &[u8],
    name: &str,
) -> Result<Option<String>, FormError> {
    let pairs = pairs(headers, body, &[])?;
    match pairs.get(name).map(Vec::as_slice) {
        None | Some([]) => Ok(None),
        Some([value]) => Ok(Some(value.clone().into_owned())),
        Some(_) => {
            Err(query::QueryError::new(QueryErrorKind::DuplicateParameter, Some(name)).into())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use serde::Deserialize;

    use super::*;

    fn headers(content_type: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_str(content_type).expect("valid header"),
        );
        headers
    }

    fn form() -> HeaderMap {
        headers(FORM_CONTENT_TYPE)
    }

    #[derive(Debug, Deserialize, PartialEq)]
    #[serde(deny_unknown_fields)]
    struct Product {
        name: String,
        price: u32,
        #[serde(default)]
        published: bool,
        notes: Option<String>,
        stock: Option<u32>,
        #[serde(default)]
        tags: Vec<String>,
    }

    fn decode_product(body: &str) -> Result<Product, FormError> {
        decode(&form(), body.as_bytes())
    }

    fn failure(body: &str) -> (FormErrorKind, Option<String>) {
        let error = decode_product(body).expect_err(body);
        (error.kind(), error.field().map(str::to_owned))
    }

    #[test]
    fn decodes_typed_fields_with_form_semantics() {
        let product =
            decode_product("name=Red+mug%21&price=12&published=on&notes=&stock=&tags=a&tags=b%20c")
                .expect("valid form");

        assert_eq!(
            product,
            Product {
                name: "Red mug!".to_owned(),
                price: 12,
                published: true,
                notes: None,
                stock: None,
                tags: vec!["a".to_owned(), "b c".to_owned()],
            }
        );
    }

    #[test]
    fn checkbox_booleans_and_absent_checkboxes() {
        for (value, expected) in [
            ("on", true),
            ("true", true),
            ("1", true),
            ("false", false),
            ("0", false),
        ] {
            let product = decode_product(&format!("name=x&price=1&published={value}"))
                .unwrap_or_else(|error| panic!("{value}: {error}"));
            assert_eq!(product.published, expected, "{value}");
        }
        assert!(
            !decode_product("name=x&price=1")
                .expect("unchecked")
                .published
        );
        for value in ["yes", "", "ON", "off", "2"] {
            assert_eq!(
                failure(&format!("name=x&price=1&published={value}")),
                (FormErrorKind::InvalidValue, Some("published".to_owned())),
                "{value}"
            );
        }
    }

    #[test]
    fn repeated_scalar_fields_are_rejected_but_sequences_accept_them() {
        assert_eq!(
            failure("name=a&name=b&price=1"),
            (FormErrorKind::DuplicateField, Some("name".to_owned()))
        );
        assert_eq!(
            failure("name=a&price=1&price=1"),
            (FormErrorKind::DuplicateField, Some("price".to_owned()))
        );
        assert_eq!(
            failure("name=a&price=1&notes=x&notes=y"),
            (FormErrorKind::DuplicateField, Some("notes".to_owned()))
        );
        let product = decode_product("tags=x&name=a&tags=y&price=1").expect("sequence");
        assert_eq!(product.tags, ["x", "y"]);
    }

    #[test]
    fn reserved_fields_never_reach_the_application_type() {
        let product = decode_product(&format!(
            "{CSRF_FIELD_NAME}=abc&{METHOD_FIELD_NAME}=patch&name=a&price=1&csrf%5Ftoken=again"
        ))
        .expect("reserved fields are skipped even with deny_unknown_fields");
        assert_eq!(product.name, "a");
    }

    #[test]
    fn errors_name_the_field() {
        assert_eq!(
            failure("price=1"),
            (FormErrorKind::MissingField, Some("name".to_owned()))
        );
        assert_eq!(
            failure("name=a&price=ten"),
            (FormErrorKind::InvalidValue, Some("price".to_owned()))
        );
        assert_eq!(
            failure("name=a&price=1&admin=true"),
            (FormErrorKind::UnknownField, Some("admin".to_owned()))
        );
        assert_eq!(
            failure("name=a&price=1&stock=-1"),
            (FormErrorKind::InvalidValue, Some("stock".to_owned()))
        );
        for body in [
            "name=a&&price=1",
            "name",
            "=1",
            "name=%zz",
            "name=%FF&price=1",
        ] {
            assert_eq!(failure(body).0, FormErrorKind::Malformed, "{body}");
        }
    }

    #[test]
    fn content_type_is_strict() {
        for accepted in [
            "application/x-www-form-urlencoded",
            "Application/X-WWW-Form-URLEncoded",
            "application/x-www-form-urlencoded; charset=UTF-8",
            "application/x-www-form-urlencoded;charset=\"utf-8\"",
        ] {
            let parsed: HashMap<String, String> = decode(&headers(accepted), b"a=1")
                .unwrap_or_else(|error| panic!("{accepted}: {error}"));
            assert_eq!(parsed["a"], "1");
        }
        for rejected in [
            "text/plain",
            "multipart/form-data; boundary=x",
            "application/json",
            "application/x-www-form-urlencoded; charset=latin1",
            "application/x-www-form-urlencoded; boundary=x",
            "application/x-www-form-urlencoded;",
            "application/x-www-form-urlencodedx",
        ] {
            let error =
                decode::<HashMap<String, String>>(&headers(rejected), b"a=1").expect_err(rejected);
            assert_eq!(
                error.kind(),
                FormErrorKind::UnsupportedMediaType,
                "{rejected}"
            );
        }
        let error = decode::<HashMap<String, String>>(&HeaderMap::new(), b"a=1")
            .expect_err("missing content type");
        assert_eq!(error.kind(), FormErrorKind::UnsupportedMediaType);

        let mut twice = form();
        twice.append(
            header::CONTENT_TYPE,
            HeaderValue::from_static(FORM_CONTENT_TYPE),
        );
        let error = decode::<HashMap<String, String>>(&twice, b"a=1").expect_err("two types");
        assert_eq!(error.kind(), FormErrorKind::UnsupportedMediaType);
    }

    #[test]
    fn body_must_be_utf8_and_bounded() {
        let error = decode::<HashMap<String, String>>(&form(), b"a=\xff").expect_err("bytes");
        assert_eq!(error.kind(), FormErrorKind::Malformed);

        let at_limit = vec!["a=1"; MAX_FORM_FIELDS].join("&");
        let parsed: HashMap<String, Vec<String>> =
            decode(&form(), at_limit.as_bytes()).expect("at the bound");
        assert_eq!(parsed["a"].len(), MAX_FORM_FIELDS);

        let over = vec!["a=1"; MAX_FORM_FIELDS + 1].join("&");
        let error =
            decode::<HashMap<String, Vec<String>>>(&form(), over.as_bytes()).expect_err("over");
        assert_eq!(error.kind(), FormErrorKind::TooManyFields);
    }

    #[test]
    fn empty_body_decodes_to_defaults() {
        #[derive(Debug, Deserialize)]
        struct Optional {
            #[serde(default)]
            flag: bool,
            note: Option<String>,
        }
        let parsed: Optional = decode(&form(), b"").expect("empty form");
        assert!(!parsed.flag);
        assert!(parsed.note.is_none());
    }

    #[test]
    fn value_returns_one_field_including_reserved_ones() {
        let body = format!("{METHOD_FIELD_NAME}=DELETE&{CSRF_FIELD_NAME}=t&x=1");
        assert_eq!(
            value(&form(), body.as_bytes(), METHOD_FIELD_NAME).expect("valid"),
            Some("DELETE".to_owned())
        );
        assert_eq!(
            value(&form(), body.as_bytes(), "absent").expect("valid"),
            None
        );
        let repeated =
            value(&form(), b"_method=PUT&_method=DELETE", METHOD_FIELD_NAME).expect_err("repeated");
        assert_eq!(repeated.kind(), FormErrorKind::DuplicateField);
        assert_eq!(repeated.field(), Some(METHOD_FIELD_NAME));
        let wrong_type =
            value(&headers("application/json"), b"{}", METHOD_FIELD_NAME).expect_err("json");
        assert_eq!(wrong_type.kind(), FormErrorKind::UnsupportedMediaType);
    }

    #[test]
    fn display_is_for_logs_and_kind_message_is_caller_safe() {
        let error = decode_product("name=a&price=secret-input").expect_err("invalid");
        assert!(
            error
                .to_string()
                .starts_with(FormErrorKind::InvalidValue.message())
        );
        assert!(!FormErrorKind::InvalidValue.message().contains("secret"));
    }
}
