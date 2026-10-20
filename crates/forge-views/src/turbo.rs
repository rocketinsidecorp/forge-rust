//! Turbo-compatible responses: redirects after form submissions, Turbo
//! Stream bodies, and DOM identifiers that are safe to embed in markup.

use std::{fmt, str::FromStr};

use askama::Template;
use forge_core::Id;
use forge_http::{HeaderValue, Request, Response, StatusCode, Uri, header};
use thiserror::Error;

/// The media type Turbo requires for stream responses.
pub const TURBO_STREAM_MEDIA_TYPE: &str = "text/vnd.turbo-stream.html";
const TURBO_STREAM_CONTENT_TYPE: &str = "text/vnd.turbo-stream.html; charset=utf-8";

/// Maximum length of a [`DomId`], in bytes.
pub const MAX_DOM_ID_BYTES: usize = 128;

// ---------------------------------------------------------------- DOM ids

/// A validated DOM element identifier: `[A-Za-z][A-Za-z0-9_:-]*`, at most
/// [`MAX_DOM_ID_BYTES`] bytes.
///
/// The character set contains nothing that can close an attribute, open a tag
/// or start an entity, so a `DomId` can be written into `id="..."` and
/// `target="..."` without further escaping.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct DomId(String);

/// A string that is not a valid [`DomId`].
#[derive(Debug, Error, Eq, PartialEq)]
#[error(
    "invalid DOM id: must match [A-Za-z][A-Za-z0-9_:-]* and be at most {MAX_DOM_ID_BYTES} bytes"
)]
pub struct DomIdError;

impl DomId {
    /// Validates `value`.
    pub fn new(value: impl Into<String>) -> Result<Self, DomIdError> {
        let value = value.into();
        let mut bytes = value.bytes();
        let valid = value.len() <= MAX_DOM_ID_BYTES
            && bytes
                .next()
                .is_some_and(|first| first.is_ascii_alphabetic())
            && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b':' | b'-'));
        if valid {
            Ok(Self(value))
        } else {
            Err(DomIdError)
        }
    }

    /// The identifier text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DomId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl AsRef<str> for DomId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl FromStr for DomId {
    type Err = DomIdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

/// The DOM id for a record, like Rails' `dom_id`: `{prefix}_{uuid}`, for
/// example `product_0190...` for `dom_id(id, "product")`. The prefix must be
/// valid on its own (start with a letter), because UUIDs may start with a
/// digit.
///
/// ```
/// use forge_core::Id;
///
/// enum Product {}
/// let id = Id::<Product>::new();
/// let dom = forge_views::dom_id::<Product>(id, "product").expect("valid prefix");
/// assert_eq!(dom.as_str(), format!("product_{id}"));
/// assert!(forge_views::dom_id::<Product>(id, "9bad").is_err());
/// ```
pub fn dom_id<T>(id: Id<T>, prefix: &str) -> Result<DomId, DomIdError> {
    DomId::new(format!("{prefix}_{id}"))
}

// ---------------------------------------------------------- Turbo Streams

/// What a Turbo Stream element does to its target.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum StreamAction {
    /// Adds the template at the end of the target's children.
    Append,
    /// Adds the template at the start of the target's children.
    Prepend,
    /// Replaces the target element itself.
    Replace,
    /// Replaces the target element's children.
    Update,
    /// Removes the target element.
    Remove,
    /// Inserts the template before the target element.
    Before,
    /// Inserts the template after the target element.
    After,
}

impl StreamAction {
    /// The `action` attribute value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Append => "append",
            Self::Prepend => "prepend",
            Self::Replace => "replace",
            Self::Update => "update",
            Self::Remove => "remove",
            Self::Before => "before",
            Self::After => "after",
        }
    }
}

