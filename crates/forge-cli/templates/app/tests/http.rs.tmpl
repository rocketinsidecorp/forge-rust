//! The HTML stack end to end, without a network: layout, Content-Security-
//! Policy, CSRF meta tag and cookie, digested Turbo and Stimulus assets, and
//! CSRF enforcement on form submissions.

use std::error::Error;

use forge::http::{HeaderMap, HeaderValue, Method, Request, Response, Router, StatusCode, header};

type TestResult = Result<(), Box<dyn Error>>;

const FORM: &str = "application/x-www-form-urlencoded";

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Probe {
    name: String,
}

/// The application's routes plus a form endpoint for these tests; the CSRF
/// layer protects it like any other route.
fn router() -> Result<Router, Box<dyn Error>> {
    let mut router = Router::new();
    let site = app::adapters::http::site(&mut router)?;
    app::adapters::http::routes(&mut router, &site)?;
    router.route(Method::POST, "/test-form", |request: Request| async move {
        match forge::web::decode_form::<Probe>(&request) {
            Ok(probe) => Response::new(StatusCode::CREATED, probe.name),
            Err(failure) => forge::web::error(&failure),
        }
    })?;
    Ok(router)
}

fn request(
    method: Method,
    uri: &str,
    headers: &[(&'static str, &str)],
    body: &str,
) -> Result<Request, Box<dyn Error>> {
    let mut map = HeaderMap::new();
    for (name, value) in headers {
        map.append(*name, HeaderValue::from_str(value)?);
    }
    Ok(Request::new_for_test(
        method,
        uri.parse()?,
        map,
        body.to_owned().into(),
        "127.0.0.1:4000".parse()?,
        forge::core::Id::<Request>::new().into_uuid(),
    ))
}

fn header_text(response: &Response, name: header::HeaderName) -> Option<&str> {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
}

fn body(response: &Response) -> Result<String, Box<dyn Error>> {
    Ok(String::from_utf8(response.body().to_vec())?)
}

/// The token in the `__Host-forge_csrf` cookie a response sets.
fn csrf_cookie(response: &Response) -> Option<String> {
    response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find_map(|value| value.strip_prefix("__Host-forge_csrf="))
        .and_then(|value| value.split(';').next())
        .map(str::to_owned)
}

#[tokio::test]
async fn home_page_is_html_with_layout_csp_and_csrf_token() -> TestResult {
    let response = router()?
        .dispatch(request(Method::GET, "/", &[], "")?)
        .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        header_text(&response, header::CONTENT_TYPE),
        Some("text/html; charset=utf-8")
    );
    let policy = header_text(&response, header::CONTENT_SECURITY_POLICY).unwrap_or_default();
    assert!(policy.contains("script-src 'self' 'sha256-"), "{policy}");
    assert!(!policy.contains("unsafe-inline"), "{policy}");
    let page = body(&response)?;
    assert!(page.contains(env!("CARGO_PKG_NAME")), "{page}");
    assert!(page.contains("<script type=\"importmap\">"), "{page}");
    let token = csrf_cookie(&response).ok_or("the page sets the CSRF cookie")?;
    assert!(
        page.contains(&format!("<meta name=\"csrf-token\" content=\"{token}\">")),
        "{page}"
    );
    Ok(())
}

#[tokio::test]
async fn turbo_and_stimulus_are_served_digested() -> TestResult {
    let router = router()?;
    let page = body(&router.dispatch(request(Method::GET, "/", &[], "")?).await)?;

    for module in ["/assets/vendor/turbo-", "/assets/vendor/stimulus-"] {
        let start = page.find(module).ok_or(module)?;
        let end = start + page[start..].find('"').ok_or(module)?;
        let asset = router
            .dispatch(request(Method::GET, &page[start..end], &[], "")?)
            .await;

        assert_eq!(asset.status(), StatusCode::OK, "{module}");
        assert_eq!(
            header_text(&asset, header::CONTENT_TYPE),
            Some("text/javascript; charset=utf-8")
        );
        assert_eq!(
            header_text(&asset, header::CACHE_CONTROL),
            Some("public, max-age=31536000, immutable")
        );
    }
    Ok(())
}

#[tokio::test]
async fn form_posts_need_the_csrf_token_of_the_page() -> TestResult {
    let router = router()?;
    let page = router.dispatch(request(Method::GET, "/", &[], "")?).await;
    let token = csrf_cookie(&page).ok_or("the page sets the CSRF cookie")?;
    let cookie = format!("__Host-forge_csrf={token}");
    let forged = "0".repeat(64);

    for (headers, form) in [
        (
            vec![("content-type", FORM)],
            format!("name=x&csrf_token={token}"),
        ),
        (
            vec![("cookie", cookie.as_str()), ("content-type", FORM)],
            "name=x".to_owned(),
        ),
        (
            vec![("cookie", cookie.as_str()), ("content-type", FORM)],
            format!("name=x&csrf_token={forged}"),
        ),
    ] {
        let response = router
            .dispatch(request(Method::POST, "/test-form", &headers, &form)?)
            .await;
        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "{headers:?} {form}"
        );
    }

    let accepted = router
        .dispatch(request(
            Method::POST,
            "/test-form",
            &[("cookie", cookie.as_str()), ("content-type", FORM)],
            &format!("name=x&csrf_token={token}"),
        )?)
        .await;
    assert_eq!(accepted.status(), StatusCode::CREATED);
    assert_eq!(body(&accepted)?, "x");
    Ok(())
}
