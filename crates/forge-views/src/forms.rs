//! Form helpers: CSRF markup, HTTP method tunnelling and field errors.

use forge_auth::CsrfToken;
use forge_http::{FormError, Method};

use crate::csrf::mask_csrf_token;

/// Header that carries the CSRF token on `fetch`/Turbo requests
/// (`x-csrf-token`), read by [`CsrfProtection`](crate::CsrfProtection) and
/// by the session-bound check in generated applications.
pub const CSRF_HEADER_NAME: &str = "x-csrf-token";

/// `name` of the `<meta>` element that exposes the token to Turbo and
/// `fetch` (ADR 0022).
pub const CSRF_META_NAME: &str = "csrf-token";

/// `name` of the hidden form field that carries the token in HTML form
/// submissions (`csrf_token`). [`CsrfProtection`](crate::CsrfProtection)
/// reads it with the same rules as [`CSRF_HEADER_NAME`];
/// `forge_http::Request::form` never passes it to the application's type.
pub const CSRF_FIELD_NAME: &str = forge_http::CSRF_FIELD_NAME;

/// `name` of the form field that tunnels `PATCH`, `PUT` and `DELETE`
/// through `POST` (`_method`).
pub const METHOD_FIELD_NAME: &str = forge_http::METHOD_FIELD_NAME;

/// The [`FieldErrors`] key for errors that concern the whole form.
pub const BASE_FIELD: &str = "base";

/// `<meta name="csrf-token" content="...">` for the layout `<head>`, so
/// Turbo and scripts can send the token in [`CSRF_HEADER_NAME`].
///
/// The content is the token masked with a fresh one-time pad
/// ([`mask_csrf_token`]), so no two renders embed the same bytes (BREACH).
/// The result is markup: insert it with `|safe` in a template. The value is
/// HTML-escaped here even though a masked token is base64url by
/// construction. Should the CSPRNG fail, the content is empty and the page's
/// requests are refused (fail closed); the failure is logged.
#[must_use]
pub fn csrf_meta(token: &CsrfToken) -> String {
    meta_markup(&masked(token))
}

/// `<input type="hidden" name="csrf_token" value="...">` for every non-GET
/// form, with the token masked afresh on every call as in [`csrf_meta`].
/// Insert it with `|safe` in a template.
#[must_use]
pub fn csrf_field(token: &CsrfToken) -> String {
    field_markup(&masked(token))
}

/// `token` masked, or empty (a form that is then refused) when the CSPRNG
/// failed.
fn masked(token: &CsrfToken) -> String {
    mask_csrf_token(token).unwrap_or_else(|failure| {
        tracing::error!(error = %failure, "CSRF token masking failed");
        String::new()
    })
}

fn meta_markup(value: &str) -> String {
    format!(
        "<meta name=\"{CSRF_META_NAME}\" content=\"{}\">",
        escape_attribute(value)
    )
}

fn field_markup(value: &str) -> String {
    format!(
        "<input type=\"hidden\" name=\"{CSRF_FIELD_NAME}\" value=\"{}\">",
        escape_attribute(value)
    )
}

/// Escapes text for a double- or single-quoted HTML attribute (and for text
/// content).
fn escape_attribute(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

/// Validation messages keyed by form field, for rendering a form again with
/// `422 Unprocessable Entity` next to the offending inputs.
///
/// ```
/// use forge_views::FieldErrors;
///
/// let mut errors = FieldErrors::new();
/// errors.add("name", "can't be blank");
/// assert_eq!(errors.get("name"), Some("can't be blank"));
/// assert!(errors.get("price").is_none());
/// ```
///
/// Templates read it with `{% if let Some(message) = errors.get("name") %}`;
/// messages are escaped like any other value.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FieldErrors {
    entries: Vec<(String, String)>,
}

impl FieldErrors {
    /// No errors.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records `message` for `field` (use [`BASE_FIELD`] for the whole form).
    pub fn add(&mut self, field: impl Into<String>, message: impl Into<String>) {
        self.entries.push((field.into(), message.into()));
    }

    /// The first message recorded for `field`.
    #[must_use]
    pub fn get(&self, field: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|(name, _)| name == field)
            .map(|(_, message)| message.as_str())
    }

    /// Whether no error was recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The number of recorded errors.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Every `(field, message)` pair in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.entries
            .iter()
            .map(|(field, message)| (field.as_str(), message.as_str()))
    }
}

