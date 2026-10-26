//! The contract check's schema subset, formats and document lookups, driven
//! with schemas written out so every keyword has a failing case.

use std::sync::Arc;

use serde_json::json;

use super::*;
use crate::{Info, Operation, Response, Route};

fn validate(schema: &Value, value: &Value) -> Result<(), String> {
    let document = json!({"components": {"schemas": {
        "Text": {"type": "string"},
        "Loop": {"$ref": "#/components/schemas/Loop"},
        "Foreign": {"$ref": "https://example.test/schema"},
        "Gone": {"$ref": "#/components/schemas/Missing"},
        "Node": {
            "type": "object",
            "properties": {"next": {"anyOf": [{"$ref": "#/components/schemas/Node"}, {"type": "null"}]}},
            "additionalProperties": false
        },
    }}});
    Validator {
        document: &document,
        depth: 0,
    }
    .validate(schema, value, "")
}

fn failure(schema: &Value, value: &Value) -> String {
    validate(schema, value).expect_err("the value must be refused")
}

#[test]
fn types_are_checked_and_described() {
    for (value, kind, said) in [
        (json!(null), "string", "null is not of type string"),
        (json!(true), "string", "a boolean is not of type string"),
        (json!("x"), "integer", "a string is not of type integer"),
        (json!([1]), "object", "an array is not of type object"),
        (json!({"a": 1}), "array", "an object is not of type array"),
        (json!(3), "string", "a number is not of type string"),
        (json!(1.5), "integer", "a number is not of type integer"),
    ] {
        let message = failure(&json!({"type": kind}), &value);
        assert!(message.contains(said), "{message}");
        assert!(message.starts_with("body: "), "{message}");
    }
    // Several types are alternatives; a whole float is an integer.
    let nullable = json!({"type": ["string", "null"]});
    assert_eq!(validate(&nullable, &json!(null)), Ok(()));
    assert_eq!(validate(&nullable, &json!("x")), Ok(()));
    let message = failure(&nullable, &json!(1));
    assert!(message.contains("not of type string or null"), "{message}");
    assert_eq!(validate(&json!({"type": "integer"}), &json!(4.0)), Ok(()));
    assert_eq!(validate(&json!({"type": "integer"}), &json!(-4)), Ok(()));
    assert_eq!(validate(&json!({"type": "number"}), &json!(4)), Ok(()));
    assert_eq!(validate(&json!({"type": "boolean"}), &json!(false)), Ok(()));
    // A `type` that is neither text nor a list says nothing.
    assert_eq!(validate(&json!({"type": 7}), &json!("anything")), Ok(()));
    assert_eq!(validate(&json!({}), &json!({"free": ["form"]})), Ok(()));
}

#[test]
fn enums_and_alternatives_list_what_was_expected() {
    let schema = json!({"type": "string", "enum": ["draft", "live"]});
    assert_eq!(validate(&schema, &json!("live")), Ok(()));
    let message = failure(&schema, &json!("gone"));
    assert!(message.contains("\"gone\" is not one of"), "{message}");

    let either = json!({"anyOf": [{"type": "string"}, {"type": "integer", "minimum": 10}]});
    assert_eq!(validate(&either, &json!("x")), Ok(()));
    assert_eq!(validate(&either, &json!(12)), Ok(()));
    let message = failure(&either, &json!(3));
    assert!(message.contains("matches no alternative"), "{message}");
    assert!(message.contains("not of type string"), "{message}");
    assert!(message.contains("is less than 10"), "{message}");
}

