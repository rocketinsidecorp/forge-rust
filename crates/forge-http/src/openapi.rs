//! Route descriptions on handlers, the served document and contract checks
//! (ADR 0039). The document model is `forge-openapi`; this module is its
//! HTTP side.

use std::sync::Arc;

use bytes::Bytes;
use forge_openapi::{
    ContractError, Document, Field, Info, ObjectSchema, OpenApiSchema, Operation, Schema,
    SchemaGenerator,
};

use crate::{
    BoxHandlerFuture, BoxResponseFuture, Handler, HeaderValue, MatchedRoute, Middleware, Next,
    Request, Response, Router, StatusCode, header,
};

/// Where an application serves its OpenAPI document when
/// `FORGE_OPENAPI_ENABLED` allows it.
pub const OPENAPI_PATH: &str = "/openapi.json";

/// A handler that carries its route's description; see [`describe`].
pub struct Described<H> {
    handler: H,
    operation: Operation,
}

impl<H> std::fmt::Debug for Described<H> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Described")
            .field("operation", &self.operation)
            .finish_non_exhaustive()
    }
}

/// Returns `handler`, unchanged in behavior, carrying `operation`. Register
/// the described handler itself (after any guard of the route), so the
/// router sees the description:
///
/// ```
/// use forge_http::{Method, Response, Router, StatusCode, describe};
/// use forge_openapi::Operation;
///
/// let mut router = Router::new();
/// router.route(
///     Method::DELETE,
///     "/v1/posts/{id}",
///     describe(
///         |_| async { Response::empty(StatusCode::NO_CONTENT) },
///         Operation::new().summary("Delete a post").response(forge_openapi::Response::empty(204)),
///     ),
/// )?;
/// # Ok::<(), forge_http::RouteError>(())
/// ```
pub fn describe<H: Handler>(handler: H, operation: Operation) -> Described<H> {
    Described { handler, operation }
}

impl<H: Handler> Handler for Described<H> {
    fn call(&self, request: Request) -> BoxHandlerFuture {
        self.handler.call(request)
    }

    fn operation(&self) -> Option<&Operation> {
        Some(&self.operation)
    }
}

impl Router {
    /// Every registered route with its description, for
    /// [`Document::build`].
    #[must_use]
    pub fn openapi_routes(&self) -> Vec<forge_openapi::Route> {
        self.routes
            .iter()
            .map(|route| forge_openapi::Route {
                method: route.method.as_str().to_owned(),
                path: route.path.clone(),
                operation: route.operation.clone(),
            })
            .collect()
    }

    /// The OpenAPI document of the routes registered so far.
    pub fn openapi(&self, info: &Info) -> Result<Document, forge_openapi::BuildError> {
        Document::build(info, &self.openapi_routes())
    }

    /// Whether `method path` (the pattern as registered) is a route.
    #[must_use]
    pub fn has_route(&self, method: &crate::Method, path: &str) -> bool {
        self.routes
            .iter()
            .any(|route| route.method == *method && route.path == path)
    }

    /// Serves `document` at `GET /openapi.json` ([`OPENAPI_PATH`]). The
    /// route itself is not in the document.
    pub fn serve_openapi(&mut self, document: &Document) -> Result<&mut Self, crate::RouteError> {
        let body = Bytes::copy_from_slice(document.json());
        self.route(crate::Method::GET, OPENAPI_PATH, move |_| {
            let body = body.clone();
            async move {
                let mut response = Response::new(StatusCode::OK, body);
                response.headers_mut().insert(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("application/json"),
                );
                response
                    .headers_mut()
                    .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
                response
            }
        })
    }
}

/// What a failed contract check reports: the request and why its response
/// does not match the document.
#[derive(Debug)]
pub struct ContractFailure {
    /// The request method.
    pub method: crate::Method,
    /// The request path.
    pub path: String,
    /// Why the response does not match.
    pub error: ContractError,
}

impl std::fmt::Display for ContractFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{} {}: {}", self.method, self.path, self.error)
    }
}

type Report = dyn Fn(&ContractFailure) + Send + Sync;

