//! HTTP transport for Forge applications.
//!
//! The public API is owned by Forge. Hyper provides the wire protocol engine,
//! but handlers receive framework request and response types rather than Hyper
//! internals. This keeps the adapter replaceable and application code stable.

use std::{
    collections::HashMap,
    convert::Infallible,
    fmt,
    future::Future,
    net::{IpAddr, SocketAddr},
    panic::Location,
    pin::Pin,
    sync::{Arc, OnceLock},
    time::Duration,
};

use bytes::Bytes;
use forge_config::ServerConfig;
pub use http::{Extensions, HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use http_body_util::{BodyExt, LengthLimitError, Limited};
use hyper::{
    body::{Body as _, Incoming},
    service::service_fn,
};
use hyper_util::rt::{TokioIo, TokioTimer};
use serde::{Serialize, de::DeserializeOwned};
use thiserror::Error;
use tokio::{
    net::TcpListener,
    sync::{OwnedSemaphorePermit, Semaphore},
    task::{JoinError, JoinSet},
};
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, Span, error, info_span, warn};
use uuid::Uuid;

use crate::body::{OutgoingBody, ResponseBody};

mod body;
#[cfg(test)]
mod body_tests;
mod client_ip;
mod encoding;
mod form;
mod io;
#[cfg(test)]
mod io_tests;
mod json;
#[cfg(test)]
mod media_type_tests;
mod openapi;
mod query;
#[cfg(test)]
mod query_tests;
mod routing;

pub use client_ip::{MAX_FORWARDED_HOPS, TrustedProxies, X_FORWARDED_FOR};
pub use forge_telemetry::{SpanContext, http::ServerInstrumentation};
pub use form::{
    CSRF_FIELD_NAME, FORM_CONTENT_TYPE, FormError, FormErrorKind, MAX_FORM_FIELDS,
    METHOD_FIELD_NAME, RESERVED_FORM_FIELDS,
};
pub use json::{JSON_CONTENT_TYPE, JsonBodyError};
pub use openapi::{
    CheckResponses, ContractFailure, Described, HealthReport, HealthStatus, OPENAPI_PATH, describe,
    health_operation,
};
pub use query::{QueryError, QueryErrorKind};

/// Pause after a failed `accept` so transient errors such as `EMFILE` do not
/// turn the accept loop into a busy loop.
const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(50);
/// The shortest time a response write may make no progress before the
/// connection is ended (the stalled-write guard). The guard is the request
/// timeout, but never less than this, so lowering `FORGE_REQUEST_TIMEOUT_SECS`
/// for a fast API does not cut off clients that read a large download or an
/// event stream slowly; those are only disconnected when they stop reading.
const MIN_WRITE_STALL_TIMEOUT: Duration = Duration::from_secs(30);

type BoxHandlerFuture = Pin<Box<dyn Future<Output = Response> + Send + 'static>>;

/// Response future returned by [`Middleware::handle`] and [`Next::run`].
pub type BoxResponseFuture<'a> = Pin<Box<dyn Future<Output = Response> + Send + 'a>>;

/// An HTTP request detached from the underlying protocol implementation.
#[derive(Debug)]
pub struct Request {
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
    path_params: HashMap<String, String>,
    remote_addr: SocketAddr,
    /// Set only by [`TrustedProxies::apply`].
    client_ip: Option<IpAddr>,
    request_id: Uuid,
    /// The matched route template, set by the router before the handler.
    route: Option<Arc<str>>,
    extensions: Extensions,
}

/// Where the router reports the matched route template to the server's
/// telemetry, which runs outside every layer. Private, so no layer can
/// replace it.
#[derive(Clone, Debug, Default)]
pub(crate) struct MatchedRoute(Arc<OnceLock<Arc<str>>>);

impl Request {
    /// HTTP method.
    #[must_use]
    pub fn method(&self) -> &Method {
        &self.method
    }

    /// Request URI.
    #[must_use]
    pub fn uri(&self) -> &Uri {
        &self.uri
    }

    /// Request headers.
    #[must_use]
    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    /// Buffered body, after the configured size limit was enforced.
    #[must_use]
    pub fn body(&self) -> &Bytes {
        &self.body
    }

    /// Peer address observed by the TCP listener. Behind a reverse proxy
    /// this is the proxy; per-client counters use [`Request::client_ip`].
    #[must_use]
    pub fn remote_addr(&self) -> SocketAddr {
        self.remote_addr
    }

    /// The client address (ADR 0032): the TCP peer, unless the peer is a
    /// trusted proxy listed in `FORGE_HTTP_TRUSTED_PROXIES`, in which case it
    /// is the first untrusted `X-Forwarded-For` entry read from the right
    /// (see [`TrustedProxies::resolve`]). The [`Server`] resolves it before
    /// any [`Middleware`] runs; when no resolver ran (a [`Router`] dispatched
    /// directly) it is the peer, the same as trusting nobody. A client that
    /// reaches the application directly cannot choose it.
    ///
    /// Sign-in and reset throttles, rate limits and the live-update stream
    /// cap count clients by it. It is a full, canonical address (no zone,
    /// IPv4-mapped IPv6 as IPv4); counters bucket IPv6 by `/64` themselves.
    #[must_use]
    pub fn client_ip(&self) -> IpAddr {
        self.client_ip
            .unwrap_or_else(|| self.remote_addr.ip().to_canonical())
    }

