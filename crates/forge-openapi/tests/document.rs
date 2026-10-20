//! Schemas derived from Rust types and serde attributes, the document's
//! shape and bytes, build errors and the contract check.

#![allow(dead_code)]

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use forge_core::{Id, Timestamp, Uuid};
use forge_openapi::{
    ContractError, Document, Info, OpenApiSchema, Operation, Param, Response, Route, schema_of,
};
use serde::Serialize;
use serde_json::{Value, json};

#[derive(Serialize, OpenApiSchema)]
#[openapi(crate = "forge_openapi")]
struct Node {
    #[openapi(doc = "The node's name.", example = "root")]
    name: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    children: Vec<Node>,
    parent: Option<Box<Node>>,
}

#[derive(Serialize, OpenApiSchema)]
#[openapi(crate = "forge_openapi")]
#[serde(deny_unknown_fields)]
struct Audit {
    created_at: Timestamp,
    #[serde(skip_serializing_if = "Option::is_none")]
    deleted_at: Option<Timestamp>,
}

#[derive(Serialize, OpenApiSchema)]
#[openapi(crate = "forge_openapi")]
#[serde(rename_all = "camelCase")]
struct KitchenSink {
    #[serde(flatten)]
    audit: Audit,
    id: Id<KitchenSink>,
    owner_id: Option<Uuid>,
    count: i64,
    small: i8,
    ratio: f64,
    enabled: bool,
    tags: Vec<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    labels: BTreeMap<String, u32>,
    raw: Value,
    pair: [u16; 2],
    root: Node,
    #[openapi(required, nonnull)]
    #[serde(skip_serializing_if = "Option::is_none")]
    required_text: Option<String>,
    #[serde(skip)]
    skipped: String,
    #[openapi(format = "email")]
    formatted: String,
    #[serde(rename = "kind")]
    state: State,
    nested: HashMap<String, Vec<Node>>,
    #[serde(serialize_with = "as_text")]
    #[openapi(as = String)]
    custom: u8,
    r#type: String,
}

fn as_text<S: serde::Serializer>(value: &u8, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.collect_str(value)
}

#[derive(Serialize, OpenApiSchema)]
#[openapi(crate = "forge_openapi", doc = "Where a post is.")]
#[serde(rename_all = "snake_case")]
enum State {
    Draft,
    InReview,
    #[serde(rename = "live")]
    Published,
    #[serde(skip)]
    #[allow(dead_code)]
    Hidden,
}

#[derive(Serialize, OpenApiSchema)]
#[openapi(crate = "forge_openapi")]
struct Page<T> {
    items: Vec<T>,
    next: Option<String>,
}

#[derive(Serialize, OpenApiSchema)]
#[openapi(crate = "forge_openapi")]
#[serde(transparent)]
struct Email {
    address: String,
}

#[derive(Serialize, OpenApiSchema)]
#[openapi(crate = "forge_openapi")]
struct Sku(String);

#[derive(Serialize, OpenApiSchema)]
#[openapi(crate = "forge_openapi", name = "HealthReport")]
struct Readiness {
    status: String,
}

#[derive(Serialize, OpenApiSchema)]
#[openapi(crate = "forge_openapi")]
struct Wrapper {
    #[serde(flatten)]
    extra: BTreeMap<String, String>,
    #[serde(flatten)]
    maybe: Option<Audit>,
}

fn route(method: &str, path: &str, operation: Operation) -> Route {
    Route {
        method: method.to_owned(),
        path: path.to_owned(),
        operation: Some(Arc::new(operation)),
    }
}

fn info() -> Info {
    Info::new("shop", "0.1.0")
}

fn components() -> serde_json::Map<String, Value> {
    match schema_of::<KitchenSink>() {
        Ok((_, components)) => components,
        Err(error) => panic!("{error}"),
    }
}

