//! What a route can declare and what the document says about it: every
//! builder option, and every description the document refuses.

use std::sync::Arc;

use forge_core::Uuid;
use forge_openapi::{
    Document, Info, OpenApiSchema, Operation, Param, ParamLocation, Response, Route, TypeRef,
};
use serde::Serialize;
use serde_json::{Value, json};

#[derive(Serialize, OpenApiSchema)]
#[openapi(crate = "forge_openapi")]
struct Post {
    title: String,
}

fn route(method: &str, path: &str, operation: Operation) -> Route {
    Route {
        method: method.to_owned(),
        path: path.to_owned(),
        operation: Some(Arc::new(operation)),
    }
}

fn build(routes: &[Route]) -> Result<Value, String> {
    Document::build(&Info::new("shop", "1.0.0"), routes)
        .map(|document| document.as_value().clone())
        .map_err(|error| error.to_string())
}

fn refused(routes: &[Route]) -> String {
    build(routes).expect_err("the description must be refused")
}

#[test]
fn the_info_object_carries_the_description() {
    let info = Info::new("shop", "2.0.0").description("The shop API.");
    assert_eq!(info.title(), "shop");
    let document = Document::build(&info, &[]).expect("an empty document is fine");
    assert_eq!(
        document.as_value()["info"],
        json!({"title": "shop", "version": "2.0.0", "description": "The shop API."})
    );
    assert!(document.paths().is_empty());
    assert!(document.undocumented().is_empty());
    assert_eq!(document.as_value()["paths"], json!({}));
    for (title, version) in [("", "1"), ("t", ""), ("  ", "1"), ("t", "\n")] {
        let error = Document::build(&Info::new(title, version), &[]).expect_err("needs both");
        assert_eq!(
            error.to_string(),
            "openapi: the document needs a title and a version"
        );
    }
}

#[test]
fn every_operation_option_is_documented() {
    let operation = Operation::new()
        .id("archive_post")
        .summary("Archive a post")
        .description("Moves the post to the archive.")
        .tag("posts")
        .tag("admin")
        .deprecated()
        .authenticated()
        .param(
            Param::header::<String>("x-trace")
                .doc("A trace id.")
                .example("abc")
                .format("trace"),
        )
        .param(Param::query::<Option<u32>>("limit").example("25"))
        .param(Param::query::<String>("q").required().doc("Search."))
        .response(
            Response::json::<Post>(200)
                .description("The archived post.")
                .header("etag")
                .header("X-Rate-Limit"),
        )
        .response(Response::text(202).description("Queued"))
        .response(Response::empty(299))
        .errors([404, 499]);
    assert!(operation.request_type().is_none());
    let document = build(&[route("POST", "/posts/{id}/archive", operation)]).expect("document");
    let archive = &document["paths"]["/posts/{id}/archive"]["post"];

    assert_eq!(archive["operationId"], "archive_post");
    assert_eq!(archive["summary"], "Archive a post");
    assert_eq!(archive["description"], "Moves the post to the archive.");
    assert_eq!(archive["tags"], json!(["posts", "admin"]));
    assert_eq!(archive["deprecated"], true);
    assert_eq!(archive["security"], json!([{"sessionCookie": []}]));
    assert!(
        archive.get("x-forge-permission").is_none(),
        "authenticated, not a permission"
    );
    assert_eq!(
        archive["parameters"],
        json!([
            {"name": "x-trace", "in": "header", "description": "A trace id.",
             "schema": {"type": "string", "format": "trace", "examples": ["abc"]}},
            {"name": "id", "in": "path", "required": true, "schema": {"type": "string"}},
            {"name": "limit", "in": "query",
             "schema": {"type": "integer", "format": "int64", "minimum": 0, "examples": [25]}},
            {"name": "q", "in": "query", "required": true, "description": "Search.",
             "schema": {"type": "string"}},
        ])
    );
    let responses = &archive["responses"];
    assert_eq!(responses["200"]["description"], "The archived post.");
    assert_eq!(
        responses["200"]["headers"],
        json!({"Etag": {"schema": {"type": "string"}}, "X-Rate-Limit": {"schema": {"type": "string"}}})
    );
    assert_eq!(responses["202"]["description"], "Queued");
    assert_eq!(
        responses["202"]["content"],
        json!({"text/plain": {"schema": {"type": "string"}}})
    );
    assert_eq!(
        responses["299"]["description"], "299",
        "an unassigned status has no text"
    );
    assert!(responses["299"].get("content").is_none());
    assert_eq!(responses["404"]["description"], "Not Found");
    assert_eq!(responses["499"]["description"], "Error");
    assert_eq!(responses["401"]["description"], "Unauthorized");
    assert_eq!(responses["default"]["description"], "Error");
    assert!(responses.get("403").is_none(), "no permission, no 403");
}

