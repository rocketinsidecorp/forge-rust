//! The OpenAPI document Forge serves at `/openapi.json` in development and
//! test: valid OpenAPI 3.1 describing the JSON routes, and every response
//! below checked against what it declares (a contract test; route your own
//! JSON endpoints through `json_router` to check them too).

use std::error::Error;

use forge::http::{HeaderMap, Method, Request, Router, StatusCode};
use forge::openapi::{CheckResponses, Info, OPENAPI_PATH};

type TestResult = Result<(), Box<dyn Error>>;

/// The framework's health routes and the application's JSON routes, the
/// document served as the application serves it, and a layer that fails
/// the test on any response that does not match the document.
fn json_router() -> Result<Router, Box<dyn Error>> {
    let mut router = Router::new();
    router.standard_health_routes()?;
    app::adapters::http::status::routes(&mut router)?;
    let document = router.openapi(&Info::new(
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION"),
    ))?;
    router.serve_openapi(&document)?;
    router.layer(CheckResponses::new(document, |failure| panic!("{failure}")));
    Ok(router)
}

fn request(method: Method, path: &str) -> Result<Request, Box<dyn Error>> {
    Ok(Request::new_for_test(
        method,
        path.parse()?,
        HeaderMap::new(),
        String::new().into(),
        "127.0.0.1:4000".parse()?,
        forge::core::Id::<Request>::new().into_uuid(),
    ))
}

#[tokio::test]
async fn the_document_describes_the_json_routes() -> TestResult {
    let router = json_router()?;
    let response = router.dispatch(request(Method::GET, OPENAPI_PATH)?).await;
    assert_eq!(response.status(), StatusCode::OK);
    let document: serde_json::Value = serde_json::from_slice(response.body())?;
    assert_eq!(document["openapi"], "3.1.0");
    assert_eq!(document["info"]["title"], env!("CARGO_PKG_NAME"));
    for path in ["/v1/status", "/health", "/health/live", "/health/ready"] {
        assert!(
            document["paths"].get(path).is_some(),
            "the OpenAPI document does not describe {path}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn json_responses_match_the_document() -> TestResult {
    let router = json_router()?;
    for path in ["/v1/status", "/health", "/health/live", "/health/ready"] {
        let response = router.dispatch(request(Method::GET, path)?).await;
        assert_eq!(response.status(), StatusCode::OK, "{path}");
    }
    Ok(())
}