impl From<&FormError> for FieldErrors {
    /// The caller-safe message of a form decoding failure, on its field (or
    /// [`BASE_FIELD`] when it concerns the whole body). Request input is
    /// never copied into the message.
    fn from(error: &FormError) -> Self {
        let mut errors = Self::new();
        errors.add(error.field().unwrap_or(BASE_FIELD), error.kind().message());
        errors
    }
}

/// Proof, at the call site, that the request's CSRF token has already been
/// validated.
///
/// Method tunnelling turns a `POST` into a state-changing `PATCH`, `PUT` or
/// `DELETE`; it must never be honored for a request whose token has not been
/// checked, or a cross-site form could trigger deletions. This witness is
/// required by [`method_override`] so that contract is visible in the code
/// that calls it. Construct it only in the code path that follows a
/// successful check.
#[derive(Clone, Copy, Debug)]
pub struct CsrfVerified(());

impl CsrfVerified {
    /// Records that CSRF validation succeeded for the current request.
    ///
    /// Call this only after the CSRF token (hidden field or header) has been
    /// verified. [`CsrfProtection`](crate::CsrfProtection) does this itself
    /// and exposes the proof through [`csrf_verified`](crate::csrf_verified).
    #[must_use]
    pub fn after_check() -> Self {
        Self(())
    }
}

