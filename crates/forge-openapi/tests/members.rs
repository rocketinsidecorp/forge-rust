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
    #[serde(default, alias = "oldName", alias = "formerName")]
    nick: Option<String>,
    wrapped: Wrapper,
    #[serde(default, skip_serializing)]
    password: Option<String>,
    #[serde(skip_serializing, rename = "pin", alias = "code", default)]
    secret_pin: Option<u8>,
    #[serde(skip, default)]
    ignored: Option<u8>,
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
    unknown_member::<Person>(value).expect("the description builds")
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
fn aliases_and_write_only_members_are_accepted_and_unpublished() {
    let mut body = person();
    for name in ["oldName", "formerName", "password", "pin", "code"] {
        body[name] = json!("x");
        assert_eq!(unknown(&body), None, "{name}");
    }
    // Only those names: another spelling, the Rust name of a renamed
    // member and a `skip` member are unknown.
    for name in ["older", "secret_pin", "ignored", "Password"] {
        let mut body = person();
        body[name] = json!("x");
        assert_eq!(unknown(&body).as_deref(), Some(name), "{name}");
    }
    // `created_by` comes from the flattened struct and is listed.
    assert_eq!(unknown(&person()), None);
    // None of this reaches the document.
    let (_, components) = forge_openapi::schema_of::<Person>().expect("builds");
    let text = serde_json::to_string(&components).expect("json");
    for hidden in ["x-forge-accepts", "oldName", "password", "pin\"", "code"] {
        assert!(!text.contains(hidden), "{hidden}: {text}");
    }
}

fn check<T: OpenApiSchema + ?Sized + 'static>(value: &Value) -> Option<String> {
    unknown_member::<T>(value).expect("the description builds")
}

#[test]
fn types_that_are_not_objects_have_nothing_to_refuse() {
    assert_eq!(check::<String>(&json!("x")), None);
    assert_eq!(check::<Vec<Address>>(&json!([{"city": "x"}])), None);
    assert_eq!(
        check::<Vec<Address>>(&json!([{"city": "x", "k": 1}])).as_deref(),
        Some("[0].k")
    );
    assert_eq!(check::<Value>(&json!({"a": 1})), None);
    assert_eq!(
        check::<BTreeMap<String, u8>>(&json!({"a": 1, "b": 2})),
        None
    );
}

#[derive(Deserialize, OpenApiSchema)]
#[openapi(crate = "forge_openapi")]
struct Tree {
    name: String,
    next: Option<Box<Tree>>,
}

/// `depth` nested `next` objects, the last with `extra: 1`.
fn tree(depth: usize) -> Value {
    let mut value = json!({"name": "leaf", "next": null, "extra": 1});
    for _ in 0..depth {
        value = json!({"name": "n", "next": value});
    }
    value
}

#[test]
fn a_nullable_recursive_type_is_checked_as_deep_as_json_goes() {
    // serde_json parses 128 levels: each level costs the walk one JSON step,
    // however many schema steps (`$ref`, `anyOf`) it takes.
    let found = check::<Tree>(&tree(100)).expect("unknown member");
    assert!(found.ends_with("next.extra"), "{found}");
    assert_eq!(found.matches("next").count(), 100);
    let mut fine = tree(100);
    let mut cursor = &mut fine;
    while !cursor["next"].is_null() {
        cursor = &mut cursor["next"];
    }
    cursor.as_object_mut().map(|leaf| leaf.remove("extra"));
    assert_eq!(check::<Tree>(&fine), None);
}

#[test]
fn a_body_nested_beyond_the_limit_is_refused_not_waved_through() {
    // Past 160 levels the check stops and reports where, instead of
    // accepting what it did not look at.
    assert!(check::<Tree>(&tree(200)).is_some());
}

/// `anyOf` written by hand: a string, or an object with `a`, or one with `b`.
struct Either;

