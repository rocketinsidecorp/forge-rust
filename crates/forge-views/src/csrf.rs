//! CSRF protection for HTML forms and Turbo/`fetch` requests (ADR 0023).
//!
//! [`CsrfProtection`] is a request layer. It uses the synchronizer token
//! pattern with a host-only double-submit cookie, so it works before a user
//! has a server-side session (sign-up and login forms) and in applications
//! without a database:
//!
//! - the token is 256 bits from the operating-system CSPRNG (`getrandom`), in
//!   the canonical [`CsrfToken`] encoding, kept in the `__Host-forge_csrf`
//!   cookie (`Secure`, `HttpOnly`, `SameSite=Lax`, `Path=/`, no `Domain`);
//! - pages never embed the token itself: [`csrf_meta`](crate::csrf_meta) and
//!   [`csrf_field`](crate::csrf_field) write it masked ([`mask_csrf_token`]),
//!   as Rails and Forge-Go do: a fresh 32-byte one-time pad followed by the
//!   token XOR the pad, unpadded base64url (86 characters). Every render
//!   embeds different bytes, so a page compressed together with reflected
//!   input cannot leak the token through its size (BREACH). The cookie keeps
//!   the token itself, and is only set on a response whose handler actually
//!   asked for the token ([`csrf_token`]);
//! - every unsafe request (anything but `GET`, `HEAD`, `OPTIONS`, `TRACE`)
//!   must present exactly one well-formed token, in the `x-csrf-token`
//!   header or in the `csrf_token` field of a form-encoded body, that
//!   carries the cookie's token, masked or as it is ([`unmask_csrf_token`];
//!   constant-time comparison). The query string is never read;
//! - before the token check, a browser-declared cross-site request is
//!   refused: `Sec-Fetch-Site` must be `same-origin` or `none` when present,
//!   otherwise an `Origin` header must name the request's `Host`;
//! - after a successful check, a `POST` form may tunnel `PATCH`, `PUT` or
//!   `DELETE` in `_method` ([`method_override`]); the request method is
//!   rewritten before routing.
//!
//! Every refusal is `403 Forbidden` with a generic body; the reason is
//! logged without token values.

use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicBool, Ordering},
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use cookie::{Cookie, SameSite};
use forge_auth::{CredentialError, CsrfToken};
use forge_http::{
    BoxResponseFuture, CSRF_FIELD_NAME, FormErrorKind, HeaderValue, METHOD_FIELD_NAME, Method,
    Middleware, Next, Request, Response, StatusCode, header,
};
use subtle::ConstantTimeEq as _;
use thiserror::Error;
use tracing::{error, warn};

use crate::{
    common::hex,
    forms::{CSRF_HEADER_NAME, CsrfVerified, method_override},
};

/// Length of a masked token ([`mask_csrf_token`]): the unpadded base64url
/// encoding of the 32-byte one-time pad followed by the token XOR the pad.
pub const MASKED_CSRF_TOKEN_LEN: usize = 86;

/// Bytes of a CSRF token (its canonical form is 64 lowercase hex digits).
const TOKEN_BYTES: usize = 32;

/// Name of the host-only cookie that holds the form CSRF token.
///
/// The `__Host-` prefix makes conforming browsers accept it only with
/// `Secure`, `Path=/` and no `Domain`, so a sibling subdomain or a plain-HTTP
/// network attacker cannot plant a known token.
pub const CSRF_COOKIE_NAME: &str = "__Host-forge_csrf";

/// Why an unsafe request was refused (logged, never sent to the client).
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[non_exhaustive]
pub enum CsrfError {
    /// The browser declared the request cross-site (`Sec-Fetch-Site`), or
    /// its `Origin` does not name the request's `Host`.
    #[error("cross-site request")]
    CrossSite,
    /// No CSRF cookie: the form was not rendered by this application for
    /// this browser, or cookies are blocked.
    #[error("CSRF cookie missing")]
    MissingCookie,
    /// The CSRF cookie is repeated or not a canonical token.
    #[error("CSRF cookie invalid")]
    InvalidCookie,
    /// Neither the header nor the form field carries a token.
    #[error("CSRF token missing")]
    MissingToken,
    /// A header or field is repeated or malformed, or the header and the
    /// field disagree.
    #[error("CSRF token malformed or ambiguous")]
    InvalidToken,
    /// The presented token is not the cookie's token.
    #[error("CSRF token does not match")]
    Mismatch,
    /// The `_method` field is repeated or the form body is malformed.
    #[error("invalid method override")]
    InvalidMethodOverride,
}

/// Request layer enforcing CSRF protection and applying the `_method`
/// override (see the [module documentation](self)).
///
/// Add it to the router once; it protects every route, including routes
/// registered after it:
///
/// ```
/// use forge_http::Router;
///
/// let mut router = Router::new();
/// router.layer(forge_views::CsrfProtection::new());
/// ```
#[derive(Clone, Copy, Debug, Default)]
pub struct CsrfProtection {
    _private: (),
}

