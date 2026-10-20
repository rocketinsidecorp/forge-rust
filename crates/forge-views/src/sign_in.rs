//! Sending signed-out browsers to the sign-in page (ADR 0026).
//!
//! Access policies answer `401 Unauthorized` when a request has no valid
//! session. [`SignInRedirect`] turns that answer into a `303 See Other` to
//! the sign-in page for clients that prefer HTML ([`prefers_html`]), with
//! the page they asked for as a validated `return_to` parameter (`GET` and
//! `HEAD` only: a form submission cannot be replayed). JSON and other API
//! clients keep the `401`.
//!
//! Every return path that comes from a request goes through
//! [`return_path`], which accepts only a path on this site, so `return_to`
//! cannot redirect anywhere else.

use forge_http::{
    BoxResponseFuture, HeaderValue, Method, Middleware, Next, Request, Response, StatusCode, header,
};
use tracing::error;

use crate::turbo::{RedirectError, see_other};

/// The query/form parameter that carries the page to return to after
/// signing in.
pub const RETURN_TO_PARAM: &str = "return_to";

/// Longest return path accepted, in bytes.
pub const MAX_RETURN_PATH_BYTES: usize = 2048;

/// `candidate` when it is a path on this site that is safe to redirect to
/// after signing in, `None` otherwise.
///
/// Accepted: at most [`MAX_RETURN_PATH_BYTES`] visible ASCII characters,
/// starting with a single `/` (not `//` or `/\`), without backslashes; a
/// query string is kept. Scheme, host, whitespace, control and non-ASCII
/// characters are refused, exactly like [`see_other`]. Validate every value
/// that came from a request: when the page renders it, when the form posts
/// it and before redirecting.
///
/// ```
/// use forge_views::return_path;
///
/// assert_eq!(return_path("/tenants/1/posts?after=2"), Some("/tenants/1/posts?after=2"));
/// for hostile in ["https://evil.example/", "//evil.example", "/\\evil.example", "posts"] {
///     assert_eq!(return_path(hostile), None);
/// }
/// ```
#[must_use]
pub fn return_path(candidate: &str) -> Option<&str> {
    let bytes = candidate.as_bytes();
    let local = !bytes.is_empty()
        && bytes.len() <= MAX_RETURN_PATH_BYTES
        && bytes[0] == b'/'
        && !matches!(bytes.get(1), Some(b'/' | b'\\'))
        && bytes
            .iter()
            .all(|byte| (0x21..=0x7e).contains(byte) && *byte != b'\\');
    local.then_some(candidate)
}

/// `sign_in_path` with `return_to` as its percent-encoded `return_to` query
/// parameter, or `sign_in_path` alone when `return_to` is `None` or not a
/// valid [`return_path`].
#[must_use]
pub fn sign_in_location(sign_in_path: &str, return_to: Option<&str>) -> String {
    match return_to.and_then(return_path) {
        Some(path) => format!("{sign_in_path}?{RETURN_TO_PARAM}={}", percent_encode(path)),
        None => sign_in_path.to_owned(),
    }
}

/// Encodes everything except RFC 3986 unreserved characters.
fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len() * 3);
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push('%');
            out.push(char::from(b"0123456789ABCDEF"[usize::from(byte >> 4)]));
            out.push(char::from(b"0123456789ABCDEF"[usize::from(byte & 0x0f)]));
        }
    }
    out
}

/// Whether the client asked for an HTML page: its `Accept` header lists
/// `text/html` or `application/xhtml+xml` with a non-zero quality at least
/// as high as `application/json`. Browsers navigating and Turbo send such a
/// header; `fetch` with JSON, API clients and requests without `Accept`
/// (which accept anything) do not prefer HTML.
#[must_use]
pub fn prefers_html(request: &Request) -> bool {
    let mut html = 0_u16;
    let mut json = 0_u16;
    for value in request.headers().get_all(header::ACCEPT) {
        let Ok(value) = value.to_str() else {
            continue;
        };
        for range in value.split(',') {
            let mut parts = range.split(';');
            let media = parts.next().unwrap_or_default().trim().to_ascii_lowercase();
            let quality = parts
                .filter_map(|parameter| {
                    let (name, value) = parameter.split_once('=')?;
                    (name.trim().eq_ignore_ascii_case("q")).then(|| quality(value.trim()))
                })
                .next_back()
                .unwrap_or(Some(1000));
            let Some(quality) = quality else {
                continue;
            };
            match media.as_str() {
                "text/html" | "application/xhtml+xml" => html = html.max(quality),
                "application/json" => json = json.max(quality),
                _ => {}
            }
        }
    }
    html > 0 && html >= json
}