#[test]
fn strings_are_bounded_formatted_and_matched() {
    let bounded = json!({"type": "string", "minLength": 2, "maxLength": 3});
    assert_eq!(validate(&bounded, &json!("ab")), Ok(()));
    assert_eq!(
        validate(&bounded, &json!("éé€")),
        Ok(()),
        "characters, not bytes"
    );
    assert!(failure(&bounded, &json!("a")).contains("is shorter than 2"));
    assert!(failure(&bounded, &json!("abcd")).contains("is longer than 3"));

    let date_time = json!({"type": "string", "format": "date-time"});
    assert_eq!(validate(&date_time, &json!("2026-10-20T12:34:56Z")), Ok(()));
    assert!(failure(&date_time, &json!("yesterday")).contains("not an RFC 3339 date-time"));
    let uuid = json!({"type": "string", "format": "uuid"});
    assert_eq!(
        validate(&uuid, &json!("0192f1a0-0000-4000-8000-000000000000")),
        Ok(())
    );
    assert!(failure(&uuid, &json!("not-a-uuid")).contains("is not a UUID"));
    // Formats the validator does not know are annotations.
    let email = json!({"type": "string", "format": "email"});
    assert_eq!(validate(&email, &json!("not an email")), Ok(()));

    let v7 = json!({"type": "string", "pattern": UUID_V7_PATTERN});
    assert_eq!(
        validate(&v7, &json!("0192f1a0-0000-7000-8000-000000000000")),
        Ok(())
    );
    assert!(
        failure(&v7, &json!("0192f1a0-0000-4000-8000-000000000000")).contains("does not match")
    );
    let other = json!({"type": "string", "pattern": "^[a-z]+$"});
    let message = failure(&other, &json!("abc"));
    assert!(
        message.contains("does not support the pattern"),
        "{message}"
    );
}

#[test]
fn numbers_are_bounded() {
    let bounded = json!({"type": "integer", "minimum": 0, "maximum": 255});
    for value in [0, 7, 255] {
        assert_eq!(validate(&bounded, &json!(value)), Ok(()));
    }
    assert!(failure(&bounded, &json!(-1)).contains("-1 is less than 0"));
    assert!(failure(&bounded, &json!(256)).contains("256 is greater than 255"));
    assert_eq!(
        validate(&json!({"type": "number", "minimum": 0.5}), &json!(0.5)),
        Ok(())
    );
}

#[test]
fn arrays_are_counted_and_each_item_is_checked_with_its_position() {
    let schema =
        json!({"type": "array", "minItems": 1, "maxItems": 2, "items": {"type": "integer"}});
    assert_eq!(validate(&schema, &json!([1, 2])), Ok(()));
    assert!(failure(&schema, &json!([])).contains("has 0 items, fewer than 1"));
    assert!(failure(&schema, &json!([1, 2, 3])).contains("has 3 items, more than 2"));
    let message = failure(&schema, &json!([1, "two"]));
    assert!(message.starts_with("body/1: "), "{message}");
    // Without `items`, any element is fine.
    assert_eq!(
        validate(&json!({"type": "array"}), &json!([1, "a", null])),
        Ok(())
    );
}

#[test]
fn objects_require_their_members_and_refuse_others_unless_the_schema_allows_them() {
    let schema = json!({
        "type": "object",
        "required": ["id"],
        "properties": {"id": {"type": "integer"}, "a/b": {"type": "string"}, "t~": {"type": "string"}},
        "additionalProperties": false,
    });
    assert_eq!(validate(&schema, &json!({"id": 1})), Ok(()));
    assert!(failure(&schema, &json!({})).contains("required member \"id\" is missing"));
    assert!(
        failure(&schema, &json!({"id": 1, "x": 1})).contains("member \"x\" is not in the schema")
    );
    // Member names are escaped in the pointer like JSON Pointer does.
    assert!(failure(&schema, &json!({"id": 1, "a/b": 1})).starts_with("body/a~1b: "));
    assert!(failure(&schema, &json!({"id": 1, "t~": 1})).starts_with("body/t~0: "));

    let map = json!({"type": "object", "additionalProperties": {"type": "integer"}});
    assert_eq!(validate(&map, &json!({"a": 1, "b": 2})), Ok(()));
    assert!(failure(&map, &json!({"a": "x"})).starts_with("body/a: "));
    let open = json!({"type": "object", "additionalProperties": true});
    assert_eq!(validate(&open, &json!({"a": "anything"})), Ok(()));
    assert_eq!(
        validate(&json!({"type": "object"}), &json!({"a": 1})),
        Ok(())
    );
}