/// A request layer for tests: every response of a documented route is
/// checked against the document ([`Document::validate_route_response`])
/// and each mismatch is passed to `report`; the response is returned
/// unchanged. Requests no documented route serves are not checked.
/// Streamed responses are not checked. Do not use it in production.
///
/// ```
/// # fn routes() -> Result<forge_http::Router, Box<dyn std::error::Error>> {
/// let mut router = forge_http::Router::new();
/// let document = router.openapi(&forge_openapi::Info::new("app", "0.1.0"))?;
/// router.layer(forge_http::CheckResponses::new(document, |failure| panic!("{failure}")));
/// // every request dispatched below is now also a contract test
/// # Ok(router)
/// # }
/// ```
#[derive(Clone)]
pub struct CheckResponses {
    document: Arc<Document>,
    report: Arc<Report>,
}

impl std::fmt::Debug for CheckResponses {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CheckResponses")
            .finish_non_exhaustive()
    }
}

impl CheckResponses {
    /// Checks responses against `document`; `report` receives every
    /// mismatch (a test usually panics with it).
    pub fn new(
        document: Document,
        report: impl Fn(&ContractFailure) + Send + Sync + 'static,
    ) -> Self {
        Self {
            document: Arc::new(document),
            report: Arc::new(report),
        }
    }
}

impl Middleware for CheckResponses {
    fn handle<'a>(&'a self, mut request: Request, next: Next<'a>) -> BoxResponseFuture<'a> {
        let method = request.method().clone();
        let path = request.uri().path().to_owned();
        let matched = match request.extensions().get::<MatchedRoute>() {
            Some(matched) => matched.clone(),
            None => {
                let matched = MatchedRoute::default();
                request.extensions_mut().insert(matched.clone());
                matched
            }
        };
        Box::pin(async move {
            let response = next.run(request).await;
            if response.is_stream() {
                return response;
            }
            let Some(route) = matched.0.get() else {
                return response;
            };
            let content_type = response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok());
            let checked = self.document.validate_route_response(
                method.as_str(),
                route,
                response.status().as_u16(),
                content_type,
                response.body(),
            );
            match checked {
                Ok(()) | Err(ContractError::Undocumented { .. }) => {}
                Err(error) => (self.report)(&ContractFailure {
                    method,
                    path,
                    error,
                }),
            }
            response
        })
    }
}

/// The body of `GET /health`, `/health/live` and a passing
/// `/health/ready`: `{"status": "ok" | "live" | "ready"}`.
pub struct HealthStatus;

impl OpenApiSchema for HealthStatus {
    fn schema(generator: &mut SchemaGenerator) -> Schema {
        generator.component::<Self>(Some("HealthStatus"), |generator| {
            let mut object = ObjectSchema::new();
            object.field::<str>(
                generator,
                "status",
                Field::new().doc("ok, live or ready.").example("ok"),
            );
            object.into_schema()
        })
    }
}

/// The body of a failing `GET /health/ready`:
/// `{"status": "unavailable", "checks": {"database": "unavailable"}}`.
pub struct HealthReport;

impl OpenApiSchema for HealthReport {
    fn schema(generator: &mut SchemaGenerator) -> Schema {
        generator.component::<Self>(Some("HealthReport"), |generator| {
            let mut object = ObjectSchema::new();
            object
                .field::<str>(
                    generator,
                    "status",
                    Field::new().doc("unavailable").example("unavailable"),
                )
                .field::<std::collections::BTreeMap<String, String>>(
                    generator,
                    "checks",
                    Field::new().doc("Each failed dependency and its state."),
                );
            object.into_schema()
        })
    }
}

/// The description of the health route `path` (`/health`, `/health/live` or
/// `/health/ready`); `checked` readiness also answers `503` with a
/// [`HealthReport`].
#[must_use]
pub fn health_operation(path: &str, checked: bool) -> Operation {
    let (id, summary) = match path {
        "/health/live" => ("healthLive", "Liveness: the process is serving requests"),
        "/health/ready" => ("healthReady", "Readiness: every registered check passes"),
        _ => ("health", "Health: the process is serving requests"),
    };
    let operation = Operation::new()
        .id(id)
        .summary(summary)
        .tag("health")
        .json::<HealthStatus>(200);
    if checked && path == "/health/ready" {
        operation.response(
            forge_openapi::Response::json::<HealthReport>(503).description("A check failed"),
        )
    } else {
        operation
    }
}
