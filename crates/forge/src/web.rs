//! Helpers that keep HTTP handlers thin: decode a request, call a use case,
//! map the result.
//!
//! ```rust
//! use forge::core::{ErrorCategory, ForgeError};
//! use forge::http::StatusCode;
//!
//! let response = forge::web::error(&ForgeError::new(
//!     ErrorCategory::NotFound,
//!     "product_not_found",
//!     "product not found",
//! ));
//! assert_eq!(response.status(), StatusCode::NOT_FOUND);
//! ```

use std::{error::Error as StdError, str::FromStr};

use forge_core::{ErrorCategory, ForgeError};
use forge_http::{JsonBodyError, PathParamError, Request, Response, StatusCode};
use forge_openapi::OpenApiSchema;
use serde::{Serialize, de::DeserializeOwned};
use tracing::{error, warn};

/// Renders an Askama view as an HTML response (feature `views`); see
/// `forge::views::html`. `html_with_status` answers another status, such as
/// `422` for a form shown again with errors.
#[cfg(feature = "views")]
pub use forge_views::{html, html_with_status};

/// Serializes `value` as a JSON response. Serialization failures (a bug in a
/// `Serialize` implementation) become an empty `500` and are logged.
#[must_use]
pub fn json(status: StatusCode, value: &impl Serialize) -> Response {
    Response::json(status, value).unwrap_or_else(|failure| {
        error!(error = %failure, "response serialization failed");
        Response::empty(StatusCode::INTERNAL_SERVER_ERROR)
    })
}

/// Renders a [`ForgeError`] as its caller-safe JSON report with the status of
/// its category. Internal sources never reach the client: server-side errors
/// are logged with their full cause chain instead.
#[must_use]
pub fn error(failure: &ForgeError) -> Response {
    let status = status_of(failure);
    if status.is_server_error() {
        let chain = cause_chain(failure);
        match failure.category() {
            ErrorCategory::Infrastructure => {
                warn!(code = failure.code(), error = %chain, "dependency failure");
            }
            _ => error!(code = failure.code(), error = %chain, "request failed"),
        }
    }
    json(status, &failure.report())
}

/// Logs a failure that does not change the response, with its cause chain,
/// for example an audit write or a delivery that must not alter what the
/// visitor sees. Infrastructure failures are warnings, others errors.
/// Never pass a failure whose message or sources contain a secret.
pub fn report(failure: &ForgeError) {
    let chain = cause_chain(failure);
    match failure.category() {
        ErrorCategory::Infrastructure => {
            warn!(code = failure.code(), error = %chain, "dependency failure");
        }
        _ => error!(code = failure.code(), error = %chain, "operation failed"),
    }
}

/// Runs `work` and returns its output no sooner than `minimum` after the
/// call, so a response's timing does not tell which branch it took (for
/// example whether an account exists). It never shortens `work`.
pub async fn at_least<F: std::future::Future>(minimum: std::time::Duration, work: F) -> F::Output {
    let started = tokio::time::Instant::now();
    let output = work.await;
    tokio::time::sleep_until(started + minimum).await;
    output
}

/// The `code` of the error [`decode_json`] returns for a request whose
/// `Content-Type` is not `application/json`; [`error`] answers it with
/// `415`.
pub const UNSUPPORTED_MEDIA_TYPE: &str = "unsupported_media_type";

/// The status [`error`] answers: the category's, except `415` for the one
/// validation failure that is about the media type and not the content.
fn status_of(failure: &ForgeError) -> StatusCode {
    if failure.category() == ErrorCategory::Validation && failure.code() == UNSUPPORTED_MEDIA_TYPE {
        return StatusCode::UNSUPPORTED_MEDIA_TYPE;
    }
    StatusCode::from_u16(failure.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
}

/// Decodes a JSON request body.
///
/// A request that does not declare `Content-Type: application/json`
/// (case-insensitive, an optional `charset` must be UTF-8) is refused with a
/// `Validation` error with code [`UNSUPPORTED_MEDIA_TYPE`], which [`error`]
/// answers as `415`. Malformed input is a `Validation` error with code
/// `invalid_json` (the decoder's detail is not exposed). Members `T` does not
/// have are ignored unless it uses `#[serde(deny_unknown_fields)]`;
/// [`decode_json_strict`] refuses them for a type with an OpenAPI
/// description.
pub fn decode_json<T: DeserializeOwned>(request: &Request) -> Result<T, ForgeError> {
    request.json_body().map_err(json_failure)
}

/// [`decode_json`] for a type with an OpenAPI description (what
/// [`crate::openapi::Json`] uses): a member the description does not list is
/// refused with `invalid_json` and the member's path (`address.street`,
/// truncated to 64 characters) in the `member` metadata, so decoding accepts
/// what the published document accepts (`additionalProperties: false`). A
/// `skip_serializing` member and a `#[serde(alias)]` are accepted although
/// the document does not list them. A type whose description cannot be built
/// is refused with a `500` (`json_description_invalid`): nothing can be
/// checked, so nothing is let through.
pub fn decode_json_strict<T: DeserializeOwned + OpenApiSchema + 'static>(
    request: &Request,
) -> Result<T, ForgeError> {
    request.json_body_strict().map_err(json_failure)
}