#[test]
fn references_are_followed_and_unusable_ones_are_reported() {
    let text = json!({"$ref": "#/components/schemas/Text"});
    assert_eq!(validate(&text, &json!("x")), Ok(()));
    assert!(failure(&text, &json!(1)).contains("not of type string"));
    let message = failure(&json!({"$ref": "https://example.test/schema"}), &json!(1));
    assert!(message.contains("unsupported reference"), "{message}");
    let message = failure(&json!({"$ref": "#/components/schemas/Missing"}), &json!(1));
    assert!(message.contains("unknown reference"), "{message}");
    let message = failure(&json!({"$ref": "#/components/schemas/Foreign"}), &json!(1));
    assert!(message.contains("unsupported reference"), "{message}");
    let message = failure(&json!({"$ref": "#/components/schemas/Gone"}), &json!(1));
    assert!(message.contains("unknown reference"), "{message}");
    // A schema that is not an object cannot say anything.
    let message = failure(&json!(true), &json!(1));
    assert!(
        message.contains("the document has no schema here"),
        "{message}"
    );
}

#[test]
fn a_reference_cycle_and_a_deeply_nested_value_are_bounded() {
    let message = failure(&json!({"$ref": "#/components/schemas/Loop"}), &json!(1));
    assert!(message.contains("nests too deeply"), "{message}");

    let node = json!({"$ref": "#/components/schemas/Node"});
    let mut value = json!({});
    for _ in 0..120 {
        value = json!({"next": value});
    }
    let message = failure(&node, &value);
    assert!(message.contains("nests too deeply"), "{message}");
    // A reasonable depth is fine.
    let mut value = json!({});
    for _ in 0..10 {
        value = json!({"next": value});
    }
    assert_eq!(validate(&node, &value), Ok(()));
}

#[test]
fn formats_reject_what_the_grammar_rejects() {
    for valid in [
        "2026-10-20T12:34:56Z",
        "2026-10-20t12:34:56z",
        "2026-10-20T12:34:56.5+02:00",
        "2026-10-20T23:59:60-00:30",
        "2026-12-31T00:00:00Z",
    ] {
        assert!(is_date_time(valid), "{valid}");
    }
    for invalid in [
        "",
        "2026-10-20",
        "2026-10-20T12:34:56",
        "2026-10-20T12:34:56.Z",
        "2026-10-20T12:34:56+0200",
        "2026-10-20T12:34:56+02:0x",
        "2026-10-20T12:34:56Zjunk",
        "2026-00-20T12:34:56Z",
        "2026-10-00T12:34:56Z",
        "2026-10-32T12:34:56Z",
        "2026-10-20T24:34:56Z",
        "2026-10-20T12:60:56Z",
        "2026-10-20T12:34:61Z",
        "2026/10/20T12:34:56Z",
        "2026-10-20 12:34:56Z",
        "2026-10-20T12-34-56Z",
        "20x6-10-20T12:34:56Z",
        "2026-1x-20T12:34:56Z",
        "2026-10-2xT12:34:56Z",
        "2026-10-20T1x:34:56Z",
        "2026-10-20T12:3x:56Z",
        "2026-10-20T12:34:5xZ",
    ] {
        assert!(!is_date_time(invalid), "{invalid}");
    }
    assert!(is_uuid("0192F1A0-0000-7000-8000-000000000000"), "any case");
    for invalid in [
        "",
        "0192f1a0-0000-7000-8000-00000000000",
        "0192f1a000007000800000000000000000000",
        "0192f1a0_0000-7000-8000-000000000000",
        "0192f1a0-0000-7000-8000-00000000000g",
    ] {
        assert!(!is_uuid(invalid), "{invalid}");
    }
    assert!(is_uuid_v7("0192f1a0-0000-7000-bfff-000000000000"));
    assert!(
        !is_uuid_v7("0192f1a0-0000-7000-c000-000000000000"),
        "variant"
    );
}