    /// Server-generated UUIDv7 correlation identifier.
    #[must_use]
    pub fn request_id(&self) -> Uuid {
        self.request_id
    }

    /// The template of the matched route, such as `/customers/{id}`; `None`
    /// before routing (in a [`Middleware`]).
    #[must_use]
    pub fn route(&self) -> Option<&str> {
        self.route.as_deref()
    }

    /// The trace context of the request's server span when telemetry is on
    /// (ADR 0033): parent child spans with it, or propagate it to a
    /// downstream service with [`SpanContext::inject`].
    #[must_use]
    pub fn trace_context(&self) -> Option<&SpanContext> {
        self.extensions.get::<SpanContext>()
    }

    /// Typed per-request values attached by [`Middleware`] layers (for
    /// example the CSRF token a page must render) and read by handlers.
    #[must_use]
    pub fn extensions(&self) -> &Extensions {
        &self.extensions
    }

    /// Mutable access to the per-request values, for [`Middleware`] layers.
    #[must_use]
    pub fn extensions_mut(&mut self) -> &mut Extensions {
        &mut self.extensions
    }

    /// Replaces the request method before routing.
    ///
    /// This exists for [`Middleware`] that runs before the route table, such
    /// as the CSRF-verified `_method` override of HTML forms; a handler never
    /// needs it because routing has already happened.
    pub fn set_method(&mut self, method: Method) {
        self.method = method;
    }

    /// Replaces the request URI before routing.
    ///
    /// This exists for [`Middleware`] that rewrites the path the route table
    /// sees, such as the locale prefix `forge_views::LocaleNegotiation`
    /// removes (`/pt-BR/posts` is routed as `/posts`). Layers registered
    /// before it, and the server's own request telemetry, keep seeing the
    /// original URI.
    pub fn set_uri(&mut self, uri: Uri) {
        self.uri = uri;
    }

    /// Creates a new request for testing purposes.
    ///
    /// This constructor is intended for use in tests only.
    pub fn new_for_test(
        method: Method,
        uri: Uri,
        headers: HeaderMap,
        body: Bytes,
        remote_addr: SocketAddr,
        request_id: Uuid,
    ) -> Self {
        Self {
            method,
            uri,
            headers,
            body,
            path_params: HashMap::new(),
            remote_addr,
            client_ip: None,
            request_id,
            route: None,
            extensions: Extensions::new(),
        }
    }

    /// Decodes a JSON request body. It ignores the `Content-Type` and
    /// unknown members; [`Request::json_body`] and
    /// [`Request::json_body_strict`] enforce them.
    pub fn json<T: DeserializeOwned>(&self) -> Result<T, JsonError> {
        serde_json::from_slice(&self.body).map_err(JsonError::Decode)
    }

    /// Decodes the query string into `T`.
    ///
    /// The query string is the part of the URI after `?`. It uses strict
    /// `application/x-www-form-urlencoded` decoding and rejects empty segments,
    /// repeated parameters, and unknown parameters when the target type uses
    /// `#[serde(deny_unknown_fields)]`.
    pub fn query<T: DeserializeOwned>(&self) -> Result<T, QueryError> {
        crate::query::decode(self.uri.query())
    }

    /// Decodes an HTML form body (`application/x-www-form-urlencoded`) into
    /// `T`.
    ///
    /// The request must carry exactly one form `Content-Type` (an optional
    /// `charset` must be UTF-8) and at most [`MAX_FORM_FIELDS`] pairs. The
    /// grammar is the strict query-string grammar; a field may repeat only
    /// when it is a sequence (`Vec<T>`), `bool` accepts the checkbox values
    /// `on`/`1`/`true` and `0`/`false` (declare checkboxes
    /// `#[serde(default)] bool`, since an unchecked box sends nothing), and a
    /// blank value is `None` for `Option` fields. The framework fields in
    /// [`RESERVED_FORM_FIELDS`] (`csrf_token`, `_method`) are skipped, so
    /// `#[serde(deny_unknown_fields)]` rejects only real unknown fields.
    pub fn form<T: DeserializeOwned>(&self) -> Result<T, FormError> {
        crate::form::decode(&self.headers, &self.body)
    }

    /// Returns the single decoded value of form field `name`, including the
    /// reserved framework fields. `Ok(None)` when the field is absent; a
    /// repeated field is [`FormErrorKind::DuplicateField`] and a request
    /// without a form body is [`FormErrorKind::UnsupportedMediaType`].
    pub fn form_value(&self, name: &str) -> Result<Option<String>, FormError> {
        crate::form::value(&self.headers, &self.body, name)
    }

    /// Whether the request carries exactly one form-encoded (UTF-8)
    /// `Content-Type`.
    #[must_use]
    pub fn has_form_body(&self) -> bool {
        crate::form::is_form(&self.headers)
    }