/// An RFC 9110 quality value in thousandths, `None` when malformed.
fn quality(value: &str) -> Option<u16> {
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    if fraction.len() > 3 || !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let thousandths = format!("{fraction:0<3}").parse::<u16>().ok()?;
    match whole {
        "0" => Some(thousandths),
        "1" if thousandths == 0 => Some(1000),
        _ => None,
    }
}

/// Request layer that sends signed-out browsers to the sign-in page (see the
/// [module documentation](self)).
///
/// ```
/// let mut router = forge_http::Router::new();
/// router.layer(forge_views::SignInRedirect::new("/session/new")?);
/// # Ok::<(), forge_views::RedirectError>(())
/// ```
#[derive(Clone, Debug)]
pub struct SignInRedirect {
    sign_in_path: String,
}

impl SignInRedirect {
    /// Redirects to `sign_in_path`, which must be a path on this site
    /// without a query string.
    pub fn new(sign_in_path: impl Into<String>) -> Result<Self, RedirectError> {
        let sign_in_path = sign_in_path.into();
        if return_path(&sign_in_path).is_none() || sign_in_path.contains(['?', '#']) {
            return Err(RedirectError::NotRelative);
        }
        Ok(Self { sign_in_path })
    }

    fn redirect(&self, return_to: Option<&str>, refused: &Response) -> Response {
        let location = sign_in_location(&self.sign_in_path, return_to);
        let mut response = match see_other(&location) {
            Ok(response) => response,
            Err(failure) => {
                error!(error = %failure, "sign-in redirect location is invalid");
                return Response::empty(StatusCode::INTERNAL_SERVER_ERROR);
            }
        };
        // Keep cookies the refusing handler cleared or set.
        for cookie in refused.headers().get_all(header::SET_COOKIE) {
            response
                .headers_mut()
                .append(header::SET_COOKIE, cookie.clone());
        }
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        response
    }
}