fn json_failure(failure: JsonBodyError) -> ForgeError {
    match failure {
        JsonBodyError::UnsupportedMediaType => ForgeError::new(
            ErrorCategory::Validation,
            UNSUPPORTED_MEDIA_TYPE,
            "request body must be application/json",
        ),
        JsonBodyError::UnknownMember { member } => ForgeError::new(
            ErrorCategory::Validation,
            "invalid_json",
            "request body has a member this endpoint does not accept",
        )
        .with_metadata("member", member.chars().take(64).collect::<String>()),
        // A request type whose description cannot be built is the
        // application's bug: fail closed, with the reason in the logs only.
        failure @ JsonBodyError::Description(_) => ForgeError::new(
            ErrorCategory::Internal,
            "json_description_invalid",
            "the description of this endpoint's request is invalid",
        )
        .with_source(failure),
        other => ForgeError::new(
            ErrorCategory::Validation,
            "invalid_json",
            "request body is not valid JSON for this endpoint",
        )
        .with_source(other),
    }
}

/// Decodes the query string into `T` (see [`Request::query`] for the strict
/// `application/x-www-form-urlencoded` rules and supported field types).
///
/// Every failure is a `Validation` error with code `invalid_query` and a
/// caller-safe message; when it concerns one parameter (malformed,
/// repeated, unknown, missing or invalid) its name, truncated to 64
/// characters, is in the `parameter` metadata. Unknown parameters are only
/// rejected when `T` uses `#[serde(deny_unknown_fields)]`; decode into `()`
/// to accept no query at all.
///
/// ```rust
/// use forge::http::{Method, Request, Uri, HeaderMap};
/// use bytes::Bytes;
/// use uuid::Uuid;
///
/// #[derive(serde::Deserialize)]
/// #[serde(deny_unknown_fields)]
/// struct Paging {
///     limit: Option<u16>,
/// }
///
/// let request = Request::new_for_test(
///     Method::GET,
///     Uri::from_static("/products?limit=25"),
///     HeaderMap::new(),
///     Bytes::new(),
///     "127.0.0.1:4000".parse().unwrap(),
///     Uuid::now_v7(),
/// );
/// let paging: Paging = forge::web::decode_query(&request)?;
/// assert_eq!(paging.limit, Some(25));
/// # Ok::<(), forge::core::ForgeError>(())
/// ```
pub fn decode_query<T: DeserializeOwned>(request: &Request) -> Result<T, ForgeError> {
    request.query().map_err(|failure| {
        let mut error = ForgeError::new(
            ErrorCategory::Validation,
            "invalid_query",
            failure.kind().message(),
        );
        if let Some(parameter) = failure.parameter() {
            error = error.with_metadata("parameter", parameter.to_owned());
        }
        error.with_source(failure)
    })
}

