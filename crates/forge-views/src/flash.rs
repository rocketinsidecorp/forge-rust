//! One-shot flash messages (`notice` and `alert`), as in Rails (ADR 0023).
//!
//! A handler attaches a [`Flash`] to the response that ends an action,
//! typically the `303` redirect after a form submission. The message travels
//! in the host-only `__Host-forge_flash` cookie (`Secure`, `HttpOnly`,
//! `SameSite=Lax`, `Path=/`) and [`FlashLayer`] hands it to the next request.
//! The first handler that reads it with [`flash`] consumes it: that
//! response removes the cookie, so the message is shown once. Requests that
//! do not read it (assets, health probes) leave it in place.
//!
//! The cookie carries only display text that the server set, bounded in
//! size. It is rendered through auto-escaping templates like any other value,
//! so a tampered cookie cannot inject markup; the `__Host-` prefix keeps
//! other origins and plain-HTTP attackers from setting it. Flash messages
//! must not contain secrets: the user's own browser stores them.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use cookie::{Cookie, SameSite};
use forge_http::{BoxResponseFuture, HeaderValue, Middleware, Next, Request, Response, header};
use serde_json::{Map, Value};
use thiserror::Error;
use tracing::error;

/// Name of the host-only cookie that carries the flash to the next request.
pub const FLASH_COOKIE_NAME: &str = "__Host-forge_flash";

/// Maximum size of one flash message, in bytes of UTF-8.
pub const MAX_FLASH_MESSAGE_BYTES: usize = 512;

/// Maximum size of the encoded `Set-Cookie` value. Browsers guarantee about
/// 4 KiB per cookie, including its attributes.
const MAX_FLASH_COOKIE_BYTES: usize = 3072;

const NOTICE: &str = "notice";
const ALERT: &str = "alert";

/// A flash message could not be attached to a response.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[non_exhaustive]
pub enum FlashError {
    /// A message is longer than [`MAX_FLASH_MESSAGE_BYTES`].
    #[error("flash message is longer than {MAX_FLASH_MESSAGE_BYTES} bytes")]
    MessageTooLong,
    /// The encoded cookie would exceed the browser cookie budget.
    #[error("flash messages do not fit in a cookie")]
    TooLarge,
}

/// One-shot `notice` (success) and `alert` (failure) messages.
///
/// ```
/// use forge_http::{Response, StatusCode};
/// use forge_views::Flash;
///
/// let mut response = Response::empty(StatusCode::SEE_OTHER);
/// Flash::new().with_notice("Product created.").set_on(&mut response)?;
/// assert!(response.headers().contains_key("set-cookie"));
/// # Ok::<(), forge_views::FlashError>(())
/// ```
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Flash {
    notice: Option<String>,
    alert: Option<String>,
}

impl Flash {
    /// An empty flash.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the success message.
    #[must_use]
    pub fn with_notice(mut self, message: impl Into<String>) -> Self {
        self.notice = Some(message.into());
        self
    }

    /// Sets the failure message.
    #[must_use]
    pub fn with_alert(mut self, message: impl Into<String>) -> Self {
        self.alert = Some(message.into());
        self
    }

    /// The success message, if any.
    #[must_use]
    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    /// The failure message, if any.
    #[must_use]
    pub fn alert(&self) -> Option<&str> {
        self.alert.as_deref()
    }

    /// Whether there is no message.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.notice.is_none() && self.alert.is_none()
    }

    /// Stores the messages in the flash cookie of `response`, for the next
    /// request. An empty flash adds nothing.
    pub fn set_on(&self, response: &mut Response) -> Result<(), FlashError> {
        if self.is_empty() {
            return Ok(());
        }
        let mut document = Map::new();
        for (key, message) in [(NOTICE, &self.notice), (ALERT, &self.alert)] {
            if let Some(message) = message {
                if message.len() > MAX_FLASH_MESSAGE_BYTES {
                    return Err(FlashError::MessageTooLong);
                }
                document.insert(key.to_owned(), Value::String(message.clone()));
            }
        }
        let json = Value::Object(document).to_string();
        let encoded = flash_cookie(Cookie::build((FLASH_COOKIE_NAME, json)))
            .encoded()
            .to_string();
        if encoded.len() > MAX_FLASH_COOKIE_BYTES {
            return Err(FlashError::TooLarge);
        }
        let value = HeaderValue::from_str(&encoded).map_err(|_| FlashError::TooLarge)?;
        response.headers_mut().append(header::SET_COOKIE, value);
        Ok(())
    }

    /// Parses the JSON stored by [`Flash::set_on`]; `None` when it is not
    /// exactly that shape.
    fn from_json(text: &str) -> Option<Self> {
        let Value::Object(document) = serde_json::from_str::<Value>(text).ok()? else {
            return None;
        };
        let mut flash = Self::new();
        for (key, value) in document {
            let Value::String(message) = value else {
                return None;
            };
            if message.len() > MAX_FLASH_MESSAGE_BYTES {
                return None;
            }
            match key.as_str() {
                NOTICE => flash.notice = Some(message),
                ALERT => flash.alert = Some(message),
                _ => return None,
            }
        }
        Some(flash)
    }
}