impl OpenApiSchema for Either {
    fn schema(_generator: &mut forge_openapi::SchemaGenerator) -> forge_openapi::Schema {
        json!({"anyOf": [
            {"type": "string"},
            {"type": "object", "properties": {"a": {}}, "additionalProperties": false},
            {"type": "object", "properties": {"b": {}}, "additionalProperties": false},
        ]})
    }
}

#[test]
fn any_of_accepts_a_value_when_any_matching_branch_does() {
    assert_eq!(check::<Either>(&json!("text")), None);
    assert_eq!(check::<Either>(&json!({"a": 1})), None);
    assert_eq!(check::<Either>(&json!({"b": 1})), None);
    assert_eq!(check::<Either>(&json!({"c": 1})).as_deref(), Some("c"));
    // No branch is for this kind of value: serde reports the type error.
    assert_eq!(check::<Either>(&json!(7)), None);
}

#[test]
fn a_nullable_object_is_checked_unless_the_value_is_null() {
    assert_eq!(check::<Option<Address>>(&json!(null)), None);
    assert_eq!(check::<Option<Address>>(&json!({"city": "x"})), None);
    assert_eq!(
        check::<Option<Address>>(&json!({"city": "x", "k": 1})).as_deref(),
        Some("k")
    );
}

struct Broken;

impl OpenApiSchema for Broken {
    fn schema(generator: &mut forge_openapi::SchemaGenerator) -> forge_openapi::Schema {
        generator.fail("this type cannot be described");
        Value::Null
    }
}

#[test]
fn a_description_that_cannot_be_built_is_an_error_not_a_pass() {
    let error = unknown_member::<Broken>(&json!({"anything": 1})).expect_err("fails closed");
    assert!(error.contains("cannot be described"), "{error}");
}

#[derive(Deserialize, OpenApiSchema)]
#[openapi(crate = "forge_openapi")]
struct Aliased {
    #[serde(alias = "addr", alias = "location")]
    address: Address,
    #[serde(alias = "all")]
    visits: Vec<Address>,
}

#[test]
fn a_value_under_an_alias_is_checked_like_its_member() {
    let ok = json!({"addr": {"city": "x"}, "all": [{"city": "y"}]});
    assert_eq!(check::<Aliased>(&ok), None);
    assert_eq!(
        check::<Aliased>(&json!({"addr": {"city": "x", "evil": 1}, "visits": []})).as_deref(),
        Some("addr.evil")
    );
    assert_eq!(
        check::<Aliased>(&json!({"location": {"evil": 1}, "visits": []})).as_deref(),
        Some("location.evil")
    );
    assert_eq!(
        check::<Aliased>(&json!({"address": {"city": "x"}, "all": [{"city": "y", "k": 1}]}))
            .as_deref(),
        Some("all[0].k")
    );
}

/// Two object branches that share a recursive member: an `anyOf` walk that
/// retried the whole subtree under each branch would take 2^depth steps.
struct Expr;

impl OpenApiSchema for Expr {
    fn schema(generator: &mut forge_openapi::SchemaGenerator) -> forge_openapi::Schema {
        generator.component::<Expr>(None, |generator| {
            let inner = Expr::schema(generator);
            json!({"anyOf": [
                {"type": "object", "properties": {"l": inner.clone()}, "additionalProperties": false},
                {"type": "object", "properties": {"l": inner, "r": {}}, "additionalProperties": false},
            ]})
        })
    }
}

fn chain(depth: usize, leaf: Value) -> Value {
    let mut value = leaf;
    for _ in 0..depth {
        value = json!({ "l": value });
    }
    value
}

#[test]
fn any_of_branches_sharing_a_recursive_member_do_not_blow_up() {
    let started = std::time::Instant::now();
    // Valid: the first branch accepts at once.
    assert_eq!(check::<Expr>(&chain(60, json!({}))), None);
    // Invalid at the bottom: every branch fails, at every level. The budget
    // ends the walk, closed (the body is refused), instead of 2^60 steps.
    assert!(check::<Expr>(&chain(60, json!({"zz": 1}))).is_some());
    assert!(check::<Expr>(&chain(40, json!({"zz": 1}))).is_some());
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
}