#[test]
fn a_declared_response_replaces_what_the_framework_would_imply() {
    let operation = Operation::new()
        .permission("posts:write")
        .request::<Post>()
        .response(Response::json::<Post>(201))
        .response(Response::empty(401).description("Sign in first"))
        .response(Response::empty(413));
    assert!(operation.request_type().is_some());
    let document = build(&[route("POST", "/posts", operation)]).expect("document");
    let responses = &document["paths"]["/posts"]["post"]["responses"];
    assert_eq!(responses["401"]["description"], "Sign in first");
    assert!(
        responses["401"].get("content").is_none(),
        "the declared 401 has no body"
    );
    assert!(responses["413"].get("content").is_none());
    assert_eq!(responses["403"]["description"], "Forbidden");
    assert_eq!(responses["415"]["description"], "Unsupported Media Type");
    assert_eq!(responses["422"]["description"], "Unprocessable Entity");
}

#[test]
fn with_success_puts_the_typed_response_first_and_replaces_its_status() {
    let operation = Operation::new()
        .response(Response::empty(200))
        .response(Response::empty(202))
        .with_success(Response::json::<Post>(200));
    let document = build(&[route("GET", "/posts", operation)]).expect("document");
    let responses = &document["paths"]["/posts"]["get"]["responses"];
    assert!(responses["200"]["content"]["application/json"].is_object());
    assert!(responses["202"].is_object());
    assert_eq!(Response::json::<Post>(201).status(), 201);
    assert_eq!(Response::empty(204).status(), 204);
    assert_eq!(Response::text(202).status(), 202);
}

#[test]
fn the_first_invalid_reason_is_kept() {
    let operation = Operation::new()
        .json::<Post>(200)
        .invalid("first reason")
        .invalid("second reason");
    let error = refused(&[route("GET", "/posts", operation)]);
    assert_eq!(error, "openapi: GET /posts: first reason");
}

#[test]
fn type_refs_and_parameters_know_their_types() {
    let reference = TypeRef::of::<Post>();
    assert!(reference.name().ends_with("Post"), "{}", reference.name());
    assert_eq!(format!("{reference:?}"), reference.name());
    assert!(TypeRef::of::<str>().name().contains("str"));
    let operation = Operation::new().request::<Vec<Post>>();
    let request = operation.request_type().expect("a request type");
    assert!(request.name().contains("Vec"), "{}", request.name());

    for (location, text) in [
        (ParamLocation::Header, "Header"),
        (ParamLocation::Path, "Path"),
        (ParamLocation::Query, "Query"),
    ] {
        assert_eq!(format!("{location:?}"), text);
    }
    assert!(
        ParamLocation::Header < ParamLocation::Path && ParamLocation::Path < ParamLocation::Query
    );
    let _ = Param::path::<Uuid>("id");
}

#[test]
fn parameters_are_sorted_by_location_then_name_and_never_nullable() {
    let operation = Operation::new()
        .param(Param::query::<Option<String>>("b"))
        .param(Param::query::<Option<String>>("a"))
        .param(Param::header::<Option<u8>>("x-b"))
        .param(Param::path::<Option<Uuid>>("id"))
        .json::<Post>(200);
    let document = build(&[route("GET", "/things/{id}/{kind}", operation)]).expect("document");
    let parameters = document["paths"]["/things/{id}/{kind}"]["get"]["parameters"]
        .as_array()
        .cloned()
        .expect("parameters");
    let order: Vec<String> = parameters
        .iter()
        .map(|parameter| {
            format!(
                "{}:{}",
                parameter["in"].as_str().unwrap_or_default(),
                parameter["name"].as_str().unwrap_or_default()
            )
        })
        .collect();
    assert_eq!(
        order,
        ["header:x-b", "path:id", "path:kind", "query:a", "query:b"]
    );
    assert_eq!(
        parameters[1]["schema"],
        json!({"type": "string", "format": "uuid"})
    );
    assert_eq!(parameters[3]["schema"], json!({"type": "string"}));
}