impl Middleware for SignInRedirect {
    fn handle<'a>(&'a self, request: Request, next: Next<'a>) -> BoxResponseFuture<'a> {
        Box::pin(async move {
            let html = prefers_html(&request);
            let return_to = matches!(*request.method(), Method::GET | Method::HEAD)
                .then(|| {
                    request
                        .uri()
                        .path_and_query()
                        .map(|path| path.as_str().to_owned())
                })
                .flatten();
            let response = next.run(request).await;
            if response.status() == StatusCode::UNAUTHORIZED && html {
                self.redirect(return_to.as_deref(), &response)
            } else {
                response
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use forge_http::{HeaderMap, Router};
    use uuid::Uuid;

    use super::*;

    fn request(method: Method, uri: &str, accept: Option<&str>) -> Request {
        let mut headers = HeaderMap::new();
        if let Some(accept) = accept {
            headers.insert(
                header::ACCEPT,
                HeaderValue::from_str(accept).expect("header"),
            );
        }
        Request::new_for_test(
            method,
            uri.parse().expect("uri"),
            headers,
            Bytes::new(),
            "127.0.0.1:4000".parse().expect("address"),
            Uuid::now_v7(),
        )
    }

    const BROWSER: &str = "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8";

    #[test]
    fn return_paths_must_stay_on_the_site() {
        for local in ["/", "/tenants/1/posts", "/a?b=c&d=%2F", "/a/./b"] {
            assert_eq!(return_path(local), Some(local), "{local}");
        }
        let too_long = format!("/{}", "a".repeat(MAX_RETURN_PATH_BYTES));
        for hostile in [
            "",
            "posts",
            "//evil.example",
            "/\\evil.example",
            "/a\\b",
            "https://evil.example/",
            "javascript:alert(1)",
            "/a b",
            "/a\tb",
            "/caf\u{e9}",
            too_long.as_str(),
        ] {
            assert_eq!(return_path(hostile), None, "{hostile:?}");
        }
    }

    #[test]
    fn the_sign_in_location_encodes_the_return_path() {
        assert_eq!(
            sign_in_location("/session/new", Some("/tenants/1/posts?after=2&x=y")),
            "/session/new?return_to=%2Ftenants%2F1%2Fposts%3Fafter%3D2%26x%3Dy"
        );
        assert_eq!(
            sign_in_location("/session/new", Some("//evil.example")),
            "/session/new"
        );
        assert_eq!(sign_in_location("/session/new", None), "/session/new");
    }

    #[test]
    fn html_preference_follows_the_accept_header() {
        for (accept, expected) in [
            (Some(BROWSER), true),
            (
                Some("text/vnd.turbo-stream.html, text/html, application/xhtml+xml"),
                true,
            ),
            (Some("application/json"), false),
            (Some("application/json, text/html;q=0.5"), false),
            (Some("text/html;q=0.9, application/json;q=0.9"), true),
            (Some("text/html;q=0"), false),
            (Some("*/*"), false),
            (Some("text/html;q=2"), false),
            (None, false),
        ] {
            assert_eq!(
                prefers_html(&request(Method::GET, "/", accept)),
                expected,
                "{accept:?}"
            );
        }
    }

    #[test]
    fn the_sign_in_path_must_be_local_without_a_query() {
        assert!(SignInRedirect::new("/session/new").is_ok());
        for invalid in ["session/new", "//evil", "/session/new?x=1", "https://a/b"] {
            assert!(SignInRedirect::new(invalid).is_err(), "{invalid}");
        }
    }

    fn router() -> Router {
        let mut router = Router::new();
        router.layer(SignInRedirect::new("/session/new").expect("valid path"));
        for method in [Method::GET, Method::POST] {
            router
                .route(method, "/tenants/{tenant_id}/posts", |_| async {
                    let mut response =
                        Response::new(StatusCode::UNAUTHORIZED, "authentication required");
                    response.headers_mut().append(
                        header::SET_COOKIE,
                        HeaderValue::from_static("__Host-forge_session=; Max-Age=0"),
                    );
                    response
                })
                .expect("route");
        }
        router
            .route(Method::GET, "/forbidden", |_| async {
                Response::new(StatusCode::FORBIDDEN, "forbidden")
            })
            .expect("route");
        router
    }

    #[tokio::test]
    async fn browsers_are_redirected_with_the_page_they_asked_for() {
        let response = router()
            .dispatch(request(
                Method::GET,
                "/tenants/1/posts?after=2",
                Some(BROWSER),
            ))
            .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            response.headers()[header::LOCATION],
            "/session/new?return_to=%2Ftenants%2F1%2Fposts%3Fafter%3D2"
        );
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(
            response.headers()[header::SET_COOKIE],
            "__Host-forge_session=; Max-Age=0"
        );
    }

    #[tokio::test]
    async fn form_submissions_are_redirected_without_a_return_path() {
        let response = router()
            .dispatch(request(Method::POST, "/tenants/1/posts", Some(BROWSER)))
            .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(response.headers()[header::LOCATION], "/session/new");
    }

    #[tokio::test]
    async fn api_clients_keep_the_401_and_other_statuses_pass_through() {
        let router = router();
        for accept in [Some("application/json"), None] {
            let response = router
                .dispatch(request(Method::GET, "/tenants/1/posts", accept))
                .await;
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{accept:?}");
        }
        let forbidden = router
            .dispatch(request(Method::GET, "/forbidden", Some(BROWSER)))
            .await;
        assert_eq!(forbidden.status(), StatusCode::FORBIDDEN);
    }
}
