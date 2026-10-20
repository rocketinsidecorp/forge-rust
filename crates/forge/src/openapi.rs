//! OpenAPI 3.1 from typed routes (ADR 0039).
//!
//! A route keeps its ordinary handler; [`describe`] attaches what it accepts
//! and answers, and [`typed`] goes further: it decodes the request, calls the
//! function and writes its result, taking the description from the
//! function's own types so the two cannot disagree.
//!
//! ```
//! use forge::core::ForgeError;
//! use forge::http::{Method, Request, Router, StatusCode};
//! use forge::openapi::{NoBody, OpenApiSchema, Operation, typed};
//!
//! #[derive(serde::Serialize, OpenApiSchema)]
//! struct Status {
//!     service: String,
//!     status: String,
//! }
//!
//! let mut router = Router::new();
//! router.route(
//!     Method::GET,
//!     "/v1/status",
//!     typed(
//!         StatusCode::OK,
//!         Operation::new().summary("Service status").tag("status"),
//!         |_request: Request, NoBody| async {
//!             Ok::<_, ForgeError>(Status { service: "shop".into(), status: "ok".into() })
//!         },
//!     ),
//! )?;
//! let document = router.openapi(&forge::openapi::Info::new("shop", "0.1.0"))?;
//! assert_eq!(document.paths(), ["/v1/status"]);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! `GET /openapi.json` is served in development and test (or with
//! `FORGE_OPENAPI_ENABLED=true`), and the application binary prints the
//! document with `openapi [--output FILE | --check FILE]`.

use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;

use forge_core::ForgeError;
pub use forge_http::{
    CheckResponses, ContractFailure, Described, HealthReport, HealthStatus, OPENAPI_PATH, describe,
};
use forge_http::{Handler, Request, StatusCode};
pub use forge_openapi::*;
use serde::Serialize;
use serde::de::DeserializeOwned;

/// A JSON request body of type `T`, decoded with [`crate::web::decode_json`]
/// (a body that does not decode is `422 invalid_json`; one over
/// `FORGE_MAX_BODY_BYTES` is refused with `413` before the handler runs).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Json<T>(pub T);

/// The request type of a [`typed`] handler that reads no body.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct NoBody;

/// What a [`typed`] handler receives besides the request: [`Json`] or
/// [`NoBody`].
pub trait RequestBody: Sized + Send + 'static {
    /// Decodes the body, or the error the caller receives.
    fn decode(request: &Request) -> Result<Self, ForgeError>;

    /// Adds the body to the operation's description.
    fn describe(operation: Operation) -> Operation;
}

impl<T: DeserializeOwned + OpenApiSchema + Send + 'static> RequestBody for Json<T> {
    fn decode(request: &Request) -> Result<Self, ForgeError> {
        crate::web::decode_json(request).map(Json)
    }

    fn describe(operation: Operation) -> Operation {
        operation.request::<T>()
    }
}

impl RequestBody for NoBody {
    fn decode(_request: &Request) -> Result<Self, ForgeError> {
        Ok(Self)
    }

    fn describe(operation: Operation) -> Operation {
        operation
    }
}

/// A JSON handler described by its own types: `handler` receives the
/// request and its decoded [`RequestBody`], and its `Ok` value is written
/// as JSON with `status`, its error with [`crate::web::error`] (an
/// `ErrorReport` with its category's status). The operation's request and
/// its `status` response come from the types, replacing any declared there.
/// List the error statuses the handler answers with
/// [`Operation::errors`]. A `status` that is not a 2xx with a body
/// (`204`, `205`) makes the description invalid, which stops the document
/// from building.
pub fn typed<B, R, F, Fut>(
    status: StatusCode,
    operation: Operation,
    handler: F,
) -> Described<Typed<B, R, F>>
where
    B: RequestBody,
    R: Serialize + OpenApiSchema + Send + 'static,
    F: Fn(Request, B) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<R, ForgeError>> + Send + 'static,
{
    let code = status.as_u16();
    let mut operation = B::describe(operation).with_success(Response::json::<R>(code));
    if !status.is_success() || matches!(code, 204 | 205) {
        operation = operation.invalid(format!(
            "a typed handler needs a 2xx status with a body, not {code}"
        ));
    }
    describe(
        Typed {
            status,
            handler,
            types: PhantomData,
        },
        operation,
    )
}

/// The handler [`typed`] builds.
pub struct Typed<B, R, F> {
    status: StatusCode,
    handler: F,
    types: PhantomData<fn(B) -> R>,
}

impl<B, R, F> std::fmt::Debug for Typed<B, R, F> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Typed")
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

impl<B, R, F, Fut> Handler for Typed<B, R, F>
where
    B: RequestBody,
    R: Serialize + OpenApiSchema + Send + 'static,
    F: Fn(Request, B) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<R, ForgeError>> + Send + 'static,
{
    fn call(
        &self,
        request: Request,
    ) -> Pin<Box<dyn Future<Output = forge_http::Response> + Send + 'static>> {
        let status = self.status;
        let body = match B::decode(&request) {
            Ok(body) => body,
            Err(failure) => {
                let response = crate::web::error(&failure);
                return Box::pin(async move { response });
            }
        };
        let future = (self.handler)(request, body);
        Box::pin(async move {
            match future.await {
                Ok(value) => crate::web::json(status, &value),
                Err(failure) => crate::web::error(&failure),
            }
        })
    }
}