fn flash_cookie<'c>(builder: cookie::CookieBuilder<'c>) -> Cookie<'c> {
    builder
        .path("/")
        .secure(true)
        .http_only(true)
        .same_site(SameSite::Lax)
        .build()
}

/// The flash set by the previous response, consumed by this call: the
/// current response removes the cookie (unless it sets a new flash). Empty
/// when there is none or [`FlashLayer`] is not installed.
#[must_use]
pub fn flash(request: &Request) -> Flash {
    match request.extensions().get::<FlashState>() {
        Some(state) => {
            state.inner.consumed.store(true, Ordering::Release);
            state.inner.flash.clone()
        }
        None => Flash::new(),
    }
}

#[derive(Clone)]
struct FlashState {
    inner: Arc<FlashInner>,
}

struct FlashInner {
    flash: Flash,
    consumed: AtomicBool,
}

/// What the request's flash cookie holds.
enum Incoming {
    Absent,
    Valid(Flash),
    /// Repeated, undecodable or out of shape: dropped and removed.
    Invalid,
}

fn incoming(request: &Request) -> Incoming {
    let mut found = Incoming::Absent;
    for value in request.headers().get_all(header::COOKIE) {
        let Ok(value) = value.to_str() else {
            continue;
        };
        for cookie in Cookie::split_parse_encoded(value).flatten() {
            if cookie.name() != FLASH_COOKIE_NAME {
                continue;
            }
            if !matches!(found, Incoming::Absent) {
                return Incoming::Invalid;
            }
            found = Flash::from_json(cookie.value()).map_or(Incoming::Invalid, Incoming::Valid);
        }
    }
    found
}

/// Whether the handler already set a new flash on `response`.
fn sets_flash(response: &Response) -> bool {
    let prefix = format!("{FLASH_COOKIE_NAME}=");
    response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .any(|value| value.as_bytes().starts_with(prefix.as_bytes()))
}

fn removal_cookie() -> String {
    let mut cookie = flash_cookie(Cookie::build(FLASH_COOKIE_NAME));
    cookie.make_removal();
    cookie.to_string()
}

/// Request layer that delivers the flash cookie to [`flash`] and removes it
/// once read.
///
/// ```
/// let mut router = forge_http::Router::new();
/// router.layer(forge_views::FlashLayer::new());
/// ```
#[derive(Clone, Copy, Debug, Default)]
pub struct FlashLayer {
    _private: (),
}

impl FlashLayer {
    /// The layer.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl Middleware for FlashLayer {
    fn handle<'a>(&'a self, mut request: Request, next: Next<'a>) -> BoxResponseFuture<'a> {
        Box::pin(async move {
            let (flash, present, consumed) = match incoming(&request) {
                Incoming::Absent => (Flash::new(), false, false),
                Incoming::Valid(flash) => (flash, true, false),
                Incoming::Invalid => (Flash::new(), true, true),
            };
            let state = FlashState {
                inner: Arc::new(FlashInner {
                    flash,
                    consumed: AtomicBool::new(consumed),
                }),
            };
            request.extensions_mut().insert(state.clone());
            let mut response = next.run(request).await;
            if present && state.inner.consumed.load(Ordering::Acquire) && !sets_flash(&response) {
                match HeaderValue::from_str(&removal_cookie()) {
                    Ok(value) => {
                        response.headers_mut().append(header::SET_COOKIE, value);
                    }
                    Err(failure) => {
                        error!(error = %failure, "flash removal cookie is not a valid header value");
                    }
                }
            }
            response
        })
    }
}

#[cfg(test)]
mod tests {
    use forge_http::{Method, Router, StatusCode};

    use super::*;
    use crate::test_support::{request_with, text};

    fn router() -> Router {
        let mut router = Router::new();
        router.layer(FlashLayer::new());
        router
            .route(Method::POST, "/items", |_| async {
                let mut response = Response::empty(StatusCode::SEE_OTHER);
                match Flash::new()
                    .with_notice("Item <b>created</b> & \"saved\"")
                    .set_on(&mut response)
                {
                    Ok(()) => response,
                    Err(_) => Response::empty(StatusCode::INTERNAL_SERVER_ERROR),
                }
            })
            .expect("route");
        router
            .route(Method::GET, "/items", |request: Request| async move {
                let flash = flash(&request);
                Response::new(
                    StatusCode::OK,
                    format!(
                        "{}|{}",
                        flash.notice().unwrap_or("-"),
                        flash.alert().unwrap_or("-")
                    ),
                )
            })
            .expect("route");
        router
            .route(Method::GET, "/asset", |_| async {
                Response::new(StatusCode::OK, "asset")
            })
            .expect("route");
        router
    }

