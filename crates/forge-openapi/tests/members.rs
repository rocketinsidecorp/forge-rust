//! `unknown_member`: which member of a request body the type's schema does
//! not list.

#![allow(dead_code)]

use std::collections::BTreeMap;

use forge_openapi::{OpenApiSchema, unknown_member};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize, OpenApiSchema)]
#[openapi(crate = "forge_openapi")]
struct Address {
    city: String,
}

#[derive(Deserialize, OpenApiSchema)]
#[openapi(crate = "forge_openapi")]
struct Audit {
    created_by: String,
}

#[derive(Deserialize, OpenApiSchema)]
#[openapi(crate = "forge_openapi")]
struct Wrapper(Address);

#[derive(Deserialize, OpenApiSchema)]
#[openapi(crate = "forge_openapi")]
#[serde(rename_all = "camelCase")]
struct Person {
    full_name: String,
    home: Address,
    other: Option<Address>,
    visits: Vec<Address>,
    labels: BTreeMap<String, Address>,
    anything: Value,
    #[serde(flatten)]
    audit: Audit,
    #[serde(skip_deserializing, default)]
    computed: u8,
    #[serde(default, alias = "oldName")]
    nick: Option<String>,
    wrapped: Wrapper,
}

fn person() -> Value {
    json!({
        "fullName": "Ada",
        "home": {"city": "London"},
        "other": null,
        "visits": [{"city": "Paris"}],
        "labels": {"work": {"city": "Rome"}},
        "anything": {"whatever": [1, {"x": 2}]},
        "created_by": "me",
        "wrapped": {"city": "Oslo"}
    })
}

fn unknown(value: &Value) -> Option<String> {
    unknown_member::<Person>(value)
}

#[test]
fn a_body_with_only_listed_members_has_no_unknown_member() {
    assert_eq!(unknown(&person()), None);
    let mut body = person();
    body["other"] = json!({"city": "Lima"});
    body["nick"] = json!("a");
    body["computed"] = json!(1); // listed, even though serde ignores it
    assert_eq!(unknown(&body), None);
}

#[test]
fn unknown_members_are_reported_with_their_path() {
    for (path, value, want) in [
        ("", json!(1), "extra"),
        ("home", json!({"city": "x", "zip": 1}), "home.zip"),
        ("other", json!({"city": "x", "zip": 1}), "other.zip"),
        (
            "visits",
            json!([{"city": "a"}, {"city": "b", "k": 1}]),
            "visits[1].k",
        ),
        (
            "labels",
            json!({"work": {"city": "a", "k": 1}}),
            "labels.work.k",
        ),
        ("wrapped", json!({"city": "a", "k": 1}), "wrapped.k"),
    ] {
        let mut body = person();
        if path.is_empty() {
            body["extra"] = value;
        } else {
            body[path] = value;
        }
        assert_eq!(unknown(&body).as_deref(), Some(want), "{path}");
    }
}

#[test]
fn maps_and_free_form_values_accept_any_member() {
    let mut body = person();
    body["labels"]["any key"] = json!({"city": "x"});
    body["anything"] = json!({"a": {"b": {"c": [1, 2, {"d": null}]}}});
    assert_eq!(unknown(&body), None);
}

#[test]
fn an_alias_is_not_listed_and_flattened_members_are() {
    let mut body = person();
    body["oldName"] = json!("a");
    assert_eq!(unknown(&body).as_deref(), Some("oldName"));
    // `created_by` comes from the flattened struct and is listed.
    assert_eq!(unknown(&person()), None);
}

#[test]
fn types_that_are_not_objects_have_nothing_to_refuse() {
    assert_eq!(unknown_member::<String>(&json!("x")), None);
    assert_eq!(
        unknown_member::<Vec<Address>>(&json!([{"city": "x"}])),
        None
    );
    assert_eq!(
        unknown_member::<Vec<Address>>(&json!([{"city": "x", "k": 1}])).as_deref(),
        Some("[0].k")
    );
    assert_eq!(unknown_member::<Value>(&json!({"a": 1})), None);
    assert_eq!(
        unknown_member::<BTreeMap<String, u8>>(&json!({"a": 1, "b": 2})),
        None
    );
}
