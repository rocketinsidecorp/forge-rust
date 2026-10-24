//! `forge::openapi::typed`: decoding, results and errors as the description
//! says, checked with the document itself.

use std::error::Error;

use bytes::Bytes;
use forge::core::{ErrorCategory, ForgeError};
use forge::http::{HeaderMap, HeaderValue, Method, Request, Router, StatusCode, header};
use forge::openapi::{CheckResponses, Info, Json, NoBody, OpenApiSchema, Operation, typed};
use serde::{Deserialize, Serialize};

#[derive(Deserialize, OpenApiSchema)]
#[serde(deny_unknown_fields)]
struct NewPost {
    title: String,
}

#[derive(Serialize, OpenApiSchema)]
struct Post {
    title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    body: Option<String>,
}

/// A comment, which does not say `deny_unknown_fields`: the document still
/// says `additionalProperties: false`, and a typed handler enforces it.
#[derive(Deserialize, Serialize, OpenApiSchema)]
struct Comment {
    text: String,
    #[serde(default)]
    author: Option<String>,
}

fn request(method: Method, path: &str, body: &str) -> Result<Request, Box<dyn Error>> {
    request_as(method, path, body, Some("application/json"))
}

fn request_as(
    method: Method,
    path: &str,
    body: &str,
    content_type: Option<&str>,
) -> Result<Request, Box<dyn Error>> {
    let mut headers = HeaderMap::new();
    if let Some(content_type) = content_type {
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_str(content_type)?);
    }
    Ok(Request::new_for_test(
        method,
        path.parse()?,
        headers,
        Bytes::from(body.to_owned()),
        "127.0.0.1:4000".parse()?,
        uuid::Uuid::now_v7(),
    ))
}

fn router() -> Result<Router, Box<dyn Error>> {
    let mut router = Router::new();
    router.route(
        Method::POST,
        "/v1/posts",
        typed(
            StatusCode::CREATED,
            Operation::new().summary("Create a post").errors([409]),
            |_request: Request, Json(post): Json<NewPost>| async move {
                if post.title == "taken" {
                    return Err(ForgeError::new(
                        ErrorCategory::Conflict,
                        "post_exists",
                        "a post with this title exists",
                    ));
                }
                Ok(Post {
                    title: post.title,
                    body: None,
                })
            },
        ),
    )?;
    router.route(
        Method::POST,
        "/v1/comments",
        typed(
            StatusCode::CREATED,
            Operation::new().summary("Create a comment"),
            |_request: Request, Json(comment): Json<Comment>| async move { Ok(comment) },
        ),
    )?;
    router.route(
        Method::GET,
        "/v1/status",
        typed(
            StatusCode::OK,
            Operation::new().summary("Service status"),
            |_request: Request, NoBody| async {
                Ok::<_, ForgeError>(Post {
                    title: "ok".to_owned(),
                    body: Some("up".to_owned()),
                })
            },
        ),
    )?;
    Ok(router)
}

#[tokio::test]
async fn typed_handlers_answer_as_their_description_says() -> Result<(), Box<dyn Error>> {
    let mut router = router()?;
    let document = router.openapi(&Info::new("app", "0.1.0"))?;
    let create = &document.as_value()["paths"]["/v1/posts"]["post"];
    assert_eq!(
        create["requestBody"]["content"]["application/json"]["schema"]["$ref"],
        "#/components/schemas/NewPost"
    );
    let mut statuses: Vec<String> = create["responses"]
        .as_object()
        .map(|responses| responses.keys().cloned().collect())
        .unwrap_or_default();
    statuses.sort_unstable();
    assert_eq!(statuses, ["201", "409", "413", "415", "422", "default"]);
    assert!(
        document.as_value()["paths"]["/v1/status"]["get"]
            .get("requestBody")
            .is_none()
    );

    router.layer(CheckResponses::new(document, |failure| panic!("{failure}")));
    for (method, path, body, status) in [
        (Method::POST, "/v1/posts", r#"{"title":"hello"}"#, 201),
        (Method::POST, "/v1/posts", r#"{"title":"taken"}"#, 409),
        (Method::POST, "/v1/posts", r#"{"title":1}"#, 422),
        (Method::POST, "/v1/posts", r#"{"title":"a","x":1}"#, 422),
        (Method::GET, "/v1/status", "", 200),
    ] {
        let response = router.dispatch(request(method, path, body)?).await;
        assert_eq!(response.status().as_u16(), status, "{path} {body}");
    }
    Ok(())
}

#[test]
fn a_typed_status_without_a_body_is_an_invalid_description() -> Result<(), Box<dyn Error>> {
    let mut router = Router::new();
    router.route(
        Method::DELETE,
        "/v1/posts",
        typed(
            StatusCode::NO_CONTENT,
            Operation::new(),
            |_request: Request, NoBody| async { Ok::<_, ForgeError>(()) },
        ),
    )?;
    let error = router
        .openapi(&Info::new("app", "0.1.0"))
        .err()
        .map(|error| error.to_string())
        .unwrap_or_default();
    assert!(error.contains("2xx status with a body"), "{error}");
    Ok(())
}

#[tokio::test]
async fn typed_requests_enforce_the_media_type_and_the_listed_members() -> Result<(), Box<dyn Error>>
{
    let mut router = router()?;
    let document = router.openapi(&Info::new("app", "0.1.0"))?;
    router.layer(CheckResponses::new(document, |failure| panic!("{failure}")));
    let post = |body: &str, content_type: Option<&str>| {
        request_as(Method::POST, "/v1/comments", body, content_type)
    };

    // Accepted: the listed members, with or without the optional one, and
    // the media type in any case with a UTF-8 charset.
    for (body, content_type) in [
        (r#"{"text":"hi"}"#, "application/json"),
        (r#"{"text":"hi","author":"ada"}"#, "application/json"),
        (r#"{"text":"hi"}"#, "Application/JSON; charset=utf-8"),
    ] {
        let response = router.dispatch(post(body, Some(content_type))?).await;
        assert_eq!(response.status(), StatusCode::CREATED, "{body}");
    }

    // Rejected: 415 for another media type (or none), 422 for unknown members.
    for content_type in [
        None,
        Some("text/plain"),
        Some("application/x-www-form-urlencoded"),
        Some("application/json; charset=latin1"),
    ] {
        let response = router
            .dispatch(post(r#"{"text":"hi"}"#, content_type)?)
            .await;
        assert_eq!(
            response.status(),
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "{content_type:?}"
        );
        let report: serde_json::Value = serde_json::from_slice(response.body())?;
        assert_eq!(report["code"], "unsupported_media_type");
    }
    let response = router
        .dispatch(post(
            r#"{"text":"hi","admin":true}"#,
            Some("application/json"),
        )?)
        .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let report: serde_json::Value = serde_json::from_slice(response.body())?;
    assert_eq!(report["code"], "invalid_json");
    assert_eq!(report["metadata"]["member"], "admin");
    // The handler never ran: nothing about the refused member is echoed
    // beyond its name.
    assert!(!report.to_string().contains("true"), "{report}");
    Ok(())
}