/// Decodes an HTML form body (`application/x-www-form-urlencoded`) into `T`
/// (see [`Request::form`] for the strict grammar, checkbox booleans, blank
/// optional values and the skipped `csrf_token`/`_method` fields).
///
/// Every failure is a `Validation` error (`422`) with code `invalid_form`
/// and a caller-safe message; when it concerns one field its name, truncated
/// to 64 characters, is in the `field` metadata. A body that is not
/// form-encoded is also `invalid_form`. Handlers that render the form again
/// use [`Request::form`] directly and turn the `FormError` into field errors.
///
/// ```rust
/// use bytes::Bytes;
/// use forge::http::{HeaderMap, HeaderValue, Method, Request, Uri, header};
/// use uuid::Uuid;
///
/// #[derive(serde::Deserialize)]
/// #[serde(deny_unknown_fields)]
/// struct NewProduct {
///     name: String,
///     #[serde(default)]
///     published: bool,
/// }
///
/// let mut headers = HeaderMap::new();
/// headers.insert(
///     header::CONTENT_TYPE,
///     HeaderValue::from_static("application/x-www-form-urlencoded"),
/// );
/// let request = Request::new_for_test(
///     Method::POST,
///     Uri::from_static("/products"),
///     headers,
///     Bytes::from_static(b"name=Desk&published=on&csrf_token=ignored"),
///     "127.0.0.1:4000".parse().unwrap(),
///     Uuid::now_v7(),
/// );
/// let product: NewProduct = forge::web::decode_form(&request)?;
/// assert_eq!(product.name, "Desk");
/// assert!(product.published);
/// # Ok::<(), forge::core::ForgeError>(())
/// ```
pub fn decode_form<T: DeserializeOwned>(request: &Request) -> Result<T, ForgeError> {
    request.form().map_err(|failure| {
        let mut error = ForgeError::new(
            ErrorCategory::Validation,
            "invalid_form",
            failure.kind().message(),
        );
        if let Some(field) = failure.field() {
            error = error.with_metadata("field", field.to_owned());
        }
        error.with_source(failure)
    })
}

/// Parses the path parameter `name` (see [`Request::path_param_as`]),
/// mapping an absent or unparsable value to a `Validation` error with code
/// `invalid_path_parameter` and the name in the `parameter` metadata. The
/// value itself is never echoed; the parser's error is kept as the source.
pub fn path_param<T>(request: &Request, name: &str) -> Result<T, ForgeError>
where
    T: FromStr,
    T::Err: Into<Box<dyn StdError + Send + Sync>>,
{
    request.path_param_as(name).map_err(|failure| {
        let message = match failure {
            PathParamError::Missing { .. } => "missing path parameter",
            PathParamError::Invalid { .. } => "path parameter has an invalid value",
        };
        ForgeError::new(ErrorCategory::Validation, "invalid_path_parameter", message)
            .with_metadata("parameter", failure.name().to_owned())
            .with_source(failure)
    })
}

fn cause_chain(failure: &ForgeError) -> String {
    let mut chain = failure.to_string();
    let mut source = std::error::Error::source(failure);
    while let Some(cause) = source {
        chain.push_str(": ");
        chain.push_str(&cause.to_string());
        source = cause.source();
    }
    chain
}

/// The JSON refusal of a route that authenticates an API client's bearer
/// token (`forge::tenancy::resolve_bearer`, `BearerSession::authorize`),
/// with Forge-Go's codes: `401 credentials_required` without an
/// `Authorization` header, `401 credentials_invalid` for a malformed,
/// unknown, expired, idle or revoked token (both with `WWW-Authenticate:
/// Bearer realm="forge"`), `403 permission_denied`, and `503
/// authentication_unavailable` when a store failed. The body never says
/// which proof failed.
#[must_use]
pub fn bearer_refusal(denied: forge_tenancy::BearerDenied) -> Response {
    use forge_tenancy::BearerDenied;

    let (status, category, code, message) = match denied {
        BearerDenied::MissingCredentials => (
            StatusCode::UNAUTHORIZED,
            ErrorCategory::Unauthenticated,
            "credentials_required",
            "authentication is required",
        ),
        BearerDenied::InvalidCredentials => (
            StatusCode::UNAUTHORIZED,
            ErrorCategory::Unauthenticated,
            "credentials_invalid",
            "credentials are invalid or expired",
        ),
        BearerDenied::Forbidden => (
            StatusCode::FORBIDDEN,
            ErrorCategory::Forbidden,
            "permission_denied",
            "permission denied",
        ),
        BearerDenied::Unavailable => (
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCategory::Infrastructure,
            "authentication_unavailable",
            "service temporarily unavailable",
        ),
    };
    let failure = ForgeError::new(category, code, message)
        .with_retryable(denied == BearerDenied::Unavailable);
    if denied == BearerDenied::Unavailable {
        warn!(code, "bearer authentication unavailable");
    }
    let mut response = json(status, &failure.report());
    if status == StatusCode::UNAUTHORIZED {
        response.headers_mut().insert(
            forge_http::header::WWW_AUTHENTICATE,
            forge_http::HeaderValue::from_static(forge_auth::BEARER_CHALLENGE),
        );
    }
    response
}