    /// Parses a named parameter captured by a dynamic route such as
    /// `/customers/{id}`. Percent escapes are decoded before parsing; `+`
    /// remains a literal plus, as it does in a URL path. Returns an error if the
    /// parameter is missing, malformed or cannot be parsed as `T`.
    pub fn path_param_as<T>(&self, name: &str) -> Result<T, PathParamError>
    where
        T: std::str::FromStr,
        T::Err: Into<Box<dyn std::error::Error + Send + Sync>>,
    {
        let value = self
            .path_params
            .get(name)
            .ok_or_else(|| PathParamError::Missing {
                name: name.to_owned(),
            })?;
        let decoded =
            crate::encoding::percent_decode(value, false).map_err(|_| PathParamError::Invalid {
                name: name.to_owned(),
            })?;
        decoded.parse().map_err(|_| PathParamError::Invalid {
            name: name.to_owned(),
        })
    }
}

/// Framework-owned HTTP response.
///
/// The body is either buffered ([`Response::new`]) or streamed
/// ([`Response::stream`]), for long-lived responses such as Server-Sent
/// Events.
pub struct Response {
    status: StatusCode,
    headers: HeaderMap,
    body: ResponseBody,
}

impl fmt::Debug for Response {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = formatter.debug_struct("Response");
        debug
            .field("status", &self.status)
            .field("headers", &self.headers);
        match &self.body {
            ResponseBody::Full(bytes) => debug.field("body", bytes),
            ResponseBody::Stream(_) => debug.field("body", &"<stream>"),
        };
        debug.finish()
    }
}

/// The body of a streamed response before its first chunk (and the body
/// [`Response::body`] reports for streamed responses).
static EMPTY_BODY: Bytes = Bytes::from_static(b"");

impl Response {
    /// Creates a response with a status and raw body.
    #[must_use]
    pub fn new(status: StatusCode, body: impl Into<Bytes>) -> Self {
        Self {
            status,
            headers: HeaderMap::new(),
            body: ResponseBody::Full(body.into()),
        }
    }

    /// Creates a response whose body is produced while it is being sent.
    ///
    /// Starting from `state`, the server calls `step` for each chunk: it
    /// returns the chunk and the next state, or `None` to end the body.
    /// Chunks are written (with chunked transfer encoding) as soon as they
    /// are produced; empty chunks are skipped (after 32 in a row the body
    /// yields to the scheduler, so a step that never produces data does not
    /// hold a worker thread). `step` runs on the
    /// connection's task, so the body is owned by the connection: it stops
    /// when the client disconnects, when a write makes no progress for the
    /// request timeout or 30 seconds, whichever is longer
    /// (the peer stopped reading), or when shutdown aborts
    /// connections after the grace period. A body that should end on
    /// graceful shutdown must watch a cancellation signal of its own.
    ///
    /// ```
    /// use forge_http::{Response, StatusCode};
    ///
    /// let response = Response::stream(StatusCode::OK, 0_u8, |count| async move {
    ///     (count < 3).then(|| (bytes::Bytes::from(format!("tick {count}\n")), count + 1))
    /// });
    /// assert!(response.is_stream());
    /// ```
    pub fn stream<S, F, Fut>(status: StatusCode, state: S, step: F) -> Self
    where
        S: Send + 'static,
        F: FnMut(S) -> Fut + Send + 'static,
        Fut: Future<Output = Option<(Bytes, S)>> + Send + 'static,
    {
        Self {
            status,
            headers: HeaderMap::new(),
            body: ResponseBody::Stream(Box::new(body::Unfold::new(state, step))),
        }
    }

    /// Whether the body is streamed ([`Response::stream`]).
    #[must_use]
    pub fn is_stream(&self) -> bool {
        matches!(self.body, ResponseBody::Stream(_))
    }

    /// Takes the next chunk of the body: the whole body once for a buffered
    /// response, each produced chunk for a streamed one, then `None`.
    ///
    /// For in-process consumers and tests that read a response without a
    /// network connection, like [`Router::dispatch`].
    pub async fn next_chunk(&mut self) -> Option<Bytes> {
        match &mut self.body {
            ResponseBody::Full(bytes) if bytes.is_empty() => None,
            ResponseBody::Full(bytes) => Some(std::mem::take(bytes)),
            ResponseBody::Stream(source) => {
                std::future::poll_fn(|context| source.poll_chunk(context)).await
            }
        }
    }

    /// Creates a JSON response without exposing serialization errors.
    pub fn json<T: Serialize>(status: StatusCode, value: &T) -> Result<Self, JsonError> {
        let body = serde_json::to_vec(value).map_err(JsonError::Encode)?;
        let mut response = Self::new(status, body);
        response.headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json; charset=utf-8"),
        );
        Ok(response)
    }

    /// Creates a `text/plain; charset=utf-8` response, the media type the
    /// OpenAPI document declares for Forge's own refusals (`401`, `403`,
    /// `413`).
    #[must_use]
    pub fn text(status: StatusCode, body: impl Into<Bytes>) -> Self {
        let mut response = Self::new(status, body);
        response.headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/plain; charset=utf-8"),
        );
        response
    }

    /// Creates an empty response.
    #[must_use]
    pub fn empty(status: StatusCode) -> Self {
        Self::new(status, Bytes::new())
    }

    /// Mutably accesses headers for adapter-specific response metadata.
    #[must_use]
    pub fn headers_mut(&mut self) -> &mut HeaderMap {
        &mut self.headers
    }

    /// Returns response headers for adapters and in-process consumers.
    #[must_use]
    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    /// Returns the response status code.
    #[must_use]
    pub fn status(&self) -> StatusCode {
        self.status
    }

    /// Returns the buffered response body; empty for a streamed response
    /// (read those with [`Response::next_chunk`]).
    #[must_use]
    pub fn body(&self) -> &Bytes {
        match &self.body {
            ResponseBody::Full(bytes) => bytes,
            ResponseBody::Stream(_) => &EMPTY_BODY,
        }
    }
}

