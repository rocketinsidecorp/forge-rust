//! The OpenAPI contract check (forge_openapi::Document::validate_response,
//! ADR 0039): a real response against the published document.
//!
//! Input: NUL-separated fields: method, path, status, content type and the
//! body. Properties: every response is valid or a `ContractError`, the
//! verdict is deterministic, and a valid `200` for the post route is a JSON
//! object with the string `id` and `title` the schema requires.
#![no_main]

use std::sync::{Arc, OnceLock};

use forge_openapi::{
    ContractError, Document, Info, OpenApiSchema, Operation, Param, Response, Route,
};
use libfuzzer_sys::fuzz_target;
use serde::Serialize;

#[derive(Serialize, OpenApiSchema)]
#[openapi(crate = "forge_openapi")]
struct Post {
    id: String,
    title: String,
    tags: Vec<String>,
    note: Option<String>,
}

#[derive(Serialize, OpenApiSchema)]
#[openapi(crate = "forge_openapi")]
struct Page {
    items: Vec<Post>,
    next: Option<String>,
}

fn route(method: &str, path: &str, operation: Operation) -> Route {
    Route {
        method: method.to_owned(),
        path: path.to_owned(),
        operation: Some(Arc::new(operation)),
    }
}

fn document() -> &'static Document {
    static DOCUMENT: OnceLock<Document> = OnceLock::new();
    DOCUMENT.get_or_init(|| {
        let routes = [
            route(
                "GET",
                "/v1/posts",
                Operation::new().json::<Page>(200).errors([401]),
            ),
            route(
                "POST",
                "/v1/posts",
                Operation::new()
                    .request::<Post>()
                    .response(Response::json::<Post>(201).header("location"))
                    .errors([400, 409]),
            ),
            route(
                "GET",
                "/v1/posts/{id}",
                Operation::new()
                    .param(Param::path::<String>("id"))
                    .json::<Post>(200)
                    .errors([404]),
            ),
            route(
                "DELETE",
                "/v1/posts/{id}",
                Operation::new()
                    .param(Param::path::<String>("id"))
                    .response(Response::empty(204))
                    .errors([404]),
            ),
            route(
                "GET",
                "/health",
                Operation::new().response(Response::text(200)),
            ),
        ];
        match Document::build(&Info::new("fuzz", "0.1.0"), &routes) {
            Ok(document) => document,
            Err(error) => panic!("the fixed document does not build: {error}"),
        }
    })
}

fuzz_target!(|data: &[u8]| {
    let mut fields = data.splitn(5, |byte| *byte == 0);
    let text =
        |bytes: Option<&[u8]>| String::from_utf8_lossy(bytes.unwrap_or_default()).into_owned();
    let method = text(fields.next());
    let path = text(fields.next());
    let status: u16 = text(fields.next()).trim().parse().unwrap_or(200);
    let content_type = text(fields.next());
    let body = fields.next().unwrap_or_default();
    let content_type = (!content_type.is_empty()).then_some(content_type.as_str());

    let document = document();
    let first = document.validate_response(&method, &path, status, content_type, body);
    let second = document.validate_response(&method, &path, status, content_type, body);
    assert_eq!(first, second, "the verdict is not deterministic");
    match &first {
        Ok(()) | Err(ContractError::Undocumented { .. } | ContractError::Mismatch(_)) => {}
    }

    if first.is_ok() && method.eq_ignore_ascii_case("GET") && status == 200 {
        let single = path
            .strip_prefix("/v1/posts/")
            .filter(|rest| !rest.contains(['/', '?', '#']));
        if single.is_some() {
            let value: serde_json::Value =
                serde_json::from_slice(body).expect("a valid JSON response parses as JSON");
            assert!(
                value.get("id").is_some_and(serde_json::Value::is_string)
                    && value.get("title").is_some_and(serde_json::Value::is_string),
                "a post without a string id and title was accepted"
            );
        }
    }
});