#[test]
fn structs_follow_serde_names_requiredness_and_nullability() {
    let components = components();
    let sink = &components["KitchenSink"];
    assert_eq!(sink["type"], "object");
    assert_eq!(sink["additionalProperties"], false);
    let properties = sink["properties"].as_object().cloned().unwrap_or_default();
    let mut names: Vec<&str> = properties.keys().map(String::as_str).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "count",
            "created_at",
            "custom",
            "deleted_at",
            "enabled",
            "formatted",
            "id",
            "kind",
            "labels",
            "nested",
            "ownerId",
            "pair",
            "ratio",
            "raw",
            "requiredText",
            "root",
            "small",
            "tags",
            "type",
        ],
        "camelCase, rename, flatten and skip; created_at and deleted_at come from the \
         flattened Audit, which keeps its own snake_case names"
    );
    let required: Vec<&str> = sink["required"]
        .as_array()
        .map(|list| list.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    assert!(
        required.contains(&"ownerId"),
        "Option without skip is required"
    );
    assert!(required.contains(&"requiredText"), "openapi(required)");
    assert!(
        !required.contains(&"labels"),
        "skip_serializing_if is optional"
    );
    assert!(!required.contains(&"deleted_at"));
    assert_eq!(
        properties["ownerId"],
        json!({"type": ["string", "null"], "format": "uuid"})
    );
    assert_eq!(
        properties["requiredText"],
        json!({"type": "string"}),
        "nonnull"
    );
    assert_eq!(properties["id"]["pattern"], forge_openapi::UUID_V7_PATTERN);
    assert_eq!(properties["created_at"]["format"], "date-time");
    assert_eq!(
        properties["small"],
        json!({"type": "integer", "format": "int32", "minimum": -128, "maximum": 127})
    );
    assert_eq!(properties["count"]["format"], "int64");
    assert_eq!(properties["ratio"]["format"], "double");
    assert_eq!(
        properties["tags"],
        json!({"type": "array", "items": {"type": "string"}})
    );
    assert_eq!(
        properties["pair"],
        json!({"type": "array", "items": {"type": "integer", "format": "int32", "minimum": 0, "maximum": 65535}, "minItems": 2, "maxItems": 2})
    );
    assert_eq!(properties["raw"], json!({}));
    assert_eq!(
        properties["formatted"],
        json!({"type": "string", "format": "email"})
    );
    assert_eq!(
        properties["custom"],
        json!({"type": "string"}),
        "openapi(as)"
    );
    assert_eq!(
        properties["kind"],
        json!({"$ref": "#/components/schemas/State"})
    );
    assert_eq!(
        properties["labels"],
        json!({"type": "object", "additionalProperties": {"type": "integer", "format": "int64", "minimum": 0}})
    );
}

#[test]
fn named_types_are_components_and_recursion_ends_at_the_reference() {
    let components = components();
    let node = &components["Node"];
    assert_eq!(
        node["properties"]["parent"],
        json!({"anyOf": [{"$ref": "#/components/schemas/Node"}, {"type": "null"}]})
    );
    assert_eq!(
        node["properties"]["children"]["items"],
        json!({"$ref": "#/components/schemas/Node"})
    );
    assert_eq!(
        node["properties"]["name"]["description"],
        "The node's name."
    );
    assert_eq!(node["properties"]["name"]["examples"], json!(["root"]));
    assert_eq!(
        components["State"],
        json!({"type": "string", "enum": ["draft", "in_review", "live"], "description": "Where a post is."})
    );
}

#[test]
fn generic_newtype_named_and_flattened_types() -> Result<(), forge_openapi::BuildError> {
    let (schema, components) = schema_of::<Page<Node>>()?;
    assert_eq!(schema, json!({"$ref": "#/components/schemas/Page_Node"}));
    assert!(components.contains_key("Node"));
    assert_eq!(schema_of::<Email>()?.0, json!({"type": "string"}));
    assert_eq!(schema_of::<Sku>()?.0, json!({"type": "string"}));
    let (schema, _) = schema_of::<Readiness>()?;
    assert_eq!(schema, json!({"$ref": "#/components/schemas/HealthReport"}));
    let (_, components) = schema_of::<Wrapper>()?;
    let wrapper = &components["Wrapper"];
    assert_eq!(wrapper["additionalProperties"], json!({"type": "string"}));
    assert_eq!(
        wrapper.get("required"),
        None,
        "a flattened Option is optional"
    );
    assert!(wrapper["properties"]["created_at"].is_object());
    Ok(())
}

fn status_operation() -> Operation {
    Operation::new()
        .summary("Service status")
        .tag("status")
        .json::<Readiness>(200)
}

