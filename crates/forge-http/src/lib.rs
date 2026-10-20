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
use tracing::{Instrument, Span, info_span, warn};
use uuid::Uuid;

use crate::body::{OutgoingBody, ResponseBody};

mod body;
mod client_ip;
mod encoding;
mod form;
mod io;
mod openapi;
mod query;
mod routing;

pub use client_ip::{MAX_FORWARDED_HOPS, TrustedProxies, X_FORWARDED_FOR};
pub use forge_telemetry::{SpanContext, http::ServerInstrumentation};
pub use form::{
    CSRF_FIELD_NAME, FORM_CONTENT_TYPE, FormError, FormErrorKind, MAX_FORM_FIELDS,
    METHOD_FIELD_NAME, RESERVED_FORM_FIELDS,
};
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
const JSON_CONTENT_TYPE: &str = "application/json";

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

    /// Decodes a JSON request body.
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
        });
        Ok(self)
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
    /// `404`. A matched path without a handler for the request method
    /// answers `405` with an `Allow` header listing the registered methods in
    /// sorted order. A captured segment containing an encoded slash (`%2F`)
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
        let Some(endpoint) = node.endpoint(&request.method) else {
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
                // Recorded as a server error, then the panic continues as
                // before: the connection task fails and the client is cut off.
                if let Some(telemetry) = telemetry {
                    telemetry.finish(matched.0.get().map(AsRef::as_ref), 500);
                }
                std::panic::resume_unwind(panic);
            }
        };
        if let Some(telemetry) = telemetry {
            telemetry.finish(matched.0.get().map(AsRef::as_ref), response.status.as_u16());
        }
        to_hyper_response(response, request_id)
    }
    .instrument(span)
    .await;

    Ok(response)
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
                Response::new(StatusCode::PAYLOAD_TOO_LARGE, "request body too large")
            }
            Self::Unreadable => {
                Response::new(StatusCode::BAD_REQUEST, "request body could not be read")
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
mod tests {
    use std::{
        io::{ErrorKind, Read, Write},
        net::TcpStream as StdTcpStream,
    };

    use tokio::{sync::oneshot, task::JoinHandle};

    use super::*;

    /// Upper bound for any single client wait; generous to avoid CI flakiness.
    const CLIENT_TIMEOUT: Duration = Duration::from_secs(10);

    fn request(method: Method, path: &str) -> Request {
        Request {
            method,
            uri: path.parse().expect("valid test URI"),
            headers: HeaderMap::new(),
            body: Bytes::new(),
            path_params: HashMap::new(),
            remote_addr: "127.0.0.1:4000".parse().expect("valid test address"),
            client_ip: None,
            request_id: Uuid::now_v7(),
            route: None,
            extensions: Extensions::new(),
        }
    }

    /// Test layer: records its name in the `x-trace` request/response
    /// headers, optionally rewrites the method or answers directly.
    struct Trace {
        name: &'static str,
        rewrite: Option<Method>,
        answer: Option<StatusCode>,
    }

    impl Middleware for Trace {
        fn handle<'a>(&'a self, mut request: Request, next: Next<'a>) -> BoxResponseFuture<'a> {
            Box::pin(async move {
                if let Some(status) = self.answer {
                    return Response::empty(status);
                }
                if let Some(method) = &self.rewrite {
                    request.set_method(method.clone());
                }
                request.extensions_mut().insert(self.name);
                let mut response = next.run(request).await;
                response
                    .headers_mut()
                    .append("x-trace", HeaderValue::from_static(self.name));
                response
            })
        }
    }

    fn trace(name: &'static str) -> Trace {
        Trace {
            name,
            rewrite: None,
            answer: None,
        }
    }

    /// Test layer: removes a `/v2` path prefix before routing.
    struct StripV2;

    impl Middleware for StripV2 {
        fn handle<'a>(&'a self, mut request: Request, next: Next<'a>) -> BoxResponseFuture<'a> {
            if let Some(rest) = request.uri().path().strip_prefix("/v2")
                && let Ok(uri) = rest.parse::<Uri>()
            {
                request.set_uri(uri);
            }
            next.run(request)
        }
    }

    #[tokio::test]
    async fn a_layer_can_rewrite_the_path_the_route_table_sees() {
        let mut router = Router::new();
        router
            .route(Method::GET, "/items", |request: Request| async move {
                Response::new(StatusCode::OK, request.uri().path().to_owned())
            })
            .expect("route registers");
        router.layer(StripV2);

        let response = router.dispatch(request(Method::GET, "/v2/items")).await;

        assert_eq!(response.status, StatusCode::OK);
        assert_eq!(response.body(), &"/items");
    }

    #[tokio::test]
    async fn layers_run_in_registration_order_before_routing() {
        let mut router = Router::new();
        router
            .route(Method::DELETE, "/items", |request: Request| async move {
                let seen = request
                    .extensions()
                    .get::<&'static str>()
                    .copied()
                    .unwrap_or("none");
                Response::new(StatusCode::OK, seen)
            })
            .expect("route registers");
        router.layer(trace("outer")).layer(Trace {
            rewrite: Some(Method::DELETE),
            ..trace("inner")
        });

        let response = router.dispatch(request(Method::POST, "/items")).await;

        // The inner layer rewrote POST to DELETE before the route table ran.
        assert_eq!(response.status, StatusCode::OK);
        assert_eq!(response.body(), &"inner");
        let traced: Vec<_> = response
            .headers
            .get_all("x-trace")
            .iter()
            .map(|value| value.to_str().expect("ASCII"))
            .collect();
        assert_eq!(traced, ["inner", "outer"]);
        assert!(format!("{router:?}").contains("layers: 2"));
    }

    #[tokio::test]
    async fn layers_see_unrouted_requests_and_can_answer_directly() {
        let mut router = Router::new();
        router.layer(trace("outer"));
        let not_found = router.dispatch(request(Method::GET, "/missing")).await;
        assert_eq!(not_found.status, StatusCode::NOT_FOUND);
        assert_eq!(
            not_found.headers.get("x-trace").map(HeaderValue::as_bytes),
            Some(&b"outer"[..])
        );

        router.layer(Trace {
            answer: Some(StatusCode::FORBIDDEN),
            ..trace("guard")
        });
        let refused = router.dispatch(request(Method::GET, "/missing")).await;
        assert_eq!(refused.status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn request_form_decodes_bodies_and_reads_reserved_fields() {
        #[derive(Debug, serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Login {
            email: String,
            #[serde(default)]
            remember: bool,
        }
        let mut form = request(Method::POST, "/session");
        form.headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/x-www-form-urlencoded"),
        );
        form.body =
            Bytes::from_static(b"email=a%40example.com&remember=on&csrf_token=t&_method=PUT");

        assert!(form.has_form_body());
        let login: Login = form.form().expect("valid form");
        assert_eq!(login.email, "a@example.com");
        assert!(login.remember);
        assert_eq!(
            form.form_value(CSRF_FIELD_NAME).expect("valid"),
            Some("t".to_owned())
        );
        assert_eq!(
            form.form_value(METHOD_FIELD_NAME).expect("valid"),
            Some("PUT".to_owned())
        );

        let json = request(Method::POST, "/session");
        assert!(!json.has_form_body());
        assert_eq!(
            json.form::<Login>().expect_err("no form").kind(),
            FormErrorKind::UnsupportedMediaType
        );
    }

    #[tokio::test]
    async fn routes_to_registered_handler() {
        let mut router = Router::new();
        router
            .route(Method::GET, "/customers", |_| async {
                Response::new(StatusCode::OK, "customers")
            })
            .expect("route must register");

        let response = router.dispatch(request(Method::GET, "/customers")).await;

        assert_eq!(response.status, StatusCode::OK);
        assert_eq!(response.body(), &"customers");
    }

    #[tokio::test]
    async fn dynamic_route_captures_and_decodes_typed_path_parameters() {
        let mut router = Router::new();
        router
            .route(
                Method::GET,
                "/customers/{id}",
                |request: Request| async move {
                    let id: Uuid = request
                        .path_param_as("id")
                        .expect("route should provide a valid UUID capture");
                    Response::new(StatusCode::OK, id.to_string())
                },
            )
            .expect("dynamic route must register");

        let id = Uuid::now_v7();
        let response = router
            .dispatch(request(Method::GET, &format!("/customers/{id}")))
            .await;

        assert_eq!(response.status, StatusCode::OK);
        assert_eq!(response.body(), &id.to_string());
    }

    #[tokio::test]
    async fn dynamic_route_percent_decodes_utf8_without_form_decoding_plus() {
        let mut router = Router::new();
        router
            .route(
                Method::GET,
                "/products/{slug}",
                |request: Request| async move {
                    let slug: String = request.path_param_as("slug").expect("valid path capture");
                    Response::new(StatusCode::OK, slug)
                },
            )
            .expect("dynamic route must register");

        let response = router
            .dispatch(request(Method::GET, "/products/caf%C3%A9%2Btea"))
            .await;

        assert_eq!(response.body(), &"café+tea");
    }

    #[tokio::test]
    async fn static_routes_take_precedence_over_parameters() {
        let mut router = Router::new();
        router
            .route(
                Method::GET,
                "/customers/{id}",
                |request: Request| async move {
                    let id: String = request.path_param_as("id").expect("id capture");
                    Response::new(StatusCode::OK, format!("customer:{id}"))
                },
            )
            .expect("parameter route must register");
        router
            .route(Method::GET, "/customers/new", |_| async {
                Response::new(StatusCode::OK, "new-customer")
            })
            .expect("static route must register");

        let response = router
            .dispatch(request(Method::GET, "/customers/new"))
            .await;

        assert_eq!(response.body(), &"new-customer");
    }

    #[tokio::test]
    async fn dynamic_path_parameter_errors_are_distinguishable() {
        let mut router = Router::new();
        router
            .route(
                Method::GET,
                "/customers/{id}",
                |request: Request| async move {
                    let result = request.path_param_as::<u64>("id");
                    let status = match result {
                        Ok(_) => StatusCode::OK,
                        Err(PathParamError::Missing { .. }) => StatusCode::INTERNAL_SERVER_ERROR,
                        Err(PathParamError::Invalid { .. }) => StatusCode::BAD_REQUEST,
                    };
                    Response::empty(status)
                },
            )
            .expect("dynamic route must register");

        let invalid = router
            .dispatch(request(Method::GET, "/customers/not-a-number"))
            .await;
        let malformed_escape = router
            .dispatch(request(Method::GET, "/customers/%GG"))
            .await;

        assert_eq!(invalid.status, StatusCode::BAD_REQUEST);
        assert_eq!(malformed_escape.status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn encoded_slash_in_a_capture_is_a_bad_request() {
        let mut router = Router::new();
        router
            .route(Method::GET, "/files/{name}", |_| async {
                Response::empty(StatusCode::OK)
            })
            .expect("dynamic route must register");

        for path in ["/files/a%2Fb", "/files/a%2fb", "/files/%2F"] {
            let response = router.dispatch(request(Method::GET, path)).await;
            assert_eq!(response.status, StatusCode::BAD_REQUEST, "{path}");
        }
        let escaped_percent = router
            .dispatch(request(Method::GET, "/files/a%252Fb"))
            .await;
        assert_eq!(escaped_percent.status, StatusCode::OK);
    }

    #[tokio::test]
    async fn unmatched_and_empty_segments_are_not_found() {
        let mut router = Router::new();
        router
            .route(Method::GET, "/customers/{id}", |_| async {
                Response::empty(StatusCode::OK)
            })
            .expect("dynamic route must register");

        for path in [
            "/customers",
            "/customers/",
            "/customers/1/extra",
            "/other/1",
        ] {
            let response = router.dispatch(request(Method::GET, path)).await;
            assert_eq!(response.status, StatusCode::NOT_FOUND, "{path}");
        }
    }

    #[tokio::test]
    async fn allow_header_lists_registered_methods_sorted() {
        let mut router = Router::new();
        for method in [Method::PUT, Method::DELETE, Method::GET] {
            router
                .route(method, "/customers/{id}", |_| async {
                    Response::empty(StatusCode::OK)
                })
                .expect("dynamic route must register");
        }

        let response = router.dispatch(request(Method::POST, "/customers/1")).await;

        assert_eq!(response.status, StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(response.headers[header::ALLOW], "DELETE, GET, PUT");
    }

    #[tokio::test]
    async fn dynamic_route_method_mismatch_reports_allowed_methods() {
        let mut router = Router::new();
        router
            .route(Method::GET, "/customers/{id}", |_| async {
                Response::empty(StatusCode::OK)
            })
            .expect("dynamic route must register");

        let response = router
            .dispatch(request(Method::POST, "/customers/123"))
            .await;

        assert_eq!(response.status, StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(response.headers[header::ALLOW], "GET");
    }

    #[test]
    fn ambiguous_dynamic_routes_are_rejected() {
        let mut router = Router::new();
        router
            .route(Method::GET, "/customers/{id}", |_| async {
                Response::empty(StatusCode::OK)
            })
            .expect("first parameter route must register");

        let error = router
            .route(Method::GET, "/customers/{customer_id}", |_| async {
                Response::empty(StatusCode::OK)
            })
            .expect_err("equivalent parameter route must be rejected");

        assert!(matches!(error, RouteError::Ambiguous { .. }));
    }

    #[tokio::test]
    async fn distinguishes_unknown_path_from_wrong_method() {
        let mut router = Router::new();
        router
            .route(Method::GET, "/customers", |_| async {
                Response::empty(StatusCode::OK)
            })
            .expect("route must register");

        let wrong_method = router.dispatch(request(Method::POST, "/customers")).await;
        let missing = router.dispatch(request(Method::GET, "/missing")).await;

        assert_eq!(wrong_method.status, StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(missing.status, StatusCode::NOT_FOUND);
    }

    #[test]
    fn duplicate_routes_are_rejected() {
        let mut router = Router::new();
        router
            .route(Method::GET, "/health", |_| async {
                Response::empty(StatusCode::OK)
            })
            .expect("first route must register");
        let error = router
            .route(Method::GET, "/health", |_| async {
                Response::empty(StatusCode::OK)
            })
            .expect_err("duplicate route must fail");

        assert!(matches!(error, RouteError::Duplicate { .. }));
    }

    #[test]
    fn security_headers_are_applied_at_transport_boundary() {
        let outgoing = to_hyper_response(Response::empty(StatusCode::NO_CONTENT), Uuid::now_v7());

        assert_eq!(outgoing.headers()["x-content-type-options"], "nosniff");
        assert_eq!(outgoing.headers()["x-frame-options"], "DENY");
        assert!(outgoing.headers().contains_key("x-request-id"));
    }
    #[test]
    fn router_debug_lists_routes() {
        let mut router = Router::new();
        router
            .standard_health_routes()
            .expect("health routes must register");

        let debug = format!("{router:?}");

        assert!(debug.contains("GET /health/live"), "{debug}");
    }

    #[tokio::test]
    async fn health_routes_return_json() {
        let mut router = Router::new();
        router
            .standard_health_routes()
            .expect("health routes must register");

        for (path, body) in [
            ("/health", r#"{"status":"ok"}"#),
            ("/health/live", r#"{"status":"live"}"#),
            ("/health/ready", r#"{"status":"ready"}"#),
        ] {
            let response = router.dispatch(request(Method::GET, path)).await;

            assert_eq!(response.status, StatusCode::OK, "{path}");
            assert_eq!(response.body(), &body, "{path}");
            assert_eq!(
                response.headers[header::CONTENT_TYPE],
                "application/json",
                "{path}"
            );
        }
    }

    struct TestServer {
        addr: SocketAddr,
        shutdown: oneshot::Sender<()>,
        handle: JoinHandle<Result<(), ServerError>>,
    }

    impl TestServer {
        async fn start(config: ServerConfig, router: Router) -> Self {
            Self::start_server(Server::new(config, router)).await
        }

        async fn start_server(server: Server) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("test listener must bind");
            let addr = listener.local_addr().expect("listener has an address");
            let (shutdown, signal) = oneshot::channel::<()>();
            let handle = tokio::spawn(server.serve(listener, async {
                // A dropped sender also means shutdown.
                let _ = signal.await;
            }));
            Self {
                addr,
                shutdown,
                handle,
            }
        }

        async fn stop(self) {
            self.shutdown
                .send(())
                .expect("server must still be waiting for shutdown");
            tokio::time::timeout(CLIENT_TIMEOUT, self.handle)
                .await
                .expect("server must stop promptly")
                .expect("server task must not panic")
                .expect("server must shut down cleanly");
        }
    }

    fn test_config(request_timeout: Duration) -> ServerConfig {
        ServerConfig {
            bind: "127.0.0.1:0".parse().expect("valid address"),
            request_timeout,
            shutdown_grace: Duration::from_secs(30),
            max_body_bytes: 64,
            max_connections: 16,
            trusted_proxies: Vec::new(),
        }
    }

    fn test_router() -> Router {
        let mut router = Router::new();
        router
            .standard_health_routes()
            .expect("health routes must register");
        router
            .route(Method::POST, "/echo", |request: Request| async move {
                Response::new(StatusCode::OK, request.body().clone())
            })
            .expect("echo route must register");
        router
            .route(Method::GET, "/slow", |_| async {
                tokio::time::sleep(Duration::from_secs(60)).await;
                Response::empty(StatusCode::OK)
            })
            .expect("slow route must register");
        router
    }

    fn connect(addr: SocketAddr) -> StdTcpStream {
        let stream = StdTcpStream::connect(addr).expect("client must connect");
        stream
            .set_read_timeout(Some(CLIENT_TIMEOUT))
            .expect("read timeout must be settable");
        stream
            .set_write_timeout(Some(CLIENT_TIMEOUT))
            .expect("write timeout must be settable");
        stream
    }

    /// Writes raw bytes and reads until the server closes the connection.
    fn exchange_blocking(addr: SocketAddr, raw: &[u8]) -> String {
        let mut stream = connect(addr);
        stream.write_all(raw).expect("request must be written");
        let mut response = Vec::new();
        stream
            .read_to_end(&mut response)
            .expect("server must close the connection before the client timeout");
        String::from_utf8_lossy(&response).into_owned()
    }

    async fn exchange(addr: SocketAddr, raw: &'static str) -> String {
        tokio::task::spawn_blocking(move || exchange_blocking(addr, raw.as_bytes()))
            .await
            .expect("client task must not panic")
    }

    /// Reads from a keep-alive connection until `marker` has been received.
    fn read_until(stream: &mut StdTcpStream, marker: &str) -> String {
        let mut received = Vec::new();
        let mut buffer = [0_u8; 1024];
        while !String::from_utf8_lossy(&received).contains(marker) {
            let read = stream.read(&mut buffer).expect("response must arrive");
            assert!(read > 0, "connection closed before {marker:?} arrived");
            received.extend_from_slice(&buffer[..read]);
        }
        String::from_utf8_lossy(&received).into_owned()
    }

    /// Answers with the client address the first layer saw.
    struct SeenByFirstLayer;

    impl Middleware for SeenByFirstLayer {
        fn handle<'a>(&'a self, mut request: Request, next: Next<'a>) -> BoxResponseFuture<'a> {
            let seen = request.client_ip();
            request.extensions_mut().insert(seen);
            next.run(request)
        }
    }

    fn client_ip_router() -> Router {
        let mut router = Router::new();
        router.layer(SeenByFirstLayer);
        router
            .route(Method::GET, "/ip", |request: Request| async move {
                let seen = request.extensions().get::<IpAddr>().copied();
                Response::new(
                    StatusCode::OK,
                    format!("first-layer={seen:?} handler={}", request.client_ip()),
                )
            })
            .expect("ip route must register");
        router
    }

    #[tokio::test]
    async fn server_resolves_the_client_from_trusted_proxies_only_before_every_layer() {
        let raw = "GET /ip HTTP/1.1\r\nHost: localhost\r\nX-Forwarded-For: 1.2.3.4, 203.0.113.9\r\nX-Real-IP: 198.51.100.1\r\nConnection: close\r\n\r\n";

        // The test client connects from 127.0.0.1; trusting it believes the
        // header, and the first layer already sees the resolved client.
        let mut trusting = test_config(Duration::from_secs(10));
        trusting.trusted_proxies = vec!["127.0.0.0/8".parse().expect("valid network")];
        let server = TestServer::start(trusting, client_ip_router()).await;
        let response = exchange(server.addr, raw).await;
        assert!(
            response.contains("first-layer=Some(203.0.113.9) handler=203.0.113.9"),
            "{response}"
        );
        server.stop().await;

        // Negative: the default trusts nobody, so a client that connects
        // directly cannot choose its address with any forwarding header.
        let server =
            TestServer::start(test_config(Duration::from_secs(10)), client_ip_router()).await;
        let response = exchange(server.addr, raw).await;
        assert!(
            response.contains("first-layer=Some(127.0.0.1) handler=127.0.0.1"),
            "{response}"
        );
        server.stop().await;

        // Negative: a trusted network that does not contain the peer.
        let mut elsewhere = test_config(Duration::from_secs(10));
        elsewhere.trusted_proxies = vec!["10.0.0.0/8".parse().expect("valid network")];
        let server = TestServer::start(elsewhere, client_ip_router()).await;
        let response = exchange(server.addr, raw).await;
        assert!(response.contains("handler=127.0.0.1"), "{response}");
        server.stop().await;
    }

    #[tokio::test]
    async fn serve_answers_liveness_and_stops_on_shutdown() {
        let server = TestServer::start(test_config(Duration::from_secs(10)), test_router()).await;

        let response = exchange(
            server.addr,
            "GET /health/live HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        )
        .await;

        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
        assert!(
            response
                .to_ascii_lowercase()
                .contains("content-type: application/json\r\n"),
            "{response}"
        );
        assert!(response.ends_with(r#"{"status":"live"}"#), "{response}");
        server.stop().await;
    }

    #[tokio::test]
    async fn slow_handler_times_out_with_gateway_timeout() {
        let server =
            TestServer::start(test_config(Duration::from_millis(200)), test_router()).await;

        let response = exchange(
            server.addr,
            "GET /slow HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        )
        .await;

        assert!(response.starts_with("HTTP/1.1 504 "), "{response}");
        server.stop().await;
    }

    #[tokio::test]
    async fn request_timeout_covers_slow_body() {
        let server =
            TestServer::start(test_config(Duration::from_millis(200)), test_router()).await;

        // Declares ten body bytes but sends two, then stalls.
        let response = exchange(
            server.addr,
            "POST /echo HTTP/1.1\r\nHost: localhost\r\nContent-Length: 10\r\n\
             Connection: close\r\n\r\nab",
        )
        .await;

        assert!(response.starts_with("HTTP/1.1 504 "), "{response}");
        server.stop().await;
    }

    #[tokio::test]
    async fn slow_headers_close_the_connection() {
        let server =
            TestServer::start(test_config(Duration::from_millis(200)), test_router()).await;

        // The header block is never terminated.
        let response = exchange(server.addr, "GET /health HTTP/1.1\r\nHost: localhost\r\n").await;

        assert!(!response.contains(" 200 "), "{response}");
        server.stop().await;
    }

    #[tokio::test]
    async fn complete_body_is_delivered_to_handler() {
        let server = TestServer::start(test_config(Duration::from_secs(10)), test_router()).await;

        let response = exchange(
            server.addr,
            "POST /echo HTTP/1.1\r\nHost: localhost\r\nContent-Length: 5\r\n\
             Connection: close\r\n\r\nhello",
        )
        .await;

        assert!(response.starts_with("HTTP/1.1 200 "), "{response}");
        assert!(response.ends_with("\r\n\r\nhello"), "{response}");
        server.stop().await;
    }

    #[tokio::test]
    async fn oversized_bodies_are_rejected() {
        let server = TestServer::start(test_config(Duration::from_secs(10)), test_router()).await;

        let declared = exchange(
            server.addr,
            "POST /echo HTTP/1.1\r\nHost: localhost\r\nContent-Length: 1000\r\n\
             Connection: close\r\n\r\n",
        )
        .await;
        assert!(declared.starts_with("HTTP/1.1 413 "), "{declared}");

        // An 80-byte chunk exceeds the 64-byte limit without a Content-Length.
        let chunked = exchange(
            server.addr,
            "POST /echo HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\
             Connection: close\r\n\r\n50\r\n\
             01234567890123456789012345678901234567890123456789012345678901234567890123456789\
             \r\n0\r\n\r\n",
        )
        .await;
        assert!(chunked.starts_with("HTTP/1.1 413 "), "{chunked}");
        server.stop().await;
    }

    #[tokio::test]
    async fn connection_limit_applies_backpressure() {
        let mut config = test_config(Duration::from_secs(10));
        config.max_connections = 1;
        let server = TestServer::start(config, test_router()).await;
        let addr = server.addr;

        tokio::task::spawn_blocking(move || {
            // The first client keeps its connection (and the only permit) open.
            let mut holder = connect(addr);
            holder
                .write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\n\r\n")
                .expect("request must be written");
            let first = read_until(&mut holder, r#"{"status":"ok"}"#);
            assert!(first.starts_with("HTTP/1.1 200 "), "{first}");

            // The second client sits in the listen backlog and gets no answer.
            let mut waiting = connect(addr);
            waiting
                .write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                .expect("request must be written");
            waiting
                .set_read_timeout(Some(Duration::from_millis(300)))
                .expect("read timeout must be settable");
            let mut probe = [0_u8; 1];
            let error = waiting
                .read(&mut probe)
                .expect_err("saturated server must not answer");
            assert!(
                matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut),
                "{error}"
            );

            // Releasing the first connection frees the permit.
            drop(holder);
            waiting
                .set_read_timeout(Some(CLIENT_TIMEOUT))
                .expect("read timeout must be settable");
            let mut second = Vec::new();
            waiting
                .read_to_end(&mut second)
                .expect("second client must be served once a permit is free");
            let second = String::from_utf8_lossy(&second);
            assert!(second.starts_with("HTTP/1.1 200 "), "{second}");
        })
        .await
        .expect("client task must not panic");

        server.stop().await;
    }

    #[tokio::test]
    async fn shutdown_interrupts_saturated_server_and_closes_idle_connections() {
        let mut config = test_config(Duration::from_secs(10));
        config.max_connections = 1;
        let server = TestServer::start(config, test_router()).await;
        let addr = server.addr;

        // Hold the only permit with an idle keep-alive connection.
        let holder = tokio::task::spawn_blocking(move || {
            let mut holder = connect(addr);
            holder
                .write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\n\r\n")
                .expect("request must be written");
            read_until(&mut holder, r#"{"status":"ok"}"#);
            holder
        })
        .await
        .expect("client task must not panic");

        // Grace is 30 s; `stop` requires completion well before that.
        server.stop().await;
        drop(holder);
    }

    /// A streamed body of `count` numbered lines, one every `pause`.
    fn counting(count: u8, pause: Duration) -> Response {
        Response::stream(StatusCode::OK, 0_u8, move |sent| async move {
            if sent == count {
                return None;
            }
            tokio::time::sleep(pause).await;
            Some((Bytes::from(format!("line {sent}\n")), sent + 1))
        })
    }

    #[tokio::test]
    async fn streamed_bodies_are_read_chunk_by_chunk_in_process() {
        let mut buffered = Response::new(StatusCode::OK, "whole");
        assert!(!buffered.is_stream());
        assert_eq!(buffered.next_chunk().await.as_deref(), Some(&b"whole"[..]));
        assert_eq!(buffered.next_chunk().await, None);

        let mut streamed = counting(3, Duration::ZERO);
        assert!(streamed.is_stream());
        assert!(streamed.body().is_empty());
        assert!(format!("{streamed:?}").contains("<stream>"));
        let mut chunks = Vec::new();
        while let Some(chunk) = streamed.next_chunk().await {
            chunks.push(String::from_utf8_lossy(&chunk).into_owned());
        }
        assert_eq!(chunks, ["line 0\n", "line 1\n", "line 2\n"]);
        assert_eq!(streamed.next_chunk().await, None);
    }

    #[tokio::test]
    async fn empty_chunks_are_skipped() {
        let mut response = Response::stream(StatusCode::OK, 0_u8, |step| async move {
            match step {
                0 | 2 => Some((Bytes::new(), step + 1)),
                1 => Some((Bytes::from_static(b"x"), step + 1)),
                _ => None,
            }
        });

        assert_eq!(response.next_chunk().await.as_deref(), Some(&b"x"[..]));
        assert_eq!(response.next_chunk().await, None);
    }

    #[test]
    fn a_step_that_only_returns_empty_chunks_does_not_starve_other_tasks() {
        // On a single-threaded runtime, a body that spun without yielding
        // would keep the other task from ever running. The verdict comes
        // through a channel so that a regression fails instead of hanging.
        let (verdict, outcome) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .expect("runtime must build");
            runtime.block_on(async {
                let mut empty =
                    Response::stream(StatusCode::OK, (), |()| async { Some((Bytes::new(), ())) });
                let reader = tokio::spawn(async move { empty.next_chunk().await });
                let other = tokio::spawn(async { "ran" });
                let ran = other.await.expect("the other task must not panic");
                reader.abort();
                let _ = verdict.send(ran);
            });
        });

        assert_eq!(
            outcome.recv_timeout(CLIENT_TIMEOUT),
            Ok("ran"),
            "an always-empty body must yield to other tasks"
        );
    }

    #[tokio::test]
    async fn streamed_bodies_are_sent_with_chunked_encoding() {
        let mut router = Router::new();
        router
            .route(Method::GET, "/count", |_| async {
                counting(3, Duration::from_millis(20))
            })
            .expect("route must register");
        let server = TestServer::start(test_config(Duration::from_secs(10)), router).await;

        let response = exchange(
            server.addr,
            "GET /count HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        )
        .await;

        assert!(response.starts_with("HTTP/1.1 200 "), "{response}");
        assert!(
            response
                .to_ascii_lowercase()
                .contains("transfer-encoding: chunked"),
            "{response}"
        );
        for line in ["line 0", "line 1", "line 2"] {
            assert!(response.contains(line), "{response}");
        }
        server.stop().await;
    }

    /// Records whether the value was dropped.
    struct DropFlag(Arc<std::sync::atomic::AtomicBool>);

    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// A router whose `GET /flood` streams an endless body far larger than
    /// the socket buffers; `dropped` turns true when the body is dropped.
    fn flood(dropped: &Arc<std::sync::atomic::AtomicBool>) -> Router {
        let mut router = Router::new();
        let flag = Arc::clone(dropped);
        router
            .route(Method::GET, "/flood", move |_| {
                let guard = DropFlag(Arc::clone(&flag));
                async move {
                    Response::stream(StatusCode::OK, guard, |guard| async move {
                        Some((Bytes::from(vec![b'x'; 64 * 1024]), guard))
                    })
                }
            })
            .expect("route must register");
        router
    }

    #[test]
    fn the_write_stall_guard_is_the_request_timeout_but_at_least_30_seconds() {
        assert_eq!(MIN_WRITE_STALL_TIMEOUT, Duration::from_secs(30));
        for (request_timeout, guard) in [(1, 30), (29, 30), (30, 30), (31, 31), (120, 120)] {
            assert_eq!(
                write_stall_timeout(
                    Duration::from_secs(request_timeout),
                    MIN_WRITE_STALL_TIMEOUT
                ),
                Duration::from_secs(guard),
                "{request_timeout}"
            );
        }
    }

    #[tokio::test]
    async fn a_short_request_timeout_does_not_disconnect_a_slow_reader() {
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        // The request timeout is far shorter than the pause below, but the
        // stalled-write guard keeps its 30 second floor.
        let server =
            TestServer::start(test_config(Duration::from_millis(100)), flood(&dropped)).await;
        let addr = server.addr;

        let (paused, received) = tokio::task::spawn_blocking(move || {
            let mut stream = connect(addr);
            stream
                .write_all(b"GET /flood HTTP/1.1\r\nHost: localhost\r\n\r\n")
                .expect("request must be written");
            // Not reading: the socket buffers fill and the server's writes
            // stall for ten request timeouts.
            std::thread::sleep(Duration::from_secs(1));
            let mut buffer = vec![0_u8; 64 * 1024];
            let mut received = 0;
            while received < 4 << 20 {
                let read = stream.read(&mut buffer).expect("the stream must stay open");
                assert_ne!(read, 0, "the server must not end the stream");
                received += read;
            }
            (stream, received)
        })
        .await
        .expect("client task must not panic");

        assert!(received >= 4 << 20);
        assert!(
            !dropped.load(std::sync::atomic::Ordering::SeqCst),
            "a slow but live reader must keep its stream"
        );
        drop(paused);
        server.stop().await;
    }

    #[tokio::test]
    async fn a_client_that_stops_reading_a_stream_is_disconnected() {
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut server = Server::new(test_config(Duration::from_millis(300)), flood(&dropped));
        // Without the 30 second floor, so the test does not wait for it.
        server.min_write_stall = Duration::ZERO;
        let server = TestServer::start_server(server).await;
        let addr = server.addr;

        // The client sends its request and never reads the response.
        let client = tokio::task::spawn_blocking(move || {
            let mut stream = connect(addr);
            stream
                .write_all(b"GET /flood HTTP/1.1\r\nHost: localhost\r\n\r\n")
                .expect("request must be written");
            stream
        })
        .await
        .expect("client task must not panic");

        let deadline = tokio::time::Instant::now() + CLIENT_TIMEOUT;
        while !dropped.load(std::sync::atomic::Ordering::SeqCst) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "a stalled stream must be dropped"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        drop(client);
        server.stop().await;
    }
}