/// An invalid Turbo Stream action.
#[derive(Debug, Error)]
pub enum TurboStreamError {
    /// Every action except `remove` needs a template.
    #[error("turbo stream action {0} requires a template")]
    MissingTemplate(&'static str),
    /// `remove` takes no template.
    #[error("turbo stream action remove does not take a template")]
    UnexpectedTemplate,
    /// The template view failed to render.
    #[error("turbo stream template failed to render")]
    Render(#[source] askama::Error),
}

/// One `<turbo-stream>` element.
///
/// The `template` is trusted, already-rendered HTML: render it with a view
/// (see [`from_view`](Self::from_view)) so user data inside it is escaped.
/// The target is a validated [`DomId`], so it cannot inject attributes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TurboStreamAction {
    action: StreamAction,
    target: DomId,
    template: Option<String>,
}

impl TurboStreamAction {
    /// Creates an action. `remove` must not have a template; every other
    /// action must.
    pub fn new(
        action: StreamAction,
        target: DomId,
        template: Option<String>,
    ) -> Result<Self, TurboStreamError> {
        match (action, &template) {
            (StreamAction::Remove, Some(_)) => Err(TurboStreamError::UnexpectedTemplate),
            (StreamAction::Remove, None) | (_, Some(_)) => Ok(Self {
                action,
                target,
                template,
            }),
            (other, None) => Err(TurboStreamError::MissingTemplate(other.as_str())),
        }
    }

    /// Creates a `remove` action.
    #[must_use]
    pub fn remove(target: DomId) -> Self {
        Self {
            action: StreamAction::Remove,
            target,
            template: None,
        }
    }

    /// Creates an action whose template is `view` rendered (and escaped) by
    /// Askama.
    pub fn from_view(
        action: StreamAction,
        target: DomId,
        view: &impl Template,
    ) -> Result<Self, TurboStreamError> {
        let html = view.render().map_err(TurboStreamError::Render)?;
        Self::new(action, target, Some(html))
    }

    /// The action.
    #[must_use]
    pub fn action(&self) -> StreamAction {
        self.action
    }

    /// The target element id.
    #[must_use]
    pub fn target(&self) -> &DomId {
        &self.target
    }

    /// The rendered template, if any.
    #[must_use]
    pub fn template(&self) -> Option<&str> {
        self.template.as_deref()
    }

    fn render_into(&self, out: &mut String) {
        out.push_str("<turbo-stream action=\"");
        out.push_str(self.action.as_str());
        out.push_str("\" target=\"");
        out.push_str(self.target.as_str());
        out.push_str("\">");
        if let Some(template) = &self.template {
            out.push_str("<template>");
            out.push_str(template);
            out.push_str("</template>");
        }
        out.push_str("</turbo-stream>\n");
    }
}

/// The markup of `actions`, in order: one `<turbo-stream>` element per
/// action, each followed by a newline. It is the body of a Turbo Stream
/// response and the payload of a broadcast.
#[must_use]
pub fn turbo_stream_html(actions: &[TurboStreamAction]) -> String {
    let mut body = String::new();
    for action in actions {
        action.render_into(&mut body);
    }
    body
}

/// A `200 OK` Turbo Stream response (`text/vnd.turbo-stream.html`) carrying
/// `actions` in order.
///
/// ```
/// use forge_views::{DomId, StreamAction, TurboStreamAction};
///
/// let target = DomId::new("flash").expect("valid id");
/// let action = TurboStreamAction::new(StreamAction::Update, target, Some("<p>Saved</p>".into()))
///     .expect("update has a template");
/// let response = forge_views::turbo_stream(&[action]);
/// assert_eq!(
///     response.body().as_ref(),
///     b"<turbo-stream action=\"update\" target=\"flash\"><template><p>Saved</p></template></turbo-stream>\n"
/// );
/// ```
#[must_use]
pub fn turbo_stream(actions: &[TurboStreamAction]) -> Response {
    turbo_stream_with_status(StatusCode::OK, actions)
}

/// A Turbo Stream response with `status`, for example `422` for a form that
/// failed validation (Turbo applies stream responses to form submissions
/// with any status). It varies by `Accept`, because the same URL answers
/// HTML or a redirect to clients that did not ask for a stream (see
/// [`turbo_stream_requested`]).
#[must_use]
pub fn turbo_stream_with_status(status: StatusCode, actions: &[TurboStreamAction]) -> Response {
    let mut response = Response::new(status, turbo_stream_html(actions));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(TURBO_STREAM_CONTENT_TYPE),
    );
    response
        .headers_mut()
        .append(header::VARY, HeaderValue::from_static("accept"));
    response
}