/// JSON boundary error.
#[derive(Debug, Error)]
pub enum JsonError {
    /// Request JSON did not match the requested type.
    #[error("request body is not valid JSON")]
    Decode(#[source] serde_json::Error),
    /// Response serialization failed.
    #[error("response could not be serialized")]
    Encode(#[source] serde_json::Error),
}

/// Path parameter extraction error.
#[derive(Debug, Error, Eq, PartialEq)]
pub enum PathParamError {
    /// A required path parameter was not found in the route.
    #[error("missing path parameter {name}")]
    Missing { name: String },
    /// A path parameter could not be parsed as the requested type.
    #[error("invalid path parameter {name}")]
    Invalid { name: String },
}

impl PathParamError {
    /// Returns the parameter name.
    pub fn name(&self) -> &str {
        match self {
            Self::Missing { name } | Self::Invalid { name } => name,
        }
    }
}

/// Object-safe asynchronous HTTP handler.
pub trait Handler: Send + Sync + 'static {
    /// Handles one already-validated transport request.
    fn call(&self, request: Request) -> BoxHandlerFuture;

    /// The route's OpenAPI description, when the handler carries one
    /// ([`describe`], `forge::openapi::typed`). Undescribed routes are listed
    /// as undocumented, never guessed at.
    fn operation(&self) -> Option<&forge_openapi::Operation> {
        None
    }
}

impl<F, Fut> Handler for F
where
    F: Fn(Request) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Response> + Send + 'static,
{
    fn call(&self, request: Request) -> BoxHandlerFuture {
        Box::pin((self)(request))
    }
}

/// A request layer that runs before routing.
///
/// Layers see every request the router receives, in registration order (the
/// first registered layer is the outermost), and may change it (for example
/// its method), attach [`Request::extensions`], answer without calling the
/// rest of the chain, or adjust the response that [`Next::run`] returns.
pub trait Middleware: Send + Sync + 'static {
    /// Handles `request`; call `next.run(request)` to continue to the
    /// remaining layers and then the route table.
    fn handle<'a>(&'a self, request: Request, next: Next<'a>) -> BoxResponseFuture<'a>;
}

/// The rest of the layer chain, ending with route matching.
pub struct Next<'a> {
    layers: &'a [Arc<dyn Middleware>],
    router: &'a Router,
}

impl fmt::Debug for Next<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Next")
            .field("remaining_layers", &self.layers.len())
            .finish_non_exhaustive()
    }
}

impl<'a> Next<'a> {
    /// Runs the remaining layers, then routes the request.
    pub fn run(self, request: Request) -> BoxResponseFuture<'a> {
        match self.layers.split_first() {
            Some((layer, layers)) => layer.handle(
                request,
                Next {
                    layers,
                    router: self.router,
                },
            ),
            None => Box::pin(self.router.route_request(request)),
        }
    }
}

/// A registered route and the description its handler carried.
struct RegisteredRoute {
    method: Method,
    path: String,
    operation: Option<Arc<forge_openapi::Operation>>,
    source: &'static Location<'static>,
}

/// One route of a [`Router`], as listed by [`Router::routes`].
#[derive(Clone, Copy, Debug)]
pub struct RouteInfo<'a> {
    /// The HTTP method.
    pub method: &'a Method,
    /// The pattern as registered, such as `/posts/{id}`.
    pub path: &'a str,
    /// The source line of the [`Router::route`] call that registered it
    /// (`file()` is as the compiler saw it, relative to the package's
    /// workspace for the application's own files).
    pub source: &'static Location<'static>,
}

/// Immutable route table once attached to a server.
#[derive(Default)]
pub struct Router {
    root: routing::Node,
    routes: Vec<RegisteredRoute>,
    layers: Vec<Arc<dyn Middleware>>,
}

impl fmt::Debug for Router {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut routes: Vec<String> = self
            .routes
            .iter()
            .map(|route| format!("{} {}", route.method, route.path))
            .collect();
        routes.sort_unstable();
        formatter
            .debug_struct("Router")
            .field("routes", &routes)
            .field("layers", &self.layers.len())
            .finish()
    }
}

