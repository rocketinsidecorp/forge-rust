//! `unknown_member` over `anyOf` and `oneOf`: which branches a value's kind
//! admits, and what is reported when several do.

use forge_openapi::{OpenApiSchema, Schema, SchemaGenerator, unknown_member};
use serde_json::{Value, json};

struct Mixed;

impl OpenApiSchema for Mixed {
    fn schema(_generator: &mut SchemaGenerator) -> Schema {
        json!({"anyOf": [
            {"type": "string"},
            {"type": "integer"},
            {"type": "number"},
            {"type": "boolean"},
            {"type": "null"},
            {"type": "array", "items": {
                "type": "object",
                "properties": {"a": {"type": "string"}},
                "additionalProperties": false,
            }},
            {"type": "object", "properties": {"id": {"type": "integer"}}, "additionalProperties": false},
        ]})
    }
}

struct Either;

impl OpenApiSchema for Either {
    fn schema(_generator: &mut SchemaGenerator) -> Schema {
        json!({"oneOf": [
            {"type": ["object", "null"], "properties": {"id": {"type": "integer"}}, "additionalProperties": false},
            {"type": "weird", "properties": {"other": {"type": "integer"}}},
        ]})
    }
}

struct OnlyObjects;

impl OpenApiSchema for OnlyObjects {
    fn schema(_generator: &mut SchemaGenerator) -> Schema {
        json!({"oneOf": [
            {"type": "object", "properties": {"id": {"type": "integer"}}, "additionalProperties": false},
        ]})
    }
}

fn unknown<T: OpenApiSchema + 'static>(value: &Value) -> Option<String> {
    unknown_member::<T>(value).expect("describable")
}

#[test]
fn a_value_is_checked_by_the_branches_its_kind_admits() {
    for value in [
        json!("text"),
        json!(7),
        json!(1.5),
        json!(true),
        json!(null),
    ] {
        assert_eq!(unknown::<Mixed>(&value), None, "{value}");
    }
    assert_eq!(unknown::<Mixed>(&json!({"id": 1})), None);
    assert_eq!(
        unknown::<Mixed>(&json!({"id": 1, "extra": 2})),
        Some("extra".to_owned())
    );
    assert_eq!(unknown::<Mixed>(&json!([])), None);
    assert_eq!(unknown::<Mixed>(&json!([{"a": "x"}, {"a": "y"}])), None);
    assert_eq!(
        unknown::<Mixed>(&json!([{"a": "x"}, {"a": "y", "b": 1}])),
        Some("[1].b".to_owned())
    );
}

#[test]
fn branches_of_a_type_the_validator_does_not_know_admit_everything() {
    // `type: weird` is not a JSON type, so that branch is tried for any value
    // and the first branch that finds nothing wrong wins.
    assert_eq!(unknown::<Either>(&json!({"other": 1})), None);
    assert_eq!(unknown::<Either>(&json!({"id": 1})), None);
    assert_eq!(unknown::<Either>(&json!(null)), None);
    assert_eq!(unknown::<Either>(&json!({"id": 1, "other": 2})), None);
    assert_eq!(unknown::<Either>(&json!(5)), None);
}

#[test]
fn a_value_no_branch_admits_is_left_to_the_decoder() {
    assert_eq!(unknown::<OnlyObjects>(&json!(5)), None);
    assert_eq!(unknown::<OnlyObjects>(&json!("text")), None);
    assert_eq!(unknown::<OnlyObjects>(&json!([1, 2])), None);
    assert_eq!(
        unknown::<OnlyObjects>(&json!({"id": 1, "extra": 1})),
        Some("extra".to_owned())
    );
}