    fn set_cookie(response: &Response) -> Option<String> {
        response
            .headers()
            .get(header::SET_COOKIE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    }

    /// The `name=value` pair of a `Set-Cookie` value, as a browser sends it.
    fn pair(set_cookie: &str) -> String {
        set_cookie.split(';').next().unwrap_or_default().to_owned()
    }

    #[tokio::test]
    async fn flash_survives_one_redirect_and_is_shown_once() {
        let router = router();
        let created = router
            .dispatch(request_with(Method::POST, "/items", &[]))
            .await;
        let stored = set_cookie(&created).expect("flash cookie");
        assert!(
            stored.starts_with(&format!("{FLASH_COOKIE_NAME}=")),
            "{stored}"
        );
        for attribute in ["Secure", "HttpOnly", "SameSite=Lax", "Path=/"] {
            assert!(stored.contains(attribute), "{stored}");
        }
        // Percent-encoded: no raw quotes, separators or markup in the header.
        assert!(!pair(&stored).contains(['"', '<', ' ', ';']), "{stored}");

        let jar = pair(&stored);
        let asset = router
            .dispatch(request_with(Method::GET, "/asset", &[("cookie", &jar)]))
            .await;
        assert_eq!(set_cookie(&asset), None, "an unread flash is kept");

        let shown = router
            .dispatch(request_with(Method::GET, "/items", &[("cookie", &jar)]))
            .await;
        assert_eq!(text(&shown), "Item <b>created</b> & \"saved\"|-");
        let removal = set_cookie(&shown).expect("removal cookie");
        assert!(
            removal.starts_with(&format!("{FLASH_COOKIE_NAME}=;")),
            "{removal}"
        );
        assert!(removal.contains("Max-Age=0"), "{removal}");

        let later = router
            .dispatch(request_with(Method::GET, "/items", &[]))
            .await;
        assert_eq!(text(&later), "-|-");
        assert_eq!(set_cookie(&later), None);
    }

    #[tokio::test]
    async fn tampered_or_repeated_cookies_are_dropped_and_removed() {
        let router = router();
        for jar in [
            format!("{FLASH_COOKIE_NAME}=not-json"),
            format!("{FLASH_COOKIE_NAME}=%7B%22admin%22%3A%22x%22%7D"),
            format!("{FLASH_COOKIE_NAME}=%7B%22notice%22%3A1%7D"),
            format!("{FLASH_COOKIE_NAME}=%5B%5D"),
            format!(
                "{FLASH_COOKIE_NAME}=%7B%22notice%22%3A%22a%22%7D; {FLASH_COOKIE_NAME}=%7B%22notice%22%3A%22b%22%7D"
            ),
        ] {
            let response = router
                .dispatch(request_with(Method::GET, "/asset", &[("cookie", &jar)]))
                .await;
            assert!(
                set_cookie(&response).is_some_and(|value| value.contains("Max-Age=0")),
                "{jar}"
            );
            let shown = router
                .dispatch(request_with(Method::GET, "/items", &[("cookie", &jar)]))
                .await;
            assert_eq!(text(&shown), "-|-", "{jar}");
        }
    }

    #[test]
    fn oversized_messages_are_refused() {
        let mut response = Response::empty(StatusCode::SEE_OTHER);
        assert_eq!(
            Flash::new()
                .with_alert("x".repeat(MAX_FLASH_MESSAGE_BYTES + 1))
                .set_on(&mut response),
            Err(FlashError::MessageTooLong)
        );
        // Worst-case encoding of two maximal messages exceeds the budget.
        assert_eq!(
            Flash::new()
                .with_notice("\"".repeat(MAX_FLASH_MESSAGE_BYTES))
                .with_alert("\"".repeat(MAX_FLASH_MESSAGE_BYTES))
                .set_on(&mut response),
            Err(FlashError::TooLarge)
        );
        assert!(response.headers().get(header::SET_COOKIE).is_none());

        Flash::new()
            .set_on(&mut response)
            .expect("empty flash is a no-op");
        assert!(response.headers().get(header::SET_COOKIE).is_none());
    }

    #[test]
    fn round_trips_both_messages() {
        let flash = Flash::new().with_notice("Saved").with_alert("But check é");
        let mut response = Response::empty(StatusCode::SEE_OTHER);
        flash.set_on(&mut response).expect("fits");
        let stored = set_cookie(&response).expect("cookie");
        let request = request_with(Method::GET, "/", &[("cookie", &pair(&stored))]);
        assert!(matches!(incoming(&request), Incoming::Valid(parsed) if parsed == flash));
    }
}