impl Router {
    /// Creates an empty route table.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a route and rejects duplicate or ambiguous method/pattern
    /// pairs. Whole path segments may be named captures such as `{id}`.
    /// Static segments take precedence over capture segments during dispatch.
    /// The call site is kept for [`Router::routes`].
    #[track_caller]
    pub fn route(
        &mut self,
        method: Method,
        path: impl Into<String>,
        handler: impl Handler,
    ) -> Result<&mut Self, RouteError> {
        let path = path.into();
        let operation = handler.operation().cloned().map(Arc::new);
        let segments = routing::parse_pattern(&path)?;
        let params = segments
            .iter()
            .filter_map(|segment| match segment {
                routing::Segment::Param(name) => Some(Arc::clone(name)),
                routing::Segment::Static(_) => None,
            })
            .collect::<Vec<_>>();
        self.root.insert(
            segments,
            routing::Endpoint {
                method: method.clone(),
                path: Arc::from(path.as_str()),
                params: params.into_boxed_slice(),
                handler: Arc::new(handler),
            },
        )?;
        self.routes.push(RegisteredRoute {
            method,
            path,
            operation,
            source: Location::caller(),
        });
        Ok(self)
    }

    /// Every registered route, in registration order, with the source line
    /// that registered it. This is what `forge routes` prints.
    pub fn routes(&self) -> impl ExactSizeIterator<Item = RouteInfo<'_>> {
        self.routes.iter().map(|route| RouteInfo {
            method: &route.method,
            path: &route.path,
            source: route.source,
        })
    }

    /// Adds a [`Middleware`] layer that runs before routing for every request,
    /// including requests that match no route. Layers run in registration
    /// order: the first one added is the outermost.
    pub fn layer(&mut self, middleware: impl Middleware) -> &mut Self {
        self.layers.push(Arc::new(middleware));
        self
    }

    /// Registers the standard liveness and readiness probes.
    ///
    /// `GET /health`, `GET /health/live` and `GET /health/ready` answer `200`
    /// with `application/json` bodies `{"status":"ok"}`, `{"status":"live"}`
    /// and `{"status":"ready"}` respectively.
    /// They are described in the OpenAPI document ([`health_operation`]).
    pub fn standard_health_routes(&mut self) -> Result<&mut Self, RouteError> {
        self.route(
            Method::GET,
            "/health",
            describe(
                |_| async { static_json(StatusCode::OK, r#"{"status":"ok"}"#) },
                health_operation("/health", false),
            ),
        )?;
        self.route(
            Method::GET,
            "/health/live",
            describe(
                |_| async { static_json(StatusCode::OK, r#"{"status":"live"}"#) },
                health_operation("/health/live", false),
            ),
        )?;
        self.route(
            Method::GET,
            "/health/ready",
            describe(
                |_| async { static_json(StatusCode::OK, r#"{"status":"ready"}"#) },
                health_operation("/health/ready", false),
            ),
        )
    }

    /// Runs one request through the layers and the route table without a
    /// network connection.
    ///
    /// Layers added with [`Router::layer`] run first, in registration order;
    /// then the request is routed. The raw (still percent-encoded) path is matched segment by segment;
    /// static segments win over `{name}` captures. Unmatched paths answer
    /// `404`. `HEAD` is served by the path's `GET` route unless a `HEAD`
    /// route is registered; the handler sees the `HEAD` method, and the
    /// [`Server`] sends the headers without the body (here the body is
    /// left in place). A matched path without a handler for the request
    /// method answers `405` with an `Allow` header listing the served methods
    /// in sorted order (`HEAD` included wherever `GET` is). A captured segment containing an encoded slash (`%2F`)
    /// answers `400`, because it would let a client smuggle a path separator
    /// past routing; other malformed escapes are reported by
    /// [`Request::path_param_as`] when the handler reads the parameter.
    pub async fn dispatch(&self, request: Request) -> Response {
        Next {
            layers: &self.layers,
            router: self,
        }
        .run(request)
        .await
    }

    /// Matches the request against the route table and calls its handler.
    async fn route_request(&self, request: Request) -> Response {
        let path = request.uri.path().to_owned();
        let mut captures = Vec::new();
        let Some(node) = self.root.find(&path, &mut captures) else {
            return Response::new(StatusCode::NOT_FOUND, "not found");
        };
        let Some(endpoint) = node.serving(&request.method) else {
            let allow = node.allow();
            let mut response = Response::new(StatusCode::METHOD_NOT_ALLOWED, "method not allowed");
            if let Ok(value) = HeaderValue::from_str(&allow) {
                response.headers.insert(header::ALLOW, value);
            }
            return response;
        };
        if captures.iter().any(|value| has_encoded_slash(value)) {
            return Response::new(StatusCode::BAD_REQUEST, "bad request");
        }
        let params = endpoint
            .params
            .iter()
            .zip(captures)
            .map(|(name, value)| (name.to_string(), value.to_owned()))
            .collect();
        let mut request = request;
        request.path_params = params;
        if let Some(matched) = request.extensions.get::<MatchedRoute>() {
            let _ = matched.0.set(Arc::clone(&endpoint.path));
        }
        request.route = Some(Arc::clone(&endpoint.path));
        endpoint.handler.call(request).await
    }
}

/// Whether `segment` contains `%2F` or `%2f`.
fn has_encoded_slash(segment: &str) -> bool {
    segment.as_bytes().windows(3).any(|window| {
        window[0] == b'%' && window[1] == b'2' && window[2].eq_ignore_ascii_case(&b'f')
    })
}

fn static_json(status: StatusCode, body: &'static str) -> Response {
    let mut response = Response::new(status, body);
    response.headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(JSON_CONTENT_TYPE),
    );
    response
}

/// Route registration error detected before serving traffic.
#[derive(Debug, Error, Eq, PartialEq)]
pub enum RouteError {
    /// Paths must be absolute and must not include query or fragment syntax.
    #[error("invalid route path {0:?}")]
    InvalidPath(String),
    /// A method/path pair may be registered only once.
    #[error("duplicate route: {method} {path}")]
    Duplicate {
        /// HTTP method.
        method: Method,
        /// Absolute request path.
        path: String,
    },
    /// A route pattern uses the same parameter name more than once.
    #[error("duplicate parameter {name:?} in route path {path:?}")]
    DuplicateParameter { path: String, name: String },
    /// Two route patterns have the same shape and method but different parameter names.
    #[error("ambiguous route: {method} {path} conflicts with existing route {existing}")]
    Ambiguous {
        /// HTTP method.
        method: Method,
        /// The newly registered route pattern.
        path: String,
        /// The route pattern that was already registered.
        existing: String,
    },
}

/// HTTP server with bounded requests and owned connection tasks.
pub struct Server {
    config: ServerConfig,
    app: Arc<ServedApp>,
    /// [`MIN_WRITE_STALL_TIMEOUT`]; lowered only by tests.
    min_write_stall: Duration,
}

impl Server {
    /// Creates a server from validated configuration and a frozen router.
    ///
    /// Every request's [`Request::client_ip`] is resolved with
    /// [`ServerConfig::trusted_proxies`] before the first [`Middleware`]
    /// runs.
    #[must_use]
    pub fn new(config: ServerConfig, router: Router) -> Self {
        Self {
            app: Arc::new(ServedApp {
                trusted_proxies: TrustedProxies::new(config.trusted_proxies.iter().copied()),
                router,
                telemetry: None,
            }),
            config,
            min_write_stall: MIN_WRITE_STALL_TIMEOUT,
        }
    }

    /// Instruments every request (ADR 0033): a server span, continued from
    /// an incoming `traceparent` only when the telemetry trusts it, whose
    /// `trace_id` and `span_id` are fields of the request's log span and
    /// whose context is [`Request::trace_context`]; and the
    /// `http.server.request.duration` and `http.server.active_requests`
    /// metrics labelled with the route template, never the raw path. It runs
    /// outside every [`Middleware`], so requests a layer answers are
    /// measured too.
    #[must_use]
    pub fn with_telemetry(mut self, instrumentation: ServerInstrumentation) -> Self {
        if let Some(app) = Arc::get_mut(&mut self.app) {
            app.telemetry = Some(instrumentation);
        }
        self
    }

    /// Binds the configured address and serves until the shutdown future resolves.
    pub async fn run(self, shutdown: impl Future<Output = ()> + Send) -> Result<(), ServerError> {
        let listener = TcpListener::bind(self.config.bind)
            .await
            .map_err(ServerError::Bind)?;
        self.serve(listener, shutdown).await
    }

    /// Serves on an existing listener, useful for socket activation and tests.
    ///
    /// At most [`ServerConfig::max_connections`] connections are open at once;
    /// while saturated the server stops accepting, leaving new clients in the
    /// kernel backlog. Transient `accept` failures are logged and retried.
    /// When `shutdown` resolves the server stops accepting, asks open
    /// connections to finish their in-flight request, and aborts whatever is
    /// still running after [`ServerConfig::shutdown_grace`].
    pub async fn serve(
        self,
        listener: TcpListener,
        shutdown: impl Future<Output = ()> + Send,
    ) -> Result<(), ServerError> {
        let cancellation = CancellationToken::new();
        let mut connections = JoinSet::new();
        let permits = Arc::new(Semaphore::new(
            self.config.max_connections.clamp(1, Semaphore::MAX_PERMITS),
        ));
        tokio::pin!(shutdown);

        'accept: loop {
            // Admission control happens before `accept`: while every permit is
            // held, pending clients wait in the listen backlog.
            let permit = tokio::select! {
                biased;
                () = &mut shutdown => break 'accept,
                Some(completed) = connections.join_next(), if !connections.is_empty() => {
                    log_connection_result(completed);
                    continue 'accept;
                }
                permit = Arc::clone(&permits).acquire_owned() => match permit {
                    Ok(permit) => permit,
                    // The semaphore is owned by this function and never closed.
                    Err(_closed) => break 'accept,
                },
            };

            loop {
                tokio::select! {
                    biased;
                    () = &mut shutdown => break 'accept,
                    Some(completed) = connections.join_next(), if !connections.is_empty() => {
                        log_connection_result(completed);
                    }
                    accepted = listener.accept() => match accepted {
                        Ok((stream, remote_addr)) => {
                            self.spawn_connection(
                                &mut connections,
                                stream,
                                remote_addr,
                                permit,
                                cancellation.child_token(),
                            );
                            continue 'accept;
                        }
                        Err(error) => {
                            warn!(%error, "failed to accept HTTP connection; retrying");
                            tokio::select! {
                                biased;
                                () = &mut shutdown => break 'accept,
                                () = tokio::time::sleep(ACCEPT_ERROR_BACKOFF) => {}
                            }
                        }
                    },
                }
            }
        }

        drop(listener);
        cancellation.cancel();
        let grace = tokio::time::sleep(self.config.shutdown_grace);
        tokio::pin!(grace);
        // Drain until every connection has finished or the grace period ends.
        while !connections.is_empty() {
            tokio::select! {
                () = &mut grace => {
                    warn!(
                        remaining = connections.len(),
                        "shutdown grace period elapsed; aborting open connections"
                    );
                    connections.abort_all();
                    break;
                }
                Some(completed) = connections.join_next() => {
                    if let Err(error) = completed {
                        warn!(%error, "connection task failed during shutdown");
                    }
                }
            }
        }

        while connections.join_next().await.is_some() {}
        Ok(())
    }

    fn spawn_connection(
        &self,
        connections: &mut JoinSet<()>,
        stream: tokio::net::TcpStream,
        remote_addr: SocketAddr,
        permit: OwnedSemaphorePermit,
        cancellation: CancellationToken,
    ) {
        let app = Arc::clone(&self.app);
        let max_body_bytes = self.config.max_body_bytes;
        let request_timeout = self.config.request_timeout;
        let write_stall = write_stall_timeout(request_timeout, self.min_write_stall);
        connections.spawn(async move {
            serve_connection(
                stream,
                remote_addr,
                app,
                max_body_bytes,
                request_timeout,
                write_stall,
                cancellation,
            )
            .await;
            // The connection slot is released only once the connection ends.
            drop(permit);
        });
    }
}

/// How long a response write may make no progress: the request timeout, but
/// at least `floor` ([`MIN_WRITE_STALL_TIMEOUT`]).
fn write_stall_timeout(request_timeout: Duration, floor: Duration) -> Duration {
    request_timeout.max(floor)
}

fn log_connection_result(completed: Result<(), JoinError>) {
    if let Err(error) = completed {
        warn!(%error, "connection task failed");
    }
}

/// What every connection of a [`Server`] shares.
struct ServedApp {
    router: Router,
    trusted_proxies: TrustedProxies,
    telemetry: Option<ServerInstrumentation>,
}

async fn serve_connection(
    stream: tokio::net::TcpStream,
    remote_addr: SocketAddr,
    app: Arc<ServedApp>,
    max_body_bytes: usize,
    request_timeout: Duration,
    write_stall: Duration,
    cancellation: CancellationToken,
) {
    let service = service_fn(move |request| {
        handle_request(
            request,
            remote_addr,
            Arc::clone(&app),
            max_body_bytes,
            request_timeout,
        )
    });
    // A write that makes no progress for `write_stall` (the request timeout,
    // at least 30 seconds) means the peer stopped reading; end the connection
    // rather than hold its slot. Slow writes that progress never trip it.
    let stream = io::StallTimeout::new(stream, write_stall);
    let connection = hyper::server::conn::http1::Builder::new()
        .timer(TokioTimer::new())
        .header_read_timeout(request_timeout)
        .serve_connection(TokioIo::new(stream), service);
    tokio::pin!(connection);

    tokio::select! {
        result = connection.as_mut() => {
            if let Err(error) = result {
                warn!(%error, %remote_addr, "HTTP connection failed");
            }
        }
        () = cancellation.cancelled() => {
            connection.as_mut().graceful_shutdown();
            if let Err(error) = connection.await {
                warn!(%error, %remote_addr, "HTTP connection failed during shutdown");
            }
        }
    }
}

async fn handle_request(
    request: hyper::Request<Incoming>,
    remote_addr: SocketAddr,
    app: Arc<ServedApp>,
    max_body_bytes: usize,
    request_timeout: Duration,
) -> Result<hyper::Response<OutgoingBody>, Infallible> {
    let request_id = Uuid::now_v7();
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let telemetry = app.telemetry.as_ref().map(|instrumentation| {
        instrumentation.start(&method, request.version(), request.headers())
    });
    let trace = telemetry
        .as_ref()
        .map(forge_telemetry::http::RequestTelemetry::context);
    let span = request_span(request_id, &method, &path, remote_addr, trace.as_ref());
    let head = method == Method::HEAD;
    let response = async move {
        let matched = MatchedRoute::default();
        let (parts, body) = request.into_parts();
        // The deadline covers reading the body as well as the handler, so a
        // client trickling body bytes cannot hold a connection indefinitely.
        let exchange = async {
            let body = match collect_body(body, max_body_bytes).await {
                Ok(body) => body,
                Err(rejection) => return rejection.into_response(),
            };
            let mut request = Request {
                method: parts.method,
                uri: parts.uri,
                headers: parts.headers,
                body,
                path_params: HashMap::new(),
                remote_addr,
                client_ip: None,
                request_id,
                route: None,
                extensions: Extensions::new(),
            };
            if let Some(trace) = trace {
                request.extensions.insert(trace);
                request.extensions.insert(matched.clone());
            }
            // The client address is resolved once, outside every layer.
            app.trusted_proxies.apply(&mut request);
            app.router.dispatch(request).await
        };
        let exchange = tokio::time::timeout(request_timeout, exchange);
        let response = match io::CatchUnwind::new(exchange).await {
            Ok(Ok(response)) => response,
            Ok(Err(_elapsed)) => Response::new(StatusCode::GATEWAY_TIMEOUT, "request timed out"),
            Err(panic) => {
                // Like Forge-Go's recoverer: logged here, in the request's
                // span, and answered with a generic 500 that carries the
                // request id; the panic message never reaches the client.
                error!(
                    %request_id,
                    panic = panic_message(panic.as_ref()),
                    "request handler panicked"
                );
                Response::text(StatusCode::INTERNAL_SERVER_ERROR, "internal server error")
            }
        };
        if let Some(telemetry) = telemetry {
            telemetry.finish(matched.0.get().map(AsRef::as_ref), response.status.as_u16());
        }
        let response = if head {
            without_body(response)
        } else {
            response
        };
        to_hyper_response(response, request_id)
    }
    .instrument(span)
    .await;

    Ok(response)
}

/// The text of a panic payload for the log (`panic!` with a literal or a
/// formatted message); other payloads are not printable.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> &str {
    payload
        .downcast_ref::<&'static str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("non-string panic payload")
}

/// The log span of a request; with telemetry it carries the trace and span
/// identifiers, so every line logged inside the request is correlated.
fn request_span(
    request_id: Uuid,
    method: &Method,
    path: &str,
    remote_addr: SocketAddr,
    trace: Option<&SpanContext>,
) -> Span {
    match trace {
        Some(trace) => info_span!(
            "http.request",
            %request_id,
            %method,
            %path,
            %remote_addr,
            trace_id = %trace.trace_id,
            span_id = %trace.span_id,
        ),
        None => info_span!("http.request", %request_id, %method, %path, %remote_addr),
    }
}

/// Reason a request body was rejected before dispatch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BodyRejection {
    /// The body exceeds `max_body_bytes`.
    TooLarge,
    /// The body stream failed, e.g. malformed chunked encoding.
    Unreadable,
}

impl BodyRejection {
    fn into_response(self) -> Response {
        match self {
            Self::TooLarge => {
                Response::text(StatusCode::PAYLOAD_TOO_LARGE, "request body too large")
            }
            Self::Unreadable => {
                Response::text(StatusCode::BAD_REQUEST, "request body could not be read")
            }
        }
    }
}

/// Buffers a request body, enforcing the configured size limit.
async fn collect_body(body: Incoming, max_body_bytes: usize) -> Result<Bytes, BodyRejection> {
    // Reject a declared Content-Length above the limit without reading it.
    let limit = u64::try_from(max_body_bytes).unwrap_or(u64::MAX);
    if body.size_hint().lower() > limit {
        return Err(BodyRejection::TooLarge);
    }
    match Limited::new(body, max_body_bytes).collect().await {
        Ok(collected) => Ok(collected.to_bytes()),
        Err(error) if error.is::<LengthLimitError>() => Err(BodyRejection::TooLarge),
        Err(_) => Err(BodyRejection::Unreadable),
    }
}

/// The answer to a `HEAD` request: the headers of the response the route
/// produced, without its body (RFC 9110 §9.3.2). A buffered body becomes its
/// `Content-Length` (unless the handler set one, or the status has no
/// content); a streamed body is dropped without being polled and its length,
/// unknown, is left out.
fn without_body(mut response: Response) -> Response {
    let body = std::mem::replace(&mut response.body, ResponseBody::Full(Bytes::new()));
    let may_have_length = !(response.status.is_informational()
        || response.status == StatusCode::NO_CONTENT
        || response.status == StatusCode::NOT_MODIFIED);
    if let ResponseBody::Full(bytes) = body
        && may_have_length
        && !response.headers.contains_key(header::CONTENT_LENGTH)
    {
        response
            .headers
            .insert(header::CONTENT_LENGTH, HeaderValue::from(bytes.len()));
    }
    response
}

fn to_hyper_response(response: Response, request_id: Uuid) -> hyper::Response<OutgoingBody> {
    let mut outgoing = hyper::Response::new(OutgoingBody::from(response.body));
    *outgoing.status_mut() = response.status;
    *outgoing.headers_mut() = response.headers;
    let request_id = HeaderValue::from_str(&request_id.to_string())
        .unwrap_or_else(|error| unreachable!("UUID is always a valid header value: {error}"));
    outgoing.headers_mut().insert("x-request-id", request_id);
    outgoing.headers_mut().insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    outgoing
        .headers_mut()
        .insert("x-frame-options", HeaderValue::from_static("DENY"));
    outgoing
}

/// Server startup failure.
///
/// Failures to accept individual connections are transient and are logged
/// and retried rather than reported here.
#[derive(Debug, Error)]
pub enum ServerError {
    /// The configured socket could not be bound.
    #[error("failed to bind HTTP listener")]
    Bind(#[source] std::io::Error),
}

#[cfg(test)]
mod tests;