/// The method a form submission should be routed as.
///
/// Browsers submit HTML forms as `GET` or `POST` only. A form can carry the
/// real method in a [`METHOD_FIELD_NAME`] (`_method`) field; this function
/// applies it under strict rules:
///
/// - only when the request method is `POST` (a `GET` with `?_method=DELETE`
///   is never honored);
/// - only the values `PATCH`, `PUT` and `DELETE`, compared ASCII
///   case-insensitively (`POST`, `GET`, `TRACE`, unknown or empty values leave
///   the request as `POST`);
/// - only after CSRF validation, which the [`CsrfVerified`] witness makes
///   explicit.
///
/// `field` is the decoded value of the `_method` form field, if present.
///
/// ```
/// use forge_http::Method;
/// use forge_views::{CsrfVerified, method_override};
///
/// let verified = CsrfVerified::after_check();
/// assert_eq!(method_override(verified, &Method::POST, Some("delete")), Method::DELETE);
/// assert_eq!(method_override(verified, &Method::GET, Some("DELETE")), Method::GET);
/// ```
#[must_use]
pub fn method_override(_verified: CsrfVerified, method: &Method, field: Option<&str>) -> Method {
    if *method != Method::POST {
        return method.clone();
    }
    match field {
        Some(value) if value.eq_ignore_ascii_case("PATCH") => Method::PATCH,
        Some(value) if value.eq_ignore_ascii_case("PUT") => Method::PUT,
        Some(value) if value.eq_ignore_ascii_case("DELETE") => Method::DELETE,
        _ => Method::POST,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn token() -> CsrfToken {
        CsrfToken::new(TOKEN).expect("valid token")
    }

    /// The value between `prefix` and the next `"`.
    fn embedded<'a>(markup: &'a str, prefix: &str) -> &'a str {
        let rest = markup.strip_prefix(prefix).expect("the expected element");
        rest.split('"').next().unwrap_or_default()
    }

    /// Both render the token masked, with a fresh pad each time: never the
    /// token itself, never the same bytes twice, always the token unmasked.
    #[test]
    fn meta_and_field_render_the_token_masked_afresh() {
        let meta_prefix = "<meta name=\"csrf-token\" content=\"";
        let field_prefix = "<input type=\"hidden\" name=\"csrf_token\" value=\"";
        let mut seen = std::collections::HashSet::new();
        for _ in 0..8 {
            let meta = csrf_meta(&token());
            let field = csrf_field(&token());
            assert!(
                meta.ends_with("\">") && field.ends_with("\">"),
                "{meta} {field}"
            );
            for value in [embedded(&meta, meta_prefix), embedded(&field, field_prefix)] {
                assert_eq!(value.len(), crate::MASKED_CSRF_TOKEN_LEN, "{value}");
                assert!(!value.contains(TOKEN));
                assert_eq!(
                    crate::unmask_csrf_token(value).map(|token| token.expose().to_owned()),
                    Ok(TOKEN.to_owned())
                );
                assert!(seen.insert(value.to_owned()), "a mask was reused: {value}");
            }
        }
    }

    #[test]
    fn markup_escapes_attribute_breaking_values() {
        let hostile = "\"><script>alert('x')&</script>";

        let meta = meta_markup(hostile);
        let field = field_markup(hostile);

        for markup in [&meta, &field] {
            assert!(!markup.contains("<script>"), "{markup}");
            assert!(!markup.contains("\"><"), "{markup}");
            assert!(
                markup.contains("&quot;&gt;&lt;script&gt;alert(&#39;x&#39;)&amp;&lt;/script&gt;"),
                "{markup}"
            );
        }
        assert_eq!(meta.matches('"').count(), 4);
        assert_eq!(field.matches('"').count(), 6);
    }

    #[test]
    fn field_errors_keep_the_first_message_per_field() {
        let mut errors = FieldErrors::new();
        assert!(errors.is_empty());
        errors.add("name", "can't be blank");
        errors.add("name", "is too short");
        errors.add(BASE_FIELD, "try again");
        assert_eq!(errors.len(), 3);
        assert_eq!(errors.get("name"), Some("can't be blank"));
        assert_eq!(errors.get(BASE_FIELD), Some("try again"));
        assert_eq!(errors.iter().count(), 3);
    }

    #[test]
    fn field_errors_from_form_errors_use_caller_safe_messages() {
        #[derive(Debug, serde::Deserialize)]
        struct Input {
            #[serde(rename = "price")]
            _price: u32,
        }
        let mut headers = forge_http::HeaderMap::new();
        headers.insert(
            forge_http::header::CONTENT_TYPE,
            forge_http::HeaderValue::from_static("application/x-www-form-urlencoded"),
        );
        let request = forge_http::Request::new_for_test(
            Method::POST,
            forge_http::Uri::from_static("/"),
            headers,
            bytes::Bytes::from_static(b"price=secret"),
            "127.0.0.1:4000".parse().expect("address"),
            uuid::Uuid::now_v7(),
        );
        let error = request.form::<Input>().expect_err("invalid price");

        let errors = FieldErrors::from(&error);

        assert_eq!(errors.get("price"), Some("form field has an invalid value"));
        assert!(
            errors
                .iter()
                .all(|(_, message)| !message.contains("secret"))
        );
    }

    #[test]
    fn field_names_are_the_reserved_form_fields() {
        assert_eq!(CSRF_FIELD_NAME, "csrf_token");
        assert_eq!(METHOD_FIELD_NAME, "_method");
    }

    #[test]
    fn header_name_matches_the_generated_application_check() {
        assert_eq!(CSRF_HEADER_NAME, "x-csrf-token");
        assert_eq!(CSRF_META_NAME, "csrf-token");
    }

    fn resolve(method: Method, field: Option<&str>) -> Method {
        method_override(CsrfVerified::after_check(), &method, field)
    }

    #[test]
    fn post_with_patch_put_delete_is_honored() {
        assert_eq!(resolve(Method::POST, Some("PATCH")), Method::PATCH);
        assert_eq!(resolve(Method::POST, Some("PUT")), Method::PUT);
        assert_eq!(resolve(Method::POST, Some("DELETE")), Method::DELETE);
        assert_eq!(resolve(Method::POST, Some("delete")), Method::DELETE);
        assert_eq!(resolve(Method::POST, Some("Put")), Method::PUT);
    }

    #[test]
    fn non_post_requests_ignore_the_field() {
        for method in [
            Method::GET,
            Method::HEAD,
            Method::PUT,
            Method::PATCH,
            Method::DELETE,
            Method::OPTIONS,
        ] {
            assert_eq!(resolve(method.clone(), Some("DELETE")), method);
        }
    }

    #[test]
    fn other_values_leave_post_as_post() {
        for value in [
            Some("GET"),
            Some("POST"),
            Some("TRACE"),
            Some("CONNECT"),
            Some("HEAD"),
            Some("OPTIONS"),
            Some("DELETE "),
            Some(" DELETE"),
            Some("DELETE\n"),
            Some("DELET"),
            Some("custom"),
            Some(""),
            None,
        ] {
            assert_eq!(resolve(Method::POST, value), Method::POST, "{value:?}");
        }
    }
}