impl CsrfProtection {
    /// The layer with the rules described in the module documentation.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

/// Per-request CSRF state shared between the layer and the handler.
#[derive(Clone)]
struct CsrfState {
    inner: Arc<CsrfInner>,
}

struct CsrfInner {
    /// A valid token from the request's cookie.
    existing: Option<CsrfToken>,
    /// A token generated for a browser without a valid cookie.
    issued: OnceLock<CsrfToken>,
    /// Whether the handler read the token (so the page embeds it).
    used: AtomicBool,
}

impl CsrfState {
    fn new(existing: Option<CsrfToken>) -> Self {
        Self {
            inner: Arc::new(CsrfInner {
                existing,
                issued: OnceLock::new(),
                used: AtomicBool::new(false),
            }),
        }
    }

    fn token(&self) -> Option<CsrfToken> {
        self.inner.used.store(true, Ordering::Release);
        if let Some(token) = &self.inner.existing {
            return Some(token.clone());
        }
        if let Some(token) = self.inner.issued.get() {
            return Some(token.clone());
        }
        match generate_token() {
            Ok(token) => Some(self.inner.issued.get_or_init(|| token).clone()),
            Err(failure) => {
                error!(error = %failure, "CSRF token generation failed");
                None
            }
        }
    }
}

/// The CSRF token to render in the current page (`csrf_meta`, `csrf_field`).
///
/// This is the token itself, as the cookie holds it: pages embed it only
/// through [`csrf_meta`](crate::csrf_meta), [`csrf_field`](crate::csrf_field)
/// or [`mask_csrf_token`], which mask it afresh on every render.
///
/// Returns the browser's existing token, or issues a new one that
/// [`CsrfProtection`] stores in the `__Host-forge_csrf` cookie of this
/// response. Reading the token also marks the response `Cache-Control:
/// no-store` (unless the handler set its own `Cache-Control`), because the
/// page now embeds a per-browser secret. `None` when the layer is not
/// installed or the CSPRNG failed; forms rendered without a token are then
/// refused on submission (fail closed).
#[must_use]
pub fn csrf_token(request: &Request) -> Option<CsrfToken> {
    request.extensions().get::<CsrfState>()?.token()
}

/// The proof that [`CsrfProtection`] verified this unsafe request, for code
/// that needs a [`CsrfVerified`] witness. `None` for safe methods and when
/// the layer is not installed.
#[must_use]
pub fn csrf_verified(request: &Request) -> Option<CsrfVerified> {
    request.extensions().get::<CsrfVerified>().copied()
}

/// The token an unsafe request presents, from exactly one `x-csrf-token`
/// header and/or exactly one `csrf_token` field of a form-encoded body,
/// unmasked ([`unmask_csrf_token`]): the canonical token whatever form it
/// was sent in.
///
/// The query string is never read. When both carry a token they must carry
/// the same one. `Ok(None)` when neither is present. Generated applications
/// use this for their session-bound check (ADR 0014) so it reads the token
/// with the same rules as [`CsrfProtection`].
pub fn presented_csrf_token(request: &Request) -> Result<Option<CsrfToken>, CsrfError> {
    let mut headers = request.headers().get_all(CSRF_HEADER_NAME).iter();
    let from_header = match (headers.next(), headers.next()) {
        // The header takes the token masked (the `csrf-token` meta tag) or
        // as it is, for good. Accepting it unmasked is harmless: masking is
        // a public transform anyone holding the token can apply, so it adds
        // no secret, and a client the server gave the token to (such as a
        // generated application's API client, from the session) may send it
        // plain. The cookie stays HttpOnly; scripts cannot read it.
        (None, _) => None,
        (Some(value), None) => Some(parse_token(value.to_str().ok())?),
        (Some(_), Some(_)) => return Err(CsrfError::InvalidToken),
    };
    let from_field = match request.form_value(CSRF_FIELD_NAME) {
        // TODO(v0.2.0): accept only masked tokens in the form field. The
        // unmasked token stays accepted for one release, the one that
        // introduces masking (v0.1.x), so forms rendered before an upgrade
        // keep working while it is deployed (docs/views.md).
        Ok(value) => value
            .map(|value| parse_token(Some(value.as_str())))
            .transpose()?,
        Err(failure) if failure.kind() == FormErrorKind::UnsupportedMediaType => None,
        Err(_) => return Err(CsrfError::InvalidToken),
    };
    match (from_header, from_field) {
        (Some(header), Some(field)) if tokens_equal(&header, &field) => Ok(Some(header)),
        (Some(_), Some(_)) => Err(CsrfError::InvalidToken),
        (Some(token), None) | (None, Some(token)) => Ok(Some(token)),
        (None, None) => Ok(None),
    }
}

/// `Set-Cookie` value storing `token` in the `__Host-forge_csrf` cookie.
///
/// [`CsrfProtection`] sets it itself. At login an application calls it with
/// the new session's freshly issued CSRF token (never the pre-login anonymous
/// token), so that the session's CSRF secret and the cookie agree (ADR 0023).
#[must_use]
pub fn csrf_cookie(token: &CsrfToken) -> String {
    Cookie::build((CSRF_COOKIE_NAME, token.expose().to_owned()))
        .path("/")
        .secure(true)
        .http_only(true)
        .same_site(SameSite::Lax)
        .build()
        .to_string()
}

fn parse_token(value: Option<&str>) -> Result<CsrfToken, CsrfError> {
    value
        .ok_or(CsrfError::InvalidToken)
        .and_then(unmask_csrf_token)
}

/// `token` masked with a fresh one-time pad from the operating-system
/// CSPRNG: the unpadded base64url encoding of the 32-byte pad followed by
/// the token's bytes XOR the pad ([`MASKED_CSRF_TOKEN_LEN`] characters, the
/// format of Rails' `masked_authenticity_token` and Forge-Go's
/// `csrf.MaskToken`). Every call returns a different value for the same
/// token; [`unmask_csrf_token`] turns any of them back into it.
/// [`csrf_meta`](crate::csrf_meta) and [`csrf_field`](crate::csrf_field)
/// call it on every render; a custom renderer that embeds the token in a
/// page calls it too, never [`CsrfToken::expose`].
pub fn mask_csrf_token(token: &CsrfToken) -> Result<String, CredentialError> {
    let raw = token_bytes(token).ok_or(CredentialError::InvalidToken)?;
    let mut masked = [0_u8; 2 * TOKEN_BYTES];
    let (pad, body) = masked.split_at_mut(TOKEN_BYTES);
    getrandom::fill(pad).map_err(|_| CredentialError::RandomnessUnavailable)?;
    for ((out, byte), mask) in body.iter_mut().zip(raw).zip(pad.iter()) {
        *out = byte ^ mask;
    }
    Ok(URL_SAFE_NO_PAD.encode(masked))
}

/// Decodes a submitted token: masked ([`mask_csrf_token`]) or as it is (the
/// canonical 64 lowercase hex digits of [`CsrfToken`]).
///
/// A masked token must be exactly [`MASKED_CSRF_TOKEN_LEN`] characters of
/// unpadded base64url, decode to exactly 64 bytes and be the canonical
/// encoding of them (no padding, no stray trailing bits); anything else is
/// [`CsrfError::InvalidToken`]. The unmasked form stays accepted, which is
/// harmless: masking is a public transform anyone holding the token can
/// apply, so it guards only against the token's bytes repeating in pages
/// (BREACH), not against a client that already has the token. Clients the
/// server gave the token to may send it plain, and forms rendered before the
/// upgrade embed it. Comparing the result with the cookie's token is the
/// caller's job, in constant time ([`CsrfProtection`] does it).
pub fn unmask_csrf_token(value: &str) -> Result<CsrfToken, CsrfError> {
    if value.len() != MASKED_CSRF_TOKEN_LEN {
        return CsrfToken::new(value).map_err(|_| CsrfError::InvalidToken);
    }
    let decoded = URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|_| CsrfError::InvalidToken)?;
    if decoded.len() != 2 * TOKEN_BYTES || URL_SAFE_NO_PAD.encode(&decoded) != value {
        return Err(CsrfError::InvalidToken);
    }
    let (pad, body) = decoded.split_at(TOKEN_BYTES);
    let raw: Vec<u8> = body
        .iter()
        .zip(pad)
        .map(|(byte, mask)| byte ^ mask)
        .collect();
    CsrfToken::new(hex(&raw)).map_err(|_| CsrfError::InvalidToken)
}