/// Whether the client asked for a Turbo Stream response: its `Accept`
/// header names `text/vnd.turbo-stream.html` itself with a non-zero
/// quality. Turbo adds it to every form submission; browser navigations,
/// `*/*` and API clients do not, so they get the HTML page or redirect.
///
/// ```
/// use forge_http::{HeaderMap, HeaderValue, Method, Request};
///
/// let mut headers = HeaderMap::new();
/// headers.insert(
///     "accept",
///     HeaderValue::from_static("text/vnd.turbo-stream.html, text/html, application/xhtml+xml"),
/// );
/// let request = Request::new_for_test(
///     Method::POST,
///     "/products".parse().expect("uri"),
///     headers,
///     bytes::Bytes::new(),
///     "127.0.0.1:4000".parse().expect("address"),
///     uuid::Uuid::now_v7(),
/// );
/// assert!(forge_views::turbo_stream_requested(&request));
/// ```
#[must_use]
pub fn turbo_stream_requested(request: &Request) -> bool {
    request
        .headers()
        .get_all(header::ACCEPT)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|range| {
            let mut parts = range.split(';');
            let media = parts.next().unwrap_or_default().trim();
            if !media.eq_ignore_ascii_case(TURBO_STREAM_MEDIA_TYPE) {
                return false;
            }
            let quality = parts
                .filter_map(|parameter| {
                    let (name, value) = parameter.split_once('=')?;
                    name.trim()
                        .eq_ignore_ascii_case("q")
                        .then(|| crate::sign_in::quality(value.trim()))
                })
                .next_back()
                .unwrap_or(Some(1000));
            quality.is_some_and(|quality| quality > 0)
        })
}

// -------------------------------------------------------------- redirects

/// A redirect target that was refused.
#[derive(Debug, Error, Eq, PartialEq)]
pub enum RedirectError {
    /// The location is not an absolute path on this site (it must start with a
    /// single `/`).
    #[error("redirect location must be a path starting with a single '/'")]
    NotRelative,
    /// The location contains whitespace, control, non-ASCII or backslash
    /// characters, which browsers normalize in ways that can change the host.
    #[error("redirect location contains forbidden characters")]
    ForbiddenCharacters,
    /// The allowed origin is not a bare `http(s)://host[:port]` origin.
    #[error("allowed origin must be http(s)://host[:port] without credentials or path")]
    InvalidOrigin,
    /// The absolute location does not belong to the allowed origin.
    #[error("redirect location is not on the allowed origin")]
    CrossOrigin,
}

/// A `303 See Other` redirect to a path on this site.
///
/// Turbo requires `303` (not `302`) after a non-GET form submission so the
/// follow-up request is a `GET`. To prevent open redirects the location must
/// be a path starting with a single `/`: absolute URLs, scheme-relative
/// `//host` and `/\host` are refused, as are whitespace, control, non-ASCII
/// and backslash characters. Percent-encode anything else. Use
/// [`see_other_to_origin`] to redirect to an absolute same-origin URL.
///
/// ```
/// let response = forge_views::see_other("/products/1?tab=details")?;
/// assert_eq!(response.status(), forge_http::StatusCode::SEE_OTHER);
/// assert!(forge_views::see_other("https://evil.example/").is_err());
/// # Ok::<(), forge_views::RedirectError>(())
/// ```
pub fn see_other(location: &str) -> Result<Response, RedirectError> {
    check_characters(location)?;
    let mut characters = location.bytes();
    if characters.next() != Some(b'/') || matches!(characters.next(), Some(b'/' | b'\\')) {
        return Err(RedirectError::NotRelative);
    }
    redirect(location)
}