#[cfg(test)]
mod web_tests;

#[cfg(test)]
mod tests {
    use std::io;

    use super::*;
    use bytes::Bytes;
    use forge_http::{HeaderMap, Method, Request, Response, StatusCode, Uri};
    use uuid::Uuid;

    #[test]
    fn bearer_refusals_carry_go_codes_and_the_bearer_challenge() {
        use forge_tenancy::BearerDenied;

        for (denied, status, code) in [
            (
                BearerDenied::MissingCredentials,
                StatusCode::UNAUTHORIZED,
                "credentials_required",
            ),
            (
                BearerDenied::InvalidCredentials,
                StatusCode::UNAUTHORIZED,
                "credentials_invalid",
            ),
            (
                BearerDenied::Forbidden,
                StatusCode::FORBIDDEN,
                "permission_denied",
            ),
            (
                BearerDenied::Unavailable,
                StatusCode::SERVICE_UNAVAILABLE,
                "authentication_unavailable",
            ),
        ] {
            let response = bearer_refusal(denied);
            assert_eq!(response.status(), status, "{denied:?}");
            assert_eq!(body(&response)["code"], code, "{denied:?}");
            let challenge = response
                .headers()
                .get(forge_http::header::WWW_AUTHENTICATE)
                .and_then(|value| value.to_str().ok());
            if status == StatusCode::UNAUTHORIZED {
                assert_eq!(challenge, Some("Bearer realm=\"forge\""));
            } else {
                assert_eq!(challenge, None);
            }
        }
    }

    #[tokio::test]
    async fn at_least_pads_fast_work_and_never_shortens_it() {
        let minimum = std::time::Duration::from_millis(30);
        let started = std::time::Instant::now();
        assert_eq!(at_least(minimum, async { 7 }).await, 7);
        assert!(started.elapsed() >= minimum);
    }

    fn request(method: Method, path: &str) -> Request {
        Request::new_for_test(
            method,
            path.parse().expect("valid test URI"),
            HeaderMap::new(),
            Bytes::new(),
            "127.0.0.1:4000".parse().expect("valid test address"),
            Uuid::now_v7(),
        )
    }

    fn body(response: &Response) -> serde_json::Value {
        serde_json::from_slice(response.body()).expect("JSON body")
    }

    fn json_request(body: &str, content_type: Option<&str>) -> Request {
        let mut headers = HeaderMap::new();
        if let Some(value) = content_type {
            headers.insert(
                forge_http::header::CONTENT_TYPE,
                forge_http::HeaderValue::from_str(value).expect("valid header"),
            );
        }
        Request::new_for_test(
            Method::POST,
            Uri::from_static("/products"),
            headers,
            Bytes::copy_from_slice(body.as_bytes()),
            "127.0.0.1:4000".parse().expect("valid test address"),
            Uuid::now_v7(),
        )
    }

    #[derive(serde::Deserialize, Debug, forge_openapi::OpenApiSchema)]
    #[openapi(crate = "forge_openapi")]
    struct Product {
        sku: String,
    }