/// The 32 bytes of a canonical (lowercase hex) token.
fn token_bytes(token: &CsrfToken) -> Option<[u8; TOKEN_BYTES]> {
    let text = token.expose().as_bytes();
    if text.len() != 2 * TOKEN_BYTES {
        return None;
    }
    let mut bytes = [0_u8; TOKEN_BYTES];
    let (pairs, _) = text.as_chunks::<2>();
    for (byte, &[high, low]) in bytes.iter_mut().zip(pairs) {
        let digit = |character: u8| char::from(character).to_digit(16);
        *byte = u8::try_from(digit(high)? * 16 + digit(low)?).ok()?;
    }
    Some(bytes)
}

/// Constant-time comparison of two canonical (fixed-length) tokens.
fn tokens_equal(left: &CsrfToken, right: &CsrfToken) -> bool {
    left.expose()
        .as_bytes()
        .ct_eq(right.expose().as_bytes())
        .into()
}

/// 256 bits from the operating-system CSPRNG, hex encoded.
fn generate_token() -> Result<CsrfToken, getrandom::Error> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes)?;
    // 64 lowercase hex characters always satisfy the canonical encoding.
    CsrfToken::new(hex(&bytes)).map_err(|_| getrandom::Error::UNEXPECTED)
}

/// The token in the request's `__Host-forge_csrf` cookie.
///
/// Cookie parsing is the `cookie` crate's; malformed pairs are skipped. A
/// repeated CSRF cookie or a non-canonical value is an error.
fn cookie_token(request: &Request) -> Result<Option<CsrfToken>, CsrfError> {
    let mut found = None;
    for value in request.headers().get_all(header::COOKIE) {
        let Ok(value) = value.to_str() else {
            continue;
        };
        for cookie in Cookie::split_parse(value).flatten() {
            if cookie.name() != CSRF_COOKIE_NAME {
                continue;
            }
            if found.is_some() {
                return Err(CsrfError::InvalidCookie);
            }
            found = Some(CsrfToken::new(cookie.value()).map_err(|_| CsrfError::InvalidCookie)?);
        }
    }
    Ok(found)
}

