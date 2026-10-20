//! Descriptions travel with handlers, the router builds and serves the
//! document, and `CheckResponses` checks every response against it.

use std::sync::{Arc, Mutex};

use bytes::Bytes;
use forge_http::{
    CheckResponses, HeaderMap, Method, OPENAPI_PATH, Request, Response, Router, StatusCode,
    describe,
};
use forge_openapi::{Info, OpenApiSchema, Operation};
use serde::Serialize;

#[derive(Serialize, OpenApiSchema)]
#[openapi(crate = "forge_openapi")]
struct Post {
    title: String,
}

fn request(method: Method, path: &str) -> Request {
    Request::new_for_test(
        method,
        path.parse().unwrap_or_default(),
        HeaderMap::new(),
        Bytes::new(),
        std::net::SocketAddr::from(([127, 0, 0, 1], 4000)),
        uuid::Uuid::now_v7(),
    )
}

fn router() -> Result<Router, forge_http::RouteError> {
    let mut router = Router::new();
    router.standard_health_routes()?;
    router.route(
        Method::GET,
        "/v1/posts/{id}",
        describe(
            |request: Request| async move {
                // A wrong body for the first post, a right one otherwise.
                let body = if request.uri().path().ends_with("/1") {
                    r#"{"title":1}"#
                } else {
                    r#"{"title":"hello"}"#
                };
                let mut response = Response::new(StatusCode::OK, body);
                response.headers_mut().insert(
                    forge_http::header::CONTENT_TYPE,
                    forge_http::HeaderValue::from_static("application/json"),
                );
                response
            },
            Operation::new().summary("A post").json::<Post>(200),
        ),
    )?;
    router.route(Method::GET, "/", |_| async {
        Response::new(StatusCode::OK, "home")
    })?;
    Ok(router)
}

#[tokio::test]
async fn the_router_documents_described_routes_and_lists_the_others()
-> Result<(), Box<dyn std::error::Error>> {
    let mut router = router()?;
    let document = router.openapi(&Info::new("app", "0.1.0"))?;
    assert_eq!(
        document.paths(),
        ["/health", "/health/live", "/health/ready", "/v1/posts/{id}"]
    );
    assert_eq!(document.undocumented(), ["GET /"]);
    let value = document.as_value();
    assert_eq!(
        value["paths"]["/health/live"]["get"]["operationId"],
        "healthLive"
    );
    assert!(router.has_route(&Method::GET, "/v1/posts/{id}"));
    assert!(!router.has_route(&Method::GET, OPENAPI_PATH));

    router.serve_openapi(&document)?;
    let served = router.dispatch(request(Method::GET, OPENAPI_PATH)).await;
    assert_eq!(served.status(), StatusCode::OK);
    assert_eq!(served.body().as_ref(), document.json());
    assert_eq!(
        served
            .headers()
            .get(forge_http::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("application/json")
    );
    // The document route is not described itself.
    let again = router.openapi(&Info::new("app", "0.1.0"))?;
    assert_eq!(again.undocumented(), ["GET /", "GET /openapi.json"]);
    Ok(())
}

#[tokio::test]
async fn check_responses_reports_only_mismatches_of_documented_routes()
-> Result<(), Box<dyn std::error::Error>> {
    let mut router = router()?;
    let document = router.openapi(&Info::new("app", "0.1.0"))?;
    let failures = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&failures);
    router.layer(CheckResponses::new(document, move |failure| {
        if let Ok(mut list) = recorded.lock() {
            list.push(failure.to_string());
        }
    }));
    for path in [
        "/health",
        "/health/live",
        "/health/ready",
        "/v1/posts/2",
        "/",
        "/missing",
    ] {
        router.dispatch(request(Method::GET, path)).await;
    }
    assert!(failures.lock().map(|list| list.is_empty()).unwrap_or(false));

    let response = router.dispatch(request(Method::GET, "/v1/posts/1")).await;
    assert_eq!(response.body().as_ref(), br#"{"title":1}"#, "unchanged");
    let list = failures.lock().map(|list| list.clone()).unwrap_or_default();
    assert_eq!(list.len(), 1, "{list:?}");
    assert!(
        list[0].starts_with("GET /v1/posts/1: openapi: response does not match")
            && list[0].contains("not of type string"),
        "{list:?}"
    );
    Ok(())
}