#[test]
fn the_document_has_the_go_shape_and_identical_bytes_for_any_route_order()
-> Result<(), Box<dyn std::error::Error>> {
    let routes = vec![
        route("GET", "/v1/status", status_operation()),
        route(
            "POST",
            "/v1/posts",
            Operation::new()
                .summary("Create a post")
                .permission("posts:write")
                .request::<Node>()
                .response(Response::json::<Node>(201).header("location"))
                .errors([409]),
        ),
        route(
            "DELETE",
            "/v1/posts/{id}",
            Operation::new()
                .param(Param::path::<Uuid>("id").doc("The post."))
                .param(Param::query::<Option<i64>>("version").required())
                .response(Response::empty(204)),
        ),
        Route {
            method: "GET".to_owned(),
            path: "/".to_owned(),
            operation: None,
        },
    ];
    let first = Document::build(&info(), &routes)?;
    let mut reversed = routes.clone();
    reversed.reverse();
    let second = Document::build(&info(), &reversed)?;
    assert_eq!(first.json(), second.json());
    let text = std::str::from_utf8(first.json())?;
    assert!(text.starts_with("{\n  \"components\": {\n"), "{text}");
    assert!(text.ends_with("}\n"));
    assert_eq!(first.undocumented(), ["GET /"]);
    assert_eq!(first.paths(), ["/v1/posts", "/v1/posts/{id}", "/v1/status"]);

    let document: Value = serde_json::from_slice(first.json())?;
    assert_eq!(document["openapi"], "3.1.0");
    assert_eq!(
        document["jsonSchemaDialect"],
        "https://json-schema.org/draft/2020-12/schema"
    );
    assert_eq!(
        document["info"],
        json!({"title": "shop", "version": "0.1.0"})
    );
    let create = &document["paths"]["/v1/posts"]["post"];
    assert_eq!(create["operationId"], "postV1Posts");
    assert_eq!(create["x-forge-permission"], "posts:write");
    assert_eq!(create["security"], json!([{"sessionCookie": []}]));
    assert_eq!(create["requestBody"]["required"], true);
    let responses = create["responses"].as_object().cloned().unwrap_or_default();
    let mut statuses: Vec<&str> = responses.keys().map(String::as_str).collect();
    statuses.sort_unstable();
    assert_eq!(
        statuses,
        ["201", "401", "403", "409", "413", "422", "default"]
    );
    assert_eq!(
        responses["201"]["headers"],
        json!({"Location": {"schema": {"type": "string"}}})
    );
    assert_eq!(responses["201"]["description"], "Created");
    assert_eq!(
        responses["401"]["content"],
        json!({"text/plain": {"schema": {"type": "string"}}})
    );
    assert_eq!(
        responses["409"]["content"]["application/json"]["schema"],
        json!({"$ref": "#/components/schemas/ErrorReport"})
    );
    assert_eq!(
        document["components"]["securitySchemes"]["sessionCookie"]["in"],
        "cookie"
    );
    let delete = &document["paths"]["/v1/posts/{id}"]["delete"];
    assert_eq!(delete["operationId"], "deleteV1PostsById");
    assert_eq!(
        delete["parameters"],
        json!([
            {"name": "id", "in": "path", "required": true, "description": "The post.", "schema": {"type": "string", "format": "uuid"}},
            {"name": "version", "in": "query", "required": true, "schema": {"type": "integer", "format": "int64"}},
        ])
    );
    assert!(document["components"]["schemas"]["ErrorReport"].is_object());
    Ok(())
}

fn build_error(routes: &[Route]) -> String {
    match Document::build(&info(), routes) {
        Ok(_) => panic!("the description must be refused"),
        Err(error) => error.to_string(),
    }
}

#[test]
fn inaccurate_descriptions_are_build_errors() {
    for (routes, expected) in [
        (
            vec![route("GET", "/x", Operation::new())],
            "declares no response",
        ),
        (
            vec![route(
                "GET",
                "/x",
                Operation::new().response(Response::json::<String>(204)),
            )],
            "a 204 response has no body",
        ),
        (
            vec![route(
                "GET",
                "/x",
                Operation::new().json::<String>(200).errors([200]),
            )],
            "not 4xx or 5xx",
        ),
        (
            vec![route(
                "GET",
                "/x",
                Operation::new().json::<String>(200).errors([404, 404]),
            )],
            "both a response and an error",
        ),
        (
            vec![route(
                "GET",
                "/x",
                Operation::new()
                    .json::<String>(200)
                    .param(Param::path::<String>("id")),
            )],
            "not a wildcard of the route",
        ),
        (
            vec![
                route("GET", "/a", status_operation().id("same")),
                route("GET", "/b", status_operation().id("same")),
            ],
            "share the operation ID",
        ),
        (
            vec![route("CONNECT", "/x", status_operation())],
            "OpenAPI 3.1 has no CONNECT operations",
        ),
        (
            vec![route("GET", "/x", status_operation().invalid("bad status"))],
            "bad status",
        ),
    ] {
        let error = build_error(&routes);
        assert!(error.contains(expected), "{error}");
    }
    assert!(Document::build(&Info::new(" ", "1"), &[]).is_err());
}