    #[test]
    fn a_wrong_media_type_is_415_and_a_right_one_decodes() {
        for content_type in [
            None,
            Some("text/plain"),
            Some("application/x-www-form-urlencoded"),
        ] {
            let req = json_request(r#"{"sku":"a"}"#, content_type);
            let failure = decode_json::<Product>(&req).expect_err("wrong media type");
            assert_eq!(failure.code(), UNSUPPORTED_MEDIA_TYPE);
            let response = error(&failure);
            assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
            assert_eq!(body(&response)["code"], "unsupported_media_type");
            assert!(decode_json_strict::<Product>(&req).is_err());
        }
        let req = json_request(r#"{"sku":"a"}"#, Some("application/json; charset=utf-8"));
        assert_eq!(decode_json::<Product>(&req).expect("decodes").sku, "a");
        // Only the one failure about the media type is 415.
        let other = ForgeError::new(ErrorCategory::Conflict, UNSUPPORTED_MEDIA_TYPE, "x");
        assert_eq!(error(&other).status(), StatusCode::CONFLICT);
    }

    #[test]
    fn strict_decoding_refuses_unknown_members_and_names_them() {
        let req = json_request(r#"{"sku":"a","price":1}"#, Some("application/json"));
        assert!(decode_json::<Product>(&req).is_ok(), "lenient ignores it");
        let failure = decode_json_strict::<Product>(&req).expect_err("unknown member");
        assert_eq!(failure.code(), "invalid_json");
        assert_eq!(error(&failure).status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body(&error(&failure))["metadata"]["member"], "price");
        let long = format!(r#"{{"sku":"a","{}":1}}"#, "k".repeat(200));
        let failure = decode_json_strict::<Product>(&json_request(&long, Some("application/json")))
            .expect_err("unknown member");
        let member = failure
            .metadata()
            .get("member")
            .and_then(serde_json::Value::as_str);
        assert_eq!(member.map(str::len), Some(64));
    }

    #[test]
    fn errors_use_category_status_and_hide_sources() {
        let failure = ForgeError::new(
            ErrorCategory::Infrastructure,
            "database_unavailable",
            "service temporarily unavailable",
        )
        .with_retryable(true)
        .with_source(io::Error::other("password=secret"));

        let response = error(&failure);

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let rendered = body(&response);
        assert_eq!(rendered["code"], "database_unavailable");
        assert_eq!(rendered["retryable"], true);
        assert!(!rendered.to_string().contains("password"));
    }

    #[test]
    fn validation_errors_are_unprocessable() {
        let failure = ForgeError::new(ErrorCategory::Validation, "invalid_sku", "invalid SKU")
            .with_metadata("field", "sku");

        let response = error(&failure);

        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body(&response)["metadata"]["field"], "sku");
    }

    #[test]
    fn decode_json_maps_malformed_bodies_to_validation() {
        #[derive(serde::Deserialize, Debug)]
        struct Input {
            #[serde(rename = "name")]
            _name: String,
        }
        let req = json_request(r#"{"title":"x"}"#, Some("application/json"));

        let failure = decode_json::<Input>(&req).expect_err("wrong shape");

        assert_eq!(failure.category(), ErrorCategory::Validation);
        assert_eq!(failure.code(), "invalid_json");
    }

    fn query_request(uri: &str) -> Request {
        let uri: Uri = uri.parse().expect("valid test URI");
        Request::new_for_test(
            forge_http::Method::GET,
            uri,
            HeaderMap::new(),
            Bytes::new(),
            "127.0.0.1:4000".parse().expect("valid test address"),
            Uuid::now_v7(),
        )
    }

    fn parameter(failure: &ForgeError) -> Option<&str> {
        failure
            .metadata()
            .get("parameter")
            .and_then(serde_json::Value::as_str)
    }

    #[derive(serde::Deserialize, Debug)]
    #[serde(deny_unknown_fields)]
    struct Paging {
        limit: Option<u16>,
        after: Option<forge_core::Id<Paging>>,
    }

    #[test]
    fn decode_query_parses_typed_parameters() -> Result<(), ForgeError> {
        let id = forge_core::Id::<Paging>::new();
        let paging: Paging =
            decode_query(&query_request(&format!("/products?limit=10&after={id}")))?;
        let empty: Paging = decode_query(&query_request("/products"))?;

        assert_eq!(paging.limit, Some(10));
        assert_eq!(paging.after, Some(id));
        assert!(empty.limit.is_none() && empty.after.is_none());
        Ok(())
    }

    #[test]
    fn decode_query_maps_failures_to_invalid_query() {
        let long = format!("/p?{}=1", "x".repeat(200));
        for (uri, expected) in [
            ("/p?limit=1&limit=2", Some("limit")),
            ("/p?limit=%zz", Some("limit")),
            ("/p?limit=%FF", Some("limit")),
            ("/p?limit=70000", Some("limit")),
            ("/p?after=1", Some("after")),
            ("/p?page=2", Some("page")),
            ("/p?limit=1&", None),
        ] {
            let failure = decode_query::<Paging>(&query_request(uri)).expect_err(uri);
            assert_eq!(failure.category(), ErrorCategory::Validation, "{uri}");
            assert_eq!(failure.code(), "invalid_query", "{uri}");
            assert_eq!(parameter(&failure), expected, "{uri}");
            // Caller-facing text never echoes the query.
            assert!(!failure.message().contains("70000"), "{uri}");
        }
        let failure = decode_query::<Paging>(&query_request(&long)).expect_err("long name");
        assert_eq!(parameter(&failure).map(str::len), Some(64));
        assert!(decode_query::<()>(&query_request("/p?x=1")).is_err());
    }

    #[test]
    fn path_param_outside_a_dynamic_route_is_a_validation_error() {
        let id = forge_core::Id::<Paging>::new();
        let req = request(forge_http::Method::GET, &format!("/products/{id}"));

        let failure = path_param::<forge_core::Id<Paging>>(&req, "id").expect_err("no capture");

        assert_eq!(failure.code(), "invalid_path_parameter");
        assert_eq!(failure.category(), ErrorCategory::Validation);
        assert_eq!(parameter(&failure), Some("id"));
    }

    #[tokio::test]
    async fn path_param_parses_captures_and_rejects_invalid_values() {
        let mut router = forge_http::Router::new();
        router
            .route(
                forge_http::Method::GET,
                "/products/{id}",
                |req: Request| async move {
                    match path_param::<forge_core::Id<Paging>>(&req, "id") {
                        Ok(_) => Response::empty(StatusCode::OK),
                        Err(failure) => error(&failure),
                    }
                },
            )
            .expect("route registers");
        let id = forge_core::Id::<Paging>::new();

        let found = router
            .dispatch(request(forge_http::Method::GET, &format!("/products/{id}")))
            .await;
        let invalid = router
            .dispatch(request(forge_http::Method::GET, "/products/secret-value"))
            .await;

        assert_eq!(found.status(), StatusCode::OK);
        assert_eq!(invalid.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let rendered = body(&invalid);
        assert_eq!(rendered["code"], "invalid_path_parameter");
        assert_eq!(rendered["metadata"]["parameter"], "id");
        assert!(!rendered.to_string().contains("secret-value"));
    }

    fn form_request(content_type: &str, body: &'static str) -> Request {
        let mut headers = HeaderMap::new();
        headers.insert(
            forge_http::header::CONTENT_TYPE,
            forge_http::HeaderValue::from_str(content_type).expect("valid header"),
        );
        Request::new_for_test(
            forge_http::Method::POST,
            Uri::from_static("/products"),
            headers,
            Bytes::from_static(body.as_bytes()),
            "127.0.0.1:4000".parse().expect("valid test address"),
            Uuid::now_v7(),
        )
    }

    #[derive(serde::Deserialize, Debug)]
    #[serde(deny_unknown_fields)]
    struct NewProduct {
        name: String,
        price: u32,
    }

    #[test]
    fn decode_form_maps_failures_to_invalid_form_with_the_field() {
        const FORM: &str = "application/x-www-form-urlencoded";
        let product: NewProduct =
            decode_form(&form_request(FORM, "name=Desk&price=10")).expect("valid form");
        assert_eq!((product.name.as_str(), product.price), ("Desk", 10));

        for (content_type, body, field) in [
            (FORM, "name=Desk&price=secret", Some("price")),
            (FORM, "name=a&name=b&price=1", Some("name")),
            (FORM, "price=1", Some("name")),
            (FORM, "name=a&price=1&admin=1", Some("admin")),
            (FORM, "name=a&&price=1", None),
            ("application/json", "{}", None),
        ] {
            let failure =
                decode_form::<NewProduct>(&form_request(content_type, body)).expect_err(body);
            assert_eq!(failure.category(), ErrorCategory::Validation, "{body}");
            assert_eq!(failure.code(), "invalid_form", "{body}");
            assert_eq!(
                failure
                    .metadata()
                    .get("field")
                    .and_then(serde_json::Value::as_str),
                field,
                "{body}"
            );
            assert!(!failure.message().contains("secret"), "{body}");
        }
        assert_eq!(
            error(&decode_form::<NewProduct>(&form_request(FORM, "")).expect_err("empty")).status(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }

    #[test]
    fn json_sets_status_and_content_type() {
        let response = json(StatusCode::CREATED, &serde_json::json!({"id": 1}));

        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(body(&response)["id"], 1);
    }
}
