//! Shared fixtures of the micro-benchmarks.

use std::fmt::Debug;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use bytes::Bytes;
use forge::http::{HeaderMap, HeaderValue, Method, Request, Response, StatusCode, Uri, header};
use uuid::Uuid;

/// A canonical 64-character lowercase hex token (the shape of a CSRF token
/// and of a session bearer): fixed, so every run sees the same bytes.
pub const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// Unwraps a fixture step or stops the benchmark naming it. Fixtures are not
/// production paths, and a failing one must never produce a figure.
pub fn must<T, E: Debug>(result: Result<T, E>, what: &str) -> T {
    result.unwrap_or_else(|error| panic!("fixture {what}: {error:?}"))
}

/// What is needed to build the same [`Request`] again: a request is moved into
/// `Router::dispatch`, so each iteration builds one from clones of these
/// parts (a refcount bump for the URI and body, a map clone for the headers).
/// That cost is inside the figures of the scenarios that dispatch; the
/// reference rows `request_build/*` measure it alone.
#[derive(Clone)]
pub struct RequestParts {
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
}

impl RequestParts {
    /// A request with `method` and `uri` and no headers or body.
    pub fn new(method: Method, uri: &str) -> Self {
        Self {
            method,
            uri: must(uri.parse(), "uri"),
            headers: HeaderMap::new(),
            body: Bytes::new(),
        }
    }

    /// Adds a header (the name is lowercase).
    #[must_use]
    pub fn named_header(mut self, name: &'static str, value: &str) -> Self {
        self.headers.append(
            header::HeaderName::from_static(name),
            must(HeaderValue::from_str(value), "header value"),
        );
        self
    }

    /// Sets the body.
    #[must_use]
    pub fn body(mut self, body: impl Into<Bytes>) -> Self {
        self.body = body.into();
        self
    }

    /// A fresh request with these parts.
    pub fn build(&self) -> Request {
        Request::new_for_test(
            self.method.clone(),
            self.uri.clone(),
            self.headers.clone(),
            self.body.clone(),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 54321),
            Uuid::nil(),
        )
    }
}

/// A handler that answers `204 No Content`: no work of its own.
pub async fn no_content(_request: Request) -> Response {
    Response::empty(StatusCode::NO_CONTENT)
}