mod one {
    #[derive(serde::Serialize, forge_openapi::OpenApiSchema)]
    #[openapi(crate = "forge_openapi", name = "Item")]
    pub struct Report {
        pub a: String,
    }
}

mod two {
    #[derive(serde::Serialize, forge_openapi::OpenApiSchema)]
    #[openapi(crate = "forge_openapi", name = "Item")]
    pub struct Summary {
        pub b: String,
    }
}

#[test]
fn colliding_component_names_name_both_types() {
    let error = build_error(&[
        route("GET", "/a", Operation::new().json::<one::Report>(200)),
        route("GET", "/b", Operation::new().json::<two::Summary>(200)),
    ]);
    assert!(
        error.contains("one::Report") && error.contains("two::Summary"),
        "{error}"
    );
}

#[derive(Serialize, OpenApiSchema)]
#[openapi(crate = "forge_openapi")]
struct NotAnObject {
    #[serde(flatten)]
    inner: String,
}

#[test]
fn flattening_a_non_object_is_a_build_error() {
    let error = build_error(&[route(
        "GET",
        "/a",
        Operation::new().json::<NotAnObject>(200),
    )]);
    assert!(error.contains("not written as an object"), "{error}");
    let _ = NotAnObject {
        inner: String::new(),
    }
    .inner;
}

#[test]
fn responses_are_checked_against_the_document() -> Result<(), forge_openapi::BuildError> {
    let document = Document::build(
        &info(),
        &[
            route(
                "GET",
                "/v1/posts/{id}",
                Operation::new().json::<Node>(200).errors([404]),
            ),
            route(
                "GET",
                "/v1/posts/new",
                Operation::new().json::<Readiness>(200),
            ),
            route(
                "DELETE",
                "/v1/posts/{id}",
                Operation::new().response(Response::empty(204)),
            ),
        ],
    )?;
    let json = Some("application/json; charset=utf-8");
    let ok = |method, path, status, content_type, body: &str| {
        document.validate_response(method, path, status, content_type, body.as_bytes())
    };
    assert_eq!(
        ok(
            "GET",
            "/v1/posts/1",
            200,
            json,
            r#"{"name":"a","parent":null,"children":[{"name":"b","parent":null}]}"#
        ),
        Ok(())
    );
    assert_eq!(
        ok("GET", "/v1/posts/new", 200, json, r#"{"status":"ok"}"#),
        Ok(()),
        "static wins"
    );
    assert_eq!(
        ok(
            "GET",
            "/v1/posts/1",
            404,
            json,
            r#"{"code":"x","category":"not_found","message":"m","retryable":false}"#
        ),
        Ok(())
    );
    assert_eq!(ok("DELETE", "/v1/posts/1", 204, None, ""), Ok(()));
    assert_eq!(
        ok("HEAD", "/v1/posts/1", 200, None, ""),
        Err(ContractError::Undocumented {
            method: "HEAD".to_owned(),
            path: "/v1/posts/1".to_owned()
        })
    );
    assert!(matches!(
        ok("GET", "/nope", 200, json, "{}"),
        Err(ContractError::Undocumented { .. })
    ));
    for (status, content_type, body, reason) in [
        (
            200,
            json,
            r#"{"name":"a"}"#,
            "required member \"parent\" is missing",
        ),
        (
            200,
            json,
            r#"{"name":"a","parent":null,"extra":1}"#,
            "member \"extra\" is not in the schema",
        ),
        (
            200,
            json,
            r#"{"name":1,"parent":null}"#,
            "a number is not of type string",
        ),
        (200, Some("text/html"), "<p>", "has Content-Type"),
        (200, json, "not json", "body is not JSON"),
        (500, json, "{}", "required member"),
    ] {
        let error = ok("GET", "/v1/posts/1", status, content_type, body)
            .err()
            .map(|error| error.to_string())
            .unwrap_or_default();
        assert!(error.contains(reason), "{body}: {error}");
    }
    let error = ok("DELETE", "/v1/posts/1", 204, None, "body")
        .err()
        .map(|error| error.to_string())
        .unwrap_or_default();
    assert!(error.contains("without a body"), "{error}");
    assert_eq!(
        document.validate_route_response(
            "GET",
            "/v1/posts/{id}",
            200,
            json,
            br#"{"name":"a","parent":null}"#
        ),
        Ok(())
    );
    Ok(())
}