/// Refuses requests the browser declares cross-site.
///
/// Modern browsers send `Sec-Fetch-Site`; only `same-origin` and `none`
/// (user-initiated) pass, so a sibling subdomain (`same-site`) is refused
/// too. Without it, an `Origin` header must be `http(s)://<Host>`; `Origin:
/// null` is refused. Requests with neither header (non-browser clients) fall
/// through to the token check, which they must still pass.
fn check_origin(request: &Request) -> Result<(), CsrfError> {
    let headers = request.headers();
    let mut fetch_site = headers.get_all("sec-fetch-site").iter();
    if let Some(value) = fetch_site.next() {
        if fetch_site.next().is_some() {
            return Err(CsrfError::CrossSite);
        }
        return match value.as_bytes() {
            b"same-origin" | b"none" => Ok(()),
            _ => Err(CsrfError::CrossSite),
        };
    }
    let mut origins = headers.get_all(header::ORIGIN).iter();
    let Some(origin) = origins.next() else {
        return Ok(());
    };
    if origins.next().is_some() {
        return Err(CsrfError::CrossSite);
    }
    let authority = origin
        .to_str()
        .ok()
        .and_then(|origin| {
            origin
                .strip_prefix("https://")
                .or_else(|| origin.strip_prefix("http://"))
        })
        .filter(|authority| !authority.is_empty() && !authority.contains('/'))
        .ok_or(CsrfError::CrossSite)?;
    let mut hosts = headers.get_all(header::HOST).iter();
    match (hosts.next(), hosts.next()) {
        (Some(host), None)
            if host
                .to_str()
                .is_ok_and(|host| host.eq_ignore_ascii_case(authority)) =>
        {
            Ok(())
        }
        _ => Err(CsrfError::CrossSite),
    }
}

/// Checks an unsafe request and returns the verified cookie token.
fn verify(
    request: &Request,
    cookie: Result<Option<CsrfToken>, CsrfError>,
) -> Result<CsrfToken, CsrfError> {
    check_origin(request)?;
    let cookie = cookie?.ok_or(CsrfError::MissingCookie)?;
    let presented = presented_csrf_token(request)?.ok_or(CsrfError::MissingToken)?;
    if tokens_equal(&cookie, &presented) {
        Ok(cookie)
    } else {
        Err(CsrfError::Mismatch)
    }
}

/// Applies `_method` to a verified `POST` form.
fn override_method(request: &mut Request, verified: CsrfVerified) -> Result<(), CsrfError> {
    if *request.method() != Method::POST {
        return Ok(());
    }
    let field = match request.form_value(METHOD_FIELD_NAME) {
        Ok(field) => field,
        Err(failure) if failure.kind() == FormErrorKind::UnsupportedMediaType => None,
        Err(_) => return Err(CsrfError::InvalidMethodOverride),
    };
    let method = method_override(verified, request.method(), field.as_deref());
    request.set_method(method);
    Ok(())
}

fn is_safe(method: &Method) -> bool {
    matches!(
        *method,
        Method::GET | Method::HEAD | Method::OPTIONS | Method::TRACE
    )
}

fn refuse(request: &Request, reason: CsrfError) -> Response {
    warn!(
        request_id = %request.request_id(),
        method = %request.method(),
        path = request.uri().path(),
        reason = %reason,
        "request refused by CSRF protection"
    );
    let mut response = Response::new(StatusCode::FORBIDDEN, "forbidden\n");
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    response
}