fn document_serving(method: &str, path: &str, operation: Operation) -> Document {
    Document::build(
        &Info::new("t", "1"),
        &[Route {
            method: method.to_owned(),
            path: path.to_owned(),
            operation: Some(Arc::new(operation)),
        }],
    )
    .expect("document")
}

#[test]
fn requests_are_matched_to_routes_like_the_router_does() {
    let document = Document::build(
        &Info::new("t", "1"),
        &[
            Route {
                method: "GET".to_owned(),
                path: "/".to_owned(),
                operation: Some(Arc::new(Operation::new().response(Response::text(200)))),
            },
            Route {
                method: "GET".to_owned(),
                path: "/posts/{id}/comments".to_owned(),
                operation: Some(Arc::new(Operation::new().response(Response::text(200)))),
            },
        ],
    )
    .expect("document");
    let ok = |method: &str, path: &str| {
        document.validate_response(method, path, 200, Some("text/plain"), b"hello")
    };
    assert_eq!(ok("GET", "/"), Ok(()));
    assert_eq!(
        ok("get", "/posts/1/comments"),
        Ok(()),
        "methods compare ignoring case"
    );
    assert_eq!(
        ok("GET", "/posts/1/comments?page=2#top"),
        Ok(()),
        "query and fragment are not the path"
    );
    for (method, path) in [
        ("GET", "posts/1/comments"),
        ("GET", "/posts//comments"),
        ("GET", "/posts/1"),
        ("GET", "/posts/1/comments/2"),
        ("POST", "/"),
    ] {
        assert_eq!(
            ok(method, path),
            Err(ContractError::Undocumented {
                method: method.to_owned(),
                path: path.to_owned()
            }),
            "{method} {path}"
        );
    }
    assert_eq!(
        document.validate_route_response(
            "get",
            "/posts/{id}/comments",
            200,
            Some("text/plain"),
            b""
        ),
        Ok(())
    );
    let error = document
        .validate_route_response("GET", "/posts/1/comments", 200, None, b"")
        .expect_err("the pattern, not a path");
    assert_eq!(
        error.to_string(),
        "openapi: no documented operation serves the request: GET /posts/1/comments"
    );
}

#[test]
fn text_bodies_must_be_utf8_and_head_has_no_body_to_check() {
    let document = document_serving(
        "GET",
        "/hello",
        Operation::new().response(Response::text(200)),
    );
    assert_eq!(
        document.validate_response(
            "GET",
            "/hello",
            200,
            Some("text/plain; charset=utf-8"),
            "olá".as_bytes()
        ),
        Ok(())
    );
    let error = document
        .validate_response("GET", "/hello", 200, Some("text/plain"), &[0xff, 0xfe])
        .expect_err("not text");
    assert!(error.to_string().contains("not UTF-8 text"), "{error}");
    let error = document
        .validate_response("GET", "/hello", 200, None, b"hello")
        .expect_err("no content type");
    assert!(error.to_string().contains("want text/plain"), "{error}");
    assert!(
        error
            .to_string()
            .starts_with("openapi: response does not match the document: GET /hello 200")
    );

    let head = document_serving(
        "HEAD",
        "/hello",
        Operation::new().response(Response::empty(200)),
    );
    assert_eq!(
        head.validate_response("HEAD", "/hello", 200, None, b"ignored"),
        Ok(())
    );
    // `default` answers any status the operation did not declare.
    assert_eq!(
        head.validate_response("HEAD", "/hello", 201, None, b""),
        Ok(())
    );
}