/// A `303 See Other` redirect to an absolute URL, allowed only when its
/// origin (scheme, host and port) equals `allowed_origin`, for example
/// `https://app.example.com`. Credentials in the URL are refused. A plain
/// path is accepted under the same rules as [`see_other`].
pub fn see_other_to_origin(
    location: &str,
    allowed_origin: &str,
) -> Result<Response, RedirectError> {
    check_characters(location)?;
    let allowed = parse_origin(allowed_origin)?;
    if location.starts_with('/') {
        return see_other(location);
    }
    let uri: Uri = location.parse().map_err(|_| RedirectError::NotRelative)?;
    let origin = origin_of(&uri).ok_or(RedirectError::NotRelative)?;
    if origin != allowed {
        return Err(RedirectError::CrossOrigin);
    }
    redirect(location)
}

/// Visible ASCII only, and no backslash.
fn check_characters(location: &str) -> Result<(), RedirectError> {
    if location
        .bytes()
        .all(|byte| (0x21..=0x7e).contains(&byte) && byte != b'\\')
    {
        Ok(())
    } else {
        Err(RedirectError::ForbiddenCharacters)
    }
}

/// `scheme://authority`, lowercased, or `None` for relative URIs and
/// authorities that carry credentials.
fn origin_of(uri: &Uri) -> Option<String> {
    let scheme = uri.scheme_str()?.to_ascii_lowercase();
    let authority = uri.authority()?;
    if !matches!(scheme.as_str(), "http" | "https") || authority.as_str().contains('@') {
        return None;
    }
    Some(format!(
        "{scheme}://{}",
        authority.as_str().to_ascii_lowercase()
    ))
}

fn parse_origin(origin: &str) -> Result<String, RedirectError> {
    check_characters(origin).map_err(|_| RedirectError::InvalidOrigin)?;
    let uri: Uri = origin.parse().map_err(|_| RedirectError::InvalidOrigin)?;
    let bare = uri.query().is_none() && matches!(uri.path(), "" | "/");
    match origin_of(&uri) {
        Some(parsed) if bare => Ok(parsed),
        _ => Err(RedirectError::InvalidOrigin),
    }
}