/// Adds the cookie of a newly issued token and keeps token-bearing pages out
/// of shared caches.
fn finish(state: &CsrfState, response: &mut Response) {
    if let Some(token) = state.inner.issued.get() {
        match HeaderValue::from_str(&csrf_cookie(token)) {
            Ok(value) => {
                response.headers_mut().append(header::SET_COOKIE, value);
            }
            Err(failure) => error!(error = %failure, "CSRF cookie is not a valid header value"),
        }
    }
    if state.inner.used.load(Ordering::Acquire)
        && !response.headers().contains_key(header::CACHE_CONTROL)
    {
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
}

impl Middleware for CsrfProtection {
    fn handle<'a>(&'a self, mut request: Request, next: Next<'a>) -> BoxResponseFuture<'a> {
        Box::pin(async move {
            let cookie = cookie_token(&request);
            let state = if is_safe(request.method()) {
                // A missing or invalid cookie is replaced if the page needs a
                // token; safe requests are never refused.
                CsrfState::new(cookie.ok().flatten())
            } else {
                let token = match verify(&request, cookie) {
                    Ok(token) => token,
                    Err(reason) => return refuse(&request, reason),
                };
                let verified = CsrfVerified::after_check();
                if let Err(reason) = override_method(&mut request, verified) {
                    return refuse(&request, reason);
                }
                request.extensions_mut().insert(verified);
                CsrfState::new(Some(token))
            };
            request.extensions_mut().insert(state.clone());
            let mut response = next.run(request).await;
            finish(&state, &mut response);
            response
        })
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use forge_http::{HeaderMap, Router, header::HeaderName};
    use uuid::Uuid;

    use super::*;
    use crate::test_support::{header as header_of, text};

    const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const OTHER: &str = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";
    const FORM: &str = "application/x-www-form-urlencoded";

    fn request(method: Method, uri: &str, headers: &[(&str, &str)], body: &str) -> Request {
        let mut map = HeaderMap::new();
        for (name, value) in headers {
            map.append(
                HeaderName::from_bytes(name.as_bytes()).expect("valid header name"),
                HeaderValue::from_str(value).expect("valid header value"),
            );
        }
        Request::new_for_test(
            method,
            uri.parse().expect("valid test URI"),
            map,
            Bytes::from(body.to_owned()),
            "127.0.0.1:4000".parse().expect("valid test address"),
            Uuid::now_v7(),
        )
    }

    fn cookie(token: &str) -> String {
        format!("theme=dark; {CSRF_COOKIE_NAME}={token}")
    }

    /// A protected router whose handlers echo the routed method and render
    /// the token on `GET /form`.
    fn router() -> Router {
        let mut router = Router::new();
        router.layer(CsrfProtection::new());
        router
            .route(Method::GET, "/form", |request: Request| async move {
                let token = csrf_token(&request).map(|token| token.expose().to_owned());
                Response::new(StatusCode::OK, token.unwrap_or_default())
            })
            .expect("route");
        router
            .route(Method::GET, "/plain", |_| async {
                Response::new(StatusCode::OK, "plain")
            })
            .expect("route");
        for method in [Method::POST, Method::PUT, Method::PATCH, Method::DELETE] {
            router
                .route(method, "/items", |request: Request| async move {
                    let verified = csrf_verified(&request).is_some();
                    Response::new(StatusCode::OK, format!("{} {verified}", request.method()))
                })
                .expect("route");
        }
        router
    }

    async fn send(request: Request) -> Response {
        router().dispatch(request).await
    }

    fn set_cookies(response: &Response) -> Vec<String> {
        response
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .map(|value| value.to_str().expect("ASCII").to_owned())
            .collect()
    }

    #[tokio::test]
    async fn first_page_issues_a_host_only_cookie_matching_the_rendered_token() {
        let response = send(request(Method::GET, "/form", &[], "")).await;

        assert_eq!(response.status(), StatusCode::OK);
        let token = text(&response);
        assert_eq!(token.len(), 64);
        let cookies = set_cookies(&response);
        assert_eq!(cookies.len(), 1);
        let set = &cookies[0];
        assert!(
            set.starts_with(&format!("{CSRF_COOKIE_NAME}={token}")),
            "{set}"
        );
        for attribute in ["Secure", "HttpOnly", "SameSite=Lax", "Path=/"] {
            assert!(set.contains(attribute), "{set}");
        }
        assert!(!set.contains("Domain"), "{set}");
        assert_eq!(header_of(&response, "cache-control"), Some("no-store"));
    }

    #[tokio::test]
    async fn existing_cookie_is_reused_and_unused_tokens_set_nothing() {
        let page = send(request(
            Method::GET,
            "/form",
            &[("cookie", &cookie(TOKEN))],
            "",
        ))
        .await;
        assert_eq!(text(&page), TOKEN);
        assert!(set_cookies(&page).is_empty());

        let plain = send(request(Method::GET, "/plain", &[], "")).await;
        assert!(set_cookies(&plain).is_empty());
        assert_eq!(header_of(&plain, "cache-control"), None);

        // A malformed cookie is replaced on the next page that needs a token.
        let replaced = send(request(
            Method::GET,
            "/form",
            &[("cookie", &cookie("short"))],
            "",
        ))
        .await;
        assert_ne!(text(&replaced), "short");
        assert_eq!(set_cookies(&replaced).len(), 1);
    }

    #[tokio::test]
    async fn form_field_or_header_with_the_cookie_token_passes() {
        let jar = cookie(TOKEN);
        let field = send(request(
            Method::POST,
            "/items",
            &[("cookie", &jar), ("content-type", FORM)],
            &format!("name=mug&{CSRF_FIELD_NAME}={TOKEN}"),
        ))
        .await;
        assert_eq!(field.status(), StatusCode::OK);
        assert_eq!(text(&field), "POST true");

        let header = send(request(
            Method::PATCH,
            "/items",
            &[
                ("cookie", &jar),
                (CSRF_HEADER_NAME, TOKEN),
                ("content-type", "application/json"),
            ],
            "{}",
        ))
        .await;
        assert_eq!(text(&header), "PATCH true");

        let both = send(request(
            Method::POST,
            "/items",
            &[
                ("cookie", &jar),
                (CSRF_HEADER_NAME, TOKEN),
                ("content-type", FORM),
            ],
            &format!("{CSRF_FIELD_NAME}={TOKEN}"),
        ))
        .await;
        assert_eq!(both.status(), StatusCode::OK);
    }

    async fn refused(request: Request) {
        let label = format!("{} {:?}", request.method(), request.headers());
        let response = send(request).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{label}");
        assert_eq!(text(&response), "forbidden\n");
        assert!(!text(&response).contains(TOKEN));
    }

    #[tokio::test]
    async fn missing_wrong_or_ambiguous_tokens_are_forbidden() {
        let jar = cookie(TOKEN);
        let form_body = |token: &str| format!("name=mug&{CSRF_FIELD_NAME}={token}");
        // No cookie at all, even with a well-formed token.
        refused(request(
            Method::POST,
            "/items",
            &[("content-type", FORM)],
            &form_body(TOKEN),
        ))
        .await;
        // No token.
        refused(request(
            Method::POST,
            "/items",
            &[("cookie", &jar), ("content-type", FORM)],
            "name=mug",
        ))
        .await;
        refused(request(Method::DELETE, "/items", &[("cookie", &jar)], "")).await;
        // Wrong token, in the field or in the header.
        refused(request(
            Method::POST,
            "/items",
            &[("cookie", &jar), ("content-type", FORM)],
            &form_body(OTHER),
        ))
        .await;
        refused(request(
            Method::PUT,
            "/items",
            &[("cookie", &jar), (CSRF_HEADER_NAME, OTHER)],
            "",
        ))
        .await;
        // Malformed tokens and cookies.
        refused(request(
            Method::POST,
            "/items",
            &[("cookie", &jar), ("content-type", FORM)],
            &form_body("abc"),
        ))
        .await;
        refused(request(
            Method::POST,
            "/items",
            &[("cookie", &jar), (CSRF_HEADER_NAME, &TOKEN.to_uppercase())],
            "",
        ))
        .await;
        refused(request(
            Method::POST,
            "/items",
            &[("cookie", &cookie("abc")), (CSRF_HEADER_NAME, "abc")],
            "",
        ))
        .await;
        // Repeated header, field or cookie.
        refused(request(
            Method::POST,
            "/items",
            &[
                ("cookie", &jar),
                (CSRF_HEADER_NAME, TOKEN),
                (CSRF_HEADER_NAME, TOKEN),
            ],
            "",
        ))
        .await;
        refused(request(
            Method::POST,
            "/items",
            &[("cookie", &jar), ("content-type", FORM)],
            &format!("{CSRF_FIELD_NAME}={TOKEN}&{CSRF_FIELD_NAME}={TOKEN}"),
        ))
        .await;
        refused(request(
            Method::POST,
            "/items",
            &[
                ("cookie", &format!("{jar}; {CSRF_COOKIE_NAME}={TOKEN}")),
                (CSRF_HEADER_NAME, TOKEN),
            ],
            "",
        ))
        .await;
        // Header and field disagree.
        refused(request(
            Method::POST,
            "/items",
            &[
                ("cookie", &jar),
                (CSRF_HEADER_NAME, TOKEN),
                ("content-type", FORM),
            ],
            &form_body(OTHER),
        ))
        .await;
        // A malformed form body cannot hide or supply a token.
        refused(request(
            Method::POST,
            "/items",
            &[("cookie", &jar), ("content-type", FORM)],
            &format!("{CSRF_FIELD_NAME}={TOKEN}&&"),
        ))
        .await;
    }

    #[tokio::test]
    async fn token_in_the_query_string_or_a_non_form_body_is_ignored() {
        let jar = cookie(TOKEN);
        refused(request(
            Method::POST,
            &format!("/items?{CSRF_FIELD_NAME}={TOKEN}"),
            &[("cookie", &jar), ("content-type", FORM)],
            "name=mug",
        ))
        .await;
        refused(request(
            Method::POST,
            "/items",
            &[("cookie", &jar), ("content-type", "text/plain")],
            &format!("{CSRF_FIELD_NAME}={TOKEN}"),
        ))
        .await;
    }

    #[tokio::test]
    async fn cross_site_requests_are_refused_even_with_a_valid_token() {
        let jar = cookie(TOKEN);
        for (name, value) in [
            ("sec-fetch-site", "cross-site"),
            ("sec-fetch-site", "same-site"),
            ("origin", "https://evil.example"),
            ("origin", "null"),
            ("origin", "https://app.example.com.evil.example"),
        ] {
            refused(request(
                Method::POST,
                "/items",
                &[
                    ("cookie", &jar),
                    (CSRF_HEADER_NAME, TOKEN),
                    ("host", "app.example.com"),
                    (name, value),
                ],
                "",
            ))
            .await;
        }
        for (name, value) in [
            ("sec-fetch-site", "same-origin"),
            ("sec-fetch-site", "none"),
            ("origin", "https://app.example.com"),
            ("origin", "http://APP.example.com"),
        ] {
            let response = send(request(
                Method::POST,
                "/items",
                &[
                    ("cookie", &jar),
                    (CSRF_HEADER_NAME, TOKEN),
                    ("host", "app.example.com"),
                    (name, value),
                ],
                "",
            ))
            .await;
            assert_eq!(response.status(), StatusCode::OK, "{name}: {value}");
        }
        // Sec-Fetch-Site takes precedence over a matching Origin.
        refused(request(
            Method::POST,
            "/items",
            &[
                ("cookie", &jar),
                (CSRF_HEADER_NAME, TOKEN),
                ("host", "app.example.com"),
                ("origin", "https://app.example.com"),
                ("sec-fetch-site", "cross-site"),
            ],
            "",
        ))
        .await;
    }

    #[tokio::test]
    async fn method_override_applies_only_to_verified_post_forms() {
        let jar = cookie(TOKEN);
        let tunnel =
            |method: &str, token: &str| format!("_method={method}&{CSRF_FIELD_NAME}={token}");

        let deleted = send(request(
            Method::POST,
            "/items",
            &[("cookie", &jar), ("content-type", FORM)],
            &tunnel("delete", TOKEN),
        ))
        .await;
        assert_eq!(text(&deleted), "DELETE true");
        let put = send(request(
            Method::POST,
            "/items",
            &[("cookie", &jar), ("content-type", FORM)],
            &tunnel("PUT", TOKEN),
        ))
        .await;
        assert_eq!(text(&put), "PUT true");

        // Without a valid token the override never happens: 403, not DELETE.
        refused(request(
            Method::POST,
            "/items",
            &[("cookie", &jar), ("content-type", FORM)],
            "_method=DELETE",
        ))
        .await;
        refused(request(
            Method::POST,
            "/items",
            &[("cookie", &jar), ("content-type", FORM)],
            &tunnel("DELETE", OTHER),
        ))
        .await;

        // GET is never overridden, from the query or a body.
        let get = send(request(
            Method::GET,
            "/plain?_method=DELETE",
            &[("content-type", FORM)],
            "_method=DELETE",
        ))
        .await;
        assert_eq!(text(&get), "plain");

        // Only PATCH, PUT and DELETE; anything else stays POST.
        for value in ["GET", "TRACE", "CONNECT", "post", "x"] {
            let response = send(request(
                Method::POST,
                "/items",
                &[("cookie", &jar), ("content-type", FORM)],
                &tunnel(value, TOKEN),
            ))
            .await;
            assert_eq!(text(&response), "POST true", "{value}");
        }

        // An override on a method other than POST is ignored.
        let patch = send(request(
            Method::PATCH,
            "/items",
            &[("cookie", &jar), ("content-type", FORM)],
            &tunnel("DELETE", TOKEN),
        ))
        .await;
        assert_eq!(text(&patch), "PATCH true");

        // A repeated `_method` is refused rather than guessed.
        refused(request(
            Method::POST,
            "/items",
            &[("cookie", &jar), ("content-type", FORM)],
            &format!("_method=PUT&_method=DELETE&{CSRF_FIELD_NAME}={TOKEN}"),
        ))
        .await;
    }

    #[tokio::test]
    async fn unrouted_unsafe_requests_are_checked_before_routing() {
        let missing = send(request(Method::POST, "/missing", &[], "")).await;
        assert_eq!(missing.status(), StatusCode::FORBIDDEN);
        let passed = send(request(
            Method::POST,
            "/missing",
            &[("cookie", &cookie(TOKEN)), (CSRF_HEADER_NAME, TOKEN)],
            "",
        ))
        .await;
        assert_eq!(passed.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn presented_token_reads_header_and_field_with_the_same_rules() {
        let header_only = request(Method::POST, "/", &[(CSRF_HEADER_NAME, TOKEN)], "");
        assert_eq!(
            presented_csrf_token(&header_only)
                .expect("valid")
                .map(|token| token.expose().to_owned()),
            Some(TOKEN.to_owned())
        );
        let none = request(Method::POST, "/", &[], "");
        assert_eq!(presented_csrf_token(&none), Ok(None));
        let mismatch = request(
            Method::POST,
            "/",
            &[(CSRF_HEADER_NAME, TOKEN), ("content-type", FORM)],
            &format!("{CSRF_FIELD_NAME}={OTHER}"),
        );
        assert_eq!(
            presented_csrf_token(&mismatch),
            Err(CsrfError::InvalidToken)
        );
    }

    fn masked(token: &str) -> String {
        mask_csrf_token(&CsrfToken::new(token).expect("valid token")).expect("CSPRNG")
    }

    fn unmasked(value: &str) -> Result<String, CsrfError> {
        unmask_csrf_token(value).map(|token| token.expose().to_owned())
    }

    #[test]
    fn masks_are_fresh_86_characters_of_base64url_and_unmask_to_the_token() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..64 {
            let value = masked(TOKEN);
            assert_eq!(value.len(), MASKED_CSRF_TOKEN_LEN);
            assert!(
                value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'),
                "{value}"
            );
            assert_eq!(unmasked(&value), Ok(TOKEN.to_owned()));
            assert!(seen.insert(value), "a pad was reused");
        }
        // The canonical token is accepted as it is.
        assert_eq!(unmasked(TOKEN), Ok(TOKEN.to_owned()));
        // Rails' and Forge-Go's layout: pad, then token XOR pad.
        let decoded = URL_SAFE_NO_PAD.decode(masked(TOKEN)).expect("base64url");
        let (pad, body) = decoded.split_at(32);
        let raw: Vec<u8> = body
            .iter()
            .zip(pad)
            .map(|(byte, mask)| byte ^ mask)
            .collect();
        assert_eq!(hex(&raw), TOKEN);
    }

    #[test]
    fn tampered_truncated_padded_and_foreign_tokens_are_refused() {
        let value = masked(TOKEN);
        let mut refused = vec![
            String::new(),
            value[..MASKED_CSRF_TOKEN_LEN - 1].to_owned(),
            format!("{value}A"),
            format!("{value}=="),
            format!("{}==", &value[..MASKED_CSRF_TOKEN_LEN - 2]),
            format!(" {}", &value[1..]),
            // The standard alphabet's `+` and `/` are not base64url.
            format!("+{}", &value[1..]),
            format!("{}/{}", &value[..10], &value[11..]),
            TOKEN.to_uppercase(),
            TOKEN[..63].to_owned(),
            format!("{TOKEN}0"),
            "z".repeat(64),
        ];
        // The 86th character carries 4 bits that must be zero.
        let mut stray = value.clone();
        stray.pop();
        stray.push('B');
        refused.push(stray);
        for candidate in &refused {
            assert_eq!(
                unmasked(candidate),
                Err(CsrfError::InvalidToken),
                "{candidate:?}"
            );
        }
        // Any changed character yields another token or an invalid one,
        // never the cookie's token.
        for index in 0..MASKED_CSRF_TOKEN_LEN {
            let mut tampered: Vec<u8> = value.clone().into_bytes();
            tampered[index] = if tampered[index] == b'A' { b'B' } else { b'A' };
            let tampered = String::from_utf8(tampered).expect("ASCII");
            assert_ne!(unmasked(&tampered), Ok(TOKEN.to_owned()), "{index}");
        }
        // A masked form of another token is that other token.
        assert_eq!(unmasked(&masked(OTHER)), Ok(OTHER.to_owned()));
    }

    /// Arbitrary input never panics and only well-formed tokens decode:
    /// random strings of every length around the two accepted ones, from an
    /// alphabet with base64, hex, padding, separators and multi-byte
    /// characters.
    #[test]
    fn arbitrary_input_never_panics() {
        const ALPHABET: &[char] = &[
            'A',
            'Z',
            'a',
            'f',
            'z',
            '0',
            '9',
            '-',
            '_',
            '+',
            '/',
            '=',
            ' ',
            '%',
            '\0',
            'é',
            '€',
            '\u{10348}',
        ];
        let mut random = [0_u8; 4096];
        getrandom::fill(&mut random).expect("CSPRNG");
        let mut bytes = random.iter().copied().cycle();
        for length in (0..=100).chain([64, 86, 86, 86, 64]) {
            for _ in 0..16 {
                let candidate: String = (0..length)
                    .map(|_| ALPHABET[usize::from(bytes.next().unwrap_or(0)) % ALPHABET.len()])
                    .collect();
                if let Ok(token) = unmask_csrf_token(&candidate) {
                    assert_eq!(token.expose().len(), 64, "{candidate:?}");
                }
            }
        }
        // Every 86-byte string of random base64url decodes or is refused,
        // without panicking; those that decode are canonical.
        for chunk in random.as_chunks::<MASKED_CSRF_TOKEN_LEN>().0 {
            let candidate: String = chunk
                .iter()
                .map(|byte| {
                    char::from(
                        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_"
                            [usize::from(*byte) % 64],
                    )
                })
                .collect();
            if unmask_csrf_token(&candidate).is_ok() {
                let decoded = URL_SAFE_NO_PAD.decode(&candidate).expect("decodes");
                assert_eq!(URL_SAFE_NO_PAD.encode(decoded), candidate);
            }
        }
    }

    #[tokio::test]
    async fn masked_and_plain_tokens_pass_in_the_field_and_the_header() {
        let jar = cookie(TOKEN);
        for token in [masked(TOKEN), TOKEN.to_owned()] {
            let field = send(request(
                Method::POST,
                "/items",
                &[("cookie", &jar), ("content-type", FORM)],
                &format!("name=mug&{CSRF_FIELD_NAME}={token}"),
            ))
            .await;
            assert_eq!(text(&field), "POST true", "{token}");
            let header = send(request(
                Method::DELETE,
                "/items",
                &[("cookie", &jar), (CSRF_HEADER_NAME, &token)],
                "",
            ))
            .await;
            assert_eq!(text(&header), "DELETE true", "{token}");
        }
        // Header and field may carry the token in different forms or masks.
        let both = send(request(
            Method::POST,
            "/items",
            &[
                ("cookie", &jar),
                (CSRF_HEADER_NAME, &masked(TOKEN)),
                ("content-type", FORM),
            ],
            &format!("{CSRF_FIELD_NAME}={}", masked(TOKEN)),
        ))
        .await;
        assert_eq!(both.status(), StatusCode::OK);
        // A masked token of another browser, or a header and a field that
        // carry different tokens, are refused.
        refused(request(
            Method::POST,
            "/items",
            &[("cookie", &jar), ("content-type", FORM)],
            &format!("{CSRF_FIELD_NAME}={}", masked(OTHER)),
        ))
        .await;
        refused(request(
            Method::PUT,
            "/items",
            &[("cookie", &jar), (CSRF_HEADER_NAME, &masked(OTHER))],
            "",
        ))
        .await;
        refused(request(
            Method::POST,
            "/items",
            &[
                ("cookie", &jar),
                (CSRF_HEADER_NAME, &masked(TOKEN)),
                ("content-type", FORM),
            ],
            &format!("{CSRF_FIELD_NAME}={}", masked(OTHER)),
        ))
        .await;
        // A masked value never works as the cookie.
        refused(request(
            Method::POST,
            "/items",
            &[
                ("cookie", &cookie(&masked(TOKEN))),
                (CSRF_HEADER_NAME, TOKEN),
            ],
            "",
        ))
        .await;
    }

    #[test]
    fn presented_tokens_are_unmasked() {
        let header = request(Method::POST, "/", &[(CSRF_HEADER_NAME, &masked(TOKEN))], "");
        assert_eq!(
            presented_csrf_token(&header)
                .expect("valid")
                .map(|token| token.expose().to_owned()),
            Some(TOKEN.to_owned())
        );
        let field = request(
            Method::POST,
            "/",
            &[("content-type", FORM)],
            &format!("{CSRF_FIELD_NAME}={}", masked(TOKEN)),
        );
        assert_eq!(
            presented_csrf_token(&field)
                .expect("valid")
                .map(|token| token.expose().to_owned()),
            Some(TOKEN.to_owned())
        );
    }

    #[test]
    fn generated_tokens_are_canonical_and_distinct() {
        let first = generate_token().expect("CSPRNG");
        let second = generate_token().expect("CSPRNG");
        assert_eq!(first.expose().len(), 64);
        assert_ne!(first, second);
        assert!(tokens_equal(&first, &first.clone()));
        assert!(!tokens_equal(&first, &second));
    }
}