#[test]
fn a_tree_that_lacks_the_operation_or_the_status_is_reported() {
    let mut document =
        document_serving("GET", "/x", Operation::new().response(Response::empty(200)));
    // Every operation has a `default` response, which any status falls under.
    let refusal = br#"{"code":"x","category":"internal","message":"m","retryable":false}"#;
    assert_eq!(
        document.validate_response("GET", "/x", 418, Some("application/json"), refusal),
        Ok(())
    );

    document.tree = Arc::new(json!({"paths": {}}));
    let error = document
        .validate_response("GET", "/x", 200, None, b"")
        .expect_err("no operation");
    assert!(
        error
            .to_string()
            .contains("the document has no such operation"),
        "{error}"
    );

    document.tree =
        Arc::new(json!({"paths": {"/x": {"get": {"responses": {"200": {"description": "OK"}}}}}}));
    assert_eq!(
        document.validate_response("GET", "/x", 200, None, b" \n"),
        Ok(())
    );
    let error = document
        .validate_response("GET", "/x", 404, None, b"")
        .expect_err("not declared and no default");
    assert!(
        error.to_string().contains("the status is not declared"),
        "{error}"
    );
    let error = document
        .validate_response("GET", "/x", 200, None, b"body")
        .expect_err("declared without a body");
    assert!(
        error
            .to_string()
            .contains("declared without a body but has one"),
        "{error}"
    );

    document.tree = Arc::new(json!({"paths": {"/x": {"get": {"responses": {"200": {
        "description": "OK",
        "content": {"application/xml": {"schema": {"type": "string"}}},
    }}}}}}));
    assert_eq!(
        document.validate_response("GET", "/x", 200, Some("application/xml"), b"<a/>"),
        Ok(()),
        "media types the validator does not read are not read"
    );
    let error = document
        .validate_response("GET", "/x", 200, Some("application/json"), b"{}")
        .expect_err("not declared");
    assert!(
        error.to_string().contains("want application/xml"),
        "{error}"
    );

    // A JSON body whose media type has no schema says nothing about it.
    document.tree = Arc::new(json!({"paths": {"/x": {"get": {"responses": {"200": {
        "description": "OK",
        "content": {"application/json": {}},
    }}}}}}));
    let error = document
        .validate_response("GET", "/x", 200, Some("application/json"), b"{}")
        .expect_err("no schema");
    assert!(
        error
            .to_string()
            .contains("the document has no schema here"),
        "{error}"
    );
}

#[test]
fn a_catch_all_is_documented_as_one_parameter_and_matched_like_the_router() {
    let text = || Some(Arc::new(Operation::new().response(Response::text(200))));
    let route = |path: &str| Route {
        method: "GET".to_owned(),
        path: path.to_owned(),
        operation: text(),
    };
    let document = Document::build(
        &Info::new("t", "1"),
        &[
            route("/files/{key...}"),
            route("/files/static"),
            route("/files/{dir}/meta"),
        ],
    )
    .expect("document");
    // OpenAPI has no multi-segment parameter: `{key...}` is `{key}`, as in
    // Forge-Go's document.
    assert_eq!(
        document.paths(),
        ["/files/static", "/files/{dir}/meta", "/files/{key}"]
    );
    let operation = &document.as_value()["paths"]["/files/{key}"]["get"];
    assert_eq!(operation["operationId"], "getFilesByKey");
    assert_eq!(operation["parameters"][0]["name"], "key");
    assert_eq!(operation["parameters"][0]["in"], "path");
    let ok = |path: &str| document.validate_response("GET", path, 200, Some("text/plain"), b"x");
    for path in ["/files/", "/files/a", "/files/a/b/c", "/files/a/meta/x"] {
        assert_eq!(ok(path), Ok(()), "{path}");
    }
    assert!(matches!(
        ok("/files"),
        Err(ContractError::Undocumented { .. })
    ));
    assert_eq!(
        document.validate_route_response("GET", "/files/{key...}", 200, Some("text/plain"), b""),
        Ok(())
    );

    // A route the document would spell like another one is refused, and a
    // catch-all must be the last segment.
    for paths in [
        &["/files/{key...}", "/files/{key}"][..],
        &["/files/{key...}/meta"][..],
    ] {
        let routes: Vec<Route> = paths.iter().map(|path| route(path)).collect();
        assert!(
            Document::build(&Info::new("t", "1"), &routes).is_err(),
            "{paths:?}"
        );
    }
}