fn redirect(location: &str) -> Result<Response, RedirectError> {
    let value = HeaderValue::from_str(location).map_err(|_| RedirectError::ForbiddenCharacters)?;
    let mut response = Response::empty(StatusCode::SEE_OTHER);
    response.headers_mut().insert(header::LOCATION, value);
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{header as header_of, text};

    enum Product {}

    fn id(value: &str) -> DomId {
        DomId::new(value).expect("valid DOM id")
    }

    #[derive(Template)]
    #[template(source = "<li>{{ name }}</li>", ext = "html")]
    struct Item<'a> {
        name: &'a str,
    }

    // ---- DomId

    #[test]
    fn dom_ids_accept_the_documented_alphabet() {
        for valid in ["a", "flash", "product_0190", "Row:1", "a-b_c:d", "x9"] {
            assert_eq!(DomId::new(valid).expect(valid).as_str(), valid);
        }
        assert!(DomId::new("a".repeat(MAX_DOM_ID_BYTES)).is_ok());
    }

    #[test]
    fn dom_ids_reject_injection_attempts() {
        for invalid in [
            "",
            "1abc",
            "_a",
            "-a",
            ":a",
            "a b",
            "a\"b",
            "a'b",
            "a<b",
            "a>b",
            "a&b",
            "a=b",
            "a\nb",
            "a\"><script>alert(1)</script>",
            "a\" onload=\"x",
            "é",
            "a.b",
            "a/b",
        ] {
            assert_eq!(DomId::new(invalid), Err(DomIdError), "{invalid:?}");
        }
        assert!(DomId::new("a".repeat(MAX_DOM_ID_BYTES + 1)).is_err());
        assert!("a b".parse::<DomId>().is_err());
    }

    #[test]
    fn dom_id_combines_prefix_and_uuid() {
        let record = Id::<Product>::new();

        let dom = dom_id::<Product>(record, "product").expect("valid");

        assert_eq!(dom.to_string(), format!("product_{record}"));
        assert_eq!(dom.as_ref(), format!("product_{record}"));
        assert!(dom_id::<Product>(record, "").is_err());
        assert!(dom_id::<Product>(record, "9").is_err());
        assert!(dom_id::<Product>(record, "a\" x=\"").is_err());
    }

    // ---- Turbo Streams

    #[test]
    fn stream_actions_render_their_names() {
        for (action, name) in [
            (StreamAction::Append, "append"),
            (StreamAction::Prepend, "prepend"),
            (StreamAction::Replace, "replace"),
            (StreamAction::Update, "update"),
            (StreamAction::Remove, "remove"),
            (StreamAction::Before, "before"),
            (StreamAction::After, "after"),
        ] {
            assert_eq!(action.as_str(), name);
        }
    }

    #[test]
    fn turbo_stream_renders_markup_and_content_type() {
        let actions = [
            TurboStreamAction::new(
                StreamAction::Append,
                id("products"),
                Some("<li>One</li>".to_owned()),
            )
            .expect("append"),
            TurboStreamAction::remove(id("product_1")),
        ];

        let response = turbo_stream(&actions);

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            header_of(&response, "content-type"),
            Some("text/vnd.turbo-stream.html; charset=utf-8")
        );
        assert_eq!(
            text(&response),
            "<turbo-stream action=\"append\" target=\"products\"><template><li>One</li></template></turbo-stream>\n\
             <turbo-stream action=\"remove\" target=\"product_1\"></turbo-stream>\n"
        );
    }

    #[test]
    fn every_template_action_wraps_its_html() {
        for action in [
            StreamAction::Append,
            StreamAction::Prepend,
            StreamAction::Replace,
            StreamAction::Update,
            StreamAction::Before,
            StreamAction::After,
        ] {
            let built =
                TurboStreamAction::new(action, id("t"), Some("<p>x</p>".to_owned())).expect("ok");
            let body = text(&turbo_stream(&[built]));
            assert_eq!(
                body,
                format!(
                    "<turbo-stream action=\"{}\" target=\"t\"><template><p>x</p></template></turbo-stream>\n",
                    action.as_str()
                )
            );
        }
    }

    #[test]
    fn templates_are_required_except_for_remove() {
        assert!(matches!(
            TurboStreamAction::new(StreamAction::Append, id("t"), None),
            Err(TurboStreamError::MissingTemplate("append"))
        ));
        assert!(matches!(
            TurboStreamAction::new(StreamAction::Remove, id("t"), Some("<p>".to_owned())),
            Err(TurboStreamError::UnexpectedTemplate)
        ));
        let removal = TurboStreamAction::new(StreamAction::Remove, id("t"), None).expect("remove");
        assert_eq!(removal.action(), StreamAction::Remove);
        assert_eq!(removal.target().as_str(), "t");
        assert_eq!(removal.template(), None);
    }

    #[test]
    fn from_view_escapes_user_data() {
        let built = TurboStreamAction::from_view(
            StreamAction::Prepend,
            id("items"),
            &Item {
                name: "<img src=x onerror=alert(1)>",
            },
        )
        .expect("renders");

        let body = text(&turbo_stream(&[built]));

        assert!(!body.contains("<img"), "{body}");
        assert!(body.contains("&#60;img"), "{body}");
    }

    #[test]
    fn stream_responses_take_a_status_and_vary_by_accept() {
        let removal = TurboStreamAction::remove(id("row_1"));

        let response = turbo_stream_with_status(
            StatusCode::UNPROCESSABLE_ENTITY,
            std::slice::from_ref(&removal),
        );

        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(header_of(&response, "vary"), Some("accept"));
        assert_eq!(
            text(&response),
            turbo_stream_html(std::slice::from_ref(&removal))
        );
        assert_eq!(
            turbo_stream_html(&[removal]),
            "<turbo-stream action=\"remove\" target=\"row_1\"></turbo-stream>\n"
        );
    }

    #[test]
    fn only_an_explicit_accept_asks_for_a_stream() {
        use crate::test_support::request_with;
        use forge_http::Method;

        for accept in [
            "text/vnd.turbo-stream.html, text/html, application/xhtml+xml",
            "TEXT/VND.TURBO-STREAM.HTML",
            "text/html, text/vnd.turbo-stream.html;q=0.5",
        ] {
            let request = request_with(Method::POST, "/", &[("accept", accept)]);
            assert!(turbo_stream_requested(&request), "{accept}");
        }
        for accept in [
            "*/*",
            "text/*",
            "text/html",
            "application/json",
            "text/vnd.turbo-stream.html;q=0",
            "text/vnd.turbo-stream.html;q=0.000",
            "text/vnd.turbo-stream.html;q=bogus",
            "text/vnd.turbo-stream.htmlx",
        ] {
            let request = request_with(Method::POST, "/", &[("accept", accept)]);
            assert!(!turbo_stream_requested(&request), "{accept}");
        }
        assert!(!turbo_stream_requested(&request_with(
            Method::POST,
            "/",
            &[]
        )));
    }

    #[test]
    fn an_empty_action_list_is_an_empty_stream() {
        let response = turbo_stream(&[]);

        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.body().is_empty());
    }

    // ---- redirects

    #[test]
    fn see_other_redirects_to_relative_paths() {
        for location in ["/", "/products", "/products/1?tab=a&b=c#frag", "/a%2Fb"] {
            let response = see_other(location).expect(location);

            assert_eq!(response.status(), StatusCode::SEE_OTHER, "{location}");
            assert_eq!(header_of(&response, "location"), Some(location));
            assert!(response.body().is_empty());
        }
    }

    #[test]
    fn see_other_rejects_open_redirect_vectors() {
        for location in [
            "",
            "products",
            "https://evil.example/",
            "http://evil.example",
            "//evil.example",
            "///evil.example",
            "/\\evil.example",
            "\\\\evil.example",
            "/\t/evil.example",
            "/\n/evil.example",
            " //evil.example",
            "javascript:alert(1)",
            "data:text/html,x",
            "mailto:a@b.c",
            "/é",
            "/a b",
            "/a\r\nSet-Cookie: x=1",
        ] {
            assert!(see_other(location).is_err(), "{location:?}");
        }
        assert_eq!(
            see_other("//evil.example").map(|_| ()),
            Err(RedirectError::NotRelative)
        );
    }

    #[test]
    fn same_origin_absolute_redirects_are_opt_in() {
        let response = see_other_to_origin(
            "https://app.example.com/orders/1?x=1",
            "https://app.example.com",
        )
        .expect("same origin");

        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            header_of(&response, "location"),
            Some("https://app.example.com/orders/1?x=1")
        );
        // Scheme and host compare case-insensitively; a trailing slash on the
        // configured origin is fine.
        assert!(
            see_other_to_origin("HTTPS://APP.example.com/x", "https://app.example.com/").is_ok()
        );
        assert!(see_other_to_origin("http://localhost:3000/x", "http://localhost:3000").is_ok());
        // Plain paths are still fine.
        assert!(see_other_to_origin("/relative", "https://app.example.com").is_ok());
    }

    #[test]
    fn cross_origin_and_tricky_absolute_redirects_are_refused() {
        let origin = "https://app.example.com";
        for location in [
            "https://evil.example/",
            "http://app.example.com/",
            "https://app.example.com:8443/",
            "https://app.example.com.evil.example/",
            "https://app.example.com@evil.example/",
            "https://evil.example@app.example.com/",
            "https://app.example.com\\@evil.example/",
            "//app.example.com/x",
            "ftp://app.example.com/",
            "https://app.example.com /x",
        ] {
            assert!(
                see_other_to_origin(location, origin).is_err(),
                "{location:?}"
            );
        }
    }

    #[test]
    fn allowed_origins_must_be_bare_origins() {
        for origin in [
            "",
            "app.example.com",
            "https://app.example.com/path",
            "https://app.example.com?x=1",
            "https://user@app.example.com",
            "ftp://app.example.com",
            "/",
        ] {
            assert_eq!(
                see_other_to_origin("https://app.example.com/", origin).map(|_| ()),
                Err(RedirectError::InvalidOrigin),
                "{origin:?}"
            );
        }
    }
}