#[test]
fn every_inaccurate_description_is_refused_with_the_route_and_the_reason() {
    let ok = || Operation::new().json::<Post>(200);
    for (routes, expected) in [
        (
            vec![route("GET", "posts", ok())],
            "GET posts: cannot describe: the path \"posts\" is not absolute",
        ),
        (
            vec![route("GET", "/posts/{id", ok())],
            "cannot describe: wildcard segment \"{id\"",
        ),
        (
            vec![route("GET", "/posts/{}", ok())],
            "cannot describe: wildcard segment \"{}\"",
        ),
        (
            vec![route(
                "GET",
                "/x",
                Operation::new().response(Response::json::<Post>(99)),
            )],
            "response status 99 is not an HTTP status",
        ),
        (
            vec![route(
                "GET",
                "/x",
                Operation::new().response(Response::json::<Post>(600)),
            )],
            "response status 600 is not an HTTP status",
        ),
        (
            vec![route(
                "GET",
                "/x",
                Operation::new()
                    .response(Response::empty(200))
                    .response(Response::empty(200)),
            )],
            "response 200 is declared twice",
        ),
        (
            vec![route(
                "GET",
                "/x",
                Operation::new().response(Response::text(304)),
            )],
            "a 304 response has no body",
        ),
        (
            vec![route("GET", "/x", ok().errors([399]))],
            "error status 399 is not 4xx or 5xx",
        ),
        (
            vec![route("GET", "/x", ok().errors([600]))],
            "error status 600 is not 4xx or 5xx",
        ),
        (
            vec![route("GET", "/x", ok().param(Param::query::<String>("")))],
            "a query parameter has no name",
        ),
        (
            vec![route("GET", "/x", ok().param(Param::header::<String>("")))],
            "a header parameter has no name",
        ),
        (
            vec![route(
                "GET",
                "/x",
                ok().param(Param::query::<String>("q"))
                    .param(Param::query::<u8>("q")),
            )],
            "query parameter \"q\" is declared twice",
        ),
        (
            vec![route(
                "GET",
                "/x/{id}",
                ok().param(Param::path::<String>("id"))
                    .param(Param::path::<u8>("id")),
            )],
            "path parameter \"id\" is declared twice",
        ),
        (
            vec![route("TRACE2", "/x", ok())],
            "OpenAPI 3.1 has no TRACE2 operations",
        ),
    ] {
        let error = refused(&routes);
        assert!(error.contains(expected), "{expected}: {error}");
    }
    // The same parameter name in two locations is fine.
    let both = ok()
        .param(Param::query::<String>("id"))
        .param(Param::header::<String>("id"));
    assert!(build(&[route("GET", "/x", both)]).is_ok());
}

#[test]
fn every_http_method_openapi_knows_is_documented_in_any_case() {
    let routes: Vec<Route> = [
        "get", "PUT", "Post", "DELETE", "options", "HEAD", "patch", "TRACE",
    ]
    .iter()
    .map(|method| {
        route(
            method,
            "/x",
            Operation::new().response(Response::empty(200)),
        )
    })
    .collect();
    let document = build(&routes).expect("document");
    let item = document["paths"]["/x"]
        .as_object()
        .cloned()
        .expect("path item");
    let mut methods: Vec<&str> = item.keys().map(String::as_str).collect();
    methods.sort_unstable();
    assert_eq!(
        methods,
        [
            "delete", "get", "head", "options", "patch", "post", "put", "trace"
        ]
    );
}

#[test]
fn the_root_path_and_unusual_segments_get_operation_ids() {
    let document = build(&[
        route("GET", "/", Operation::new().response(Response::empty(200))),
        route(
            "GET",
            "/v1/user-profiles/{profile_id}",
            Operation::new().json::<Post>(200),
        ),
    ])
    .expect("document");
    assert_eq!(document["paths"]["/"]["get"]["operationId"], "get");
    assert_eq!(
        document["paths"]["/v1/user-profiles/{profile_id}"]["get"]["operationId"],
        "getV1UserProfilesByProfileId"
    );
}

#[test]
fn the_session_scheme_is_published_only_when_a_route_needs_it() {
    let open = build(&[route("GET", "/x", Operation::new().json::<Post>(200))]).expect("document");
    assert!(open["components"].get("securitySchemes").is_none());
    for operation in [
        Operation::new().json::<Post>(200).authenticated(),
        Operation::new().json::<Post>(200).permission("posts:read"),
    ] {
        let document = build(&[route("GET", "/x", operation)]).expect("document");
        assert_eq!(
            document["components"]["securitySchemes"]["sessionCookie"]["name"],
            "__Host-forge_session"
        );
    }
}
