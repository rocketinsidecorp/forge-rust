//! The schema of every standard type, the combinators that edit schemas and
//! the object builder's refusals.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, LinkedList, VecDeque};
use std::rc::Rc;
use std::sync::Arc;

use forge_core::{ErrorReport, Id, Timestamp, Uuid, Version};
use forge_openapi::{
    Field, ObjectSchema, OpenApiSchema, Schema, SchemaGenerator, UUID_V7_PATTERN, non_null,
    null_schema, nullable, schema_of, string_enum,
};
use serde_json::{Map, Value, json};

fn schema<T: OpenApiSchema + ?Sized>() -> Value {
    schema_of::<T>().expect("describable").0
}

#[test]
fn numbers_say_their_format_and_range() {
    for (actual, expected) in [
        (schema::<bool>(), json!({"type": "boolean"})),
        (
            schema::<i8>(),
            json!({"type": "integer", "format": "int32", "minimum": -128, "maximum": 127}),
        ),
        (
            schema::<i16>(),
            json!({"type": "integer", "format": "int32", "minimum": -32768, "maximum": 32767}),
        ),
        (
            schema::<i32>(),
            json!({"type": "integer", "format": "int32"}),
        ),
        (
            schema::<i64>(),
            json!({"type": "integer", "format": "int64"}),
        ),
        (
            schema::<isize>(),
            json!({"type": "integer", "format": "int64"}),
        ),
        (
            schema::<u8>(),
            json!({"type": "integer", "format": "int32", "minimum": 0, "maximum": 255}),
        ),
        (
            schema::<u16>(),
            json!({"type": "integer", "format": "int32", "minimum": 0, "maximum": 65535}),
        ),
        (
            schema::<u32>(),
            json!({"type": "integer", "format": "int64", "minimum": 0}),
        ),
        (
            schema::<u64>(),
            json!({"type": "integer", "format": "int64", "minimum": 0}),
        ),
        (
            schema::<usize>(),
            json!({"type": "integer", "format": "int64", "minimum": 0}),
        ),
        (
            schema::<f32>(),
            json!({"type": "number", "format": "float"}),
        ),
        (
            schema::<f64>(),
            json!({"type": "number", "format": "double"}),
        ),
    ] {
        assert_eq!(actual, expected);
    }
}

#[test]
fn text_unit_and_free_form_values() {
    assert_eq!(schema::<String>(), json!({"type": "string"}));
    assert_eq!(schema::<str>(), json!({"type": "string"}));
    assert_eq!(
        schema::<char>(),
        json!({"type": "string", "minLength": 1, "maxLength": 1})
    );
    assert_eq!(schema::<()>(), json!({"type": "null"}));
    assert_eq!(schema::<Value>(), json!({}));
    assert_eq!(
        schema::<Map<String, Value>>(),
        json!({"type": "object", "additionalProperties": {}})
    );
    assert_eq!(null_schema(), json!({"type": "null"}));
}

#[test]
fn forge_core_types_describe_themselves() {
    assert_eq!(
        schema::<Uuid>(),
        json!({"type": "string", "format": "uuid"})
    );
    assert_eq!(
        schema::<Timestamp>(),
        json!({"type": "string", "format": "date-time"})
    );
    assert_eq!(
        schema::<Version>(),
        json!({"type": "integer", "format": "int64", "minimum": 1})
    );
    struct Marker;
    assert_eq!(
        schema::<Id<Marker>>(),
        json!({
            "type": "string",
            "format": "uuid",
            "pattern": UUID_V7_PATTERN,
            "description": "UUIDv7 (RFC 9562)",
        })
    );
    let (reference, components) = schema_of::<ErrorReport<'_>>().expect("describable");
    assert_eq!(
        reference,
        json!({"$ref": "#/components/schemas/ErrorReport"})
    );
    let report = &components["ErrorReport"];
    assert_eq!(
        report["required"],
        json!(["category", "code", "message", "retryable"])
    );
    assert_eq!(report["additionalProperties"], false);
    assert_eq!(report["properties"]["metadata"]["type"], "object");
    assert!(!report["required"].to_string().contains("metadata"));
}

#[test]
fn wrappers_and_references_are_transparent() {
    assert_eq!(schema::<Box<String>>(), schema::<String>());
    assert_eq!(schema::<Arc<u8>>(), schema::<u8>());
    assert_eq!(schema::<Rc<str>>(), schema::<str>());
    assert_eq!(schema::<&u8>(), schema::<u8>());
    assert_eq!(schema::<Cow<'_, str>>(), schema::<str>());
    assert_eq!(schema::<Cow<'_, [u8]>>(), schema::<[u8]>());
}

#[test]
fn options_are_nullable() {
    assert_eq!(
        schema::<Option<String>>(),
        json!({"type": ["string", "null"]})
    );
    assert_eq!(schema::<Option<Option<u8>>>(), schema::<Option<u8>>());
    assert_eq!(schema::<Option<Value>>(), json!({}));
}

#[test]
fn sequences_are_arrays_and_arrays_have_their_length() {
    let strings = json!({"type": "array", "items": {"type": "string"}});
    assert_eq!(schema::<Vec<String>>(), strings);
    assert_eq!(schema::<VecDeque<String>>(), strings);
    assert_eq!(schema::<LinkedList<String>>(), strings);
    assert_eq!(schema::<BTreeSet<String>>(), strings);
    assert_eq!(schema::<HashSet<String>>(), strings);
    assert_eq!(schema::<[String]>(), strings);
    assert_eq!(
        schema::<[String; 3]>(),
        json!({"type": "array", "items": {"type": "string"}, "minItems": 3, "maxItems": 3})
    );
}

#[test]
fn maps_are_objects_keyed_by_text() {
    let counts = json!({"type": "object", "additionalProperties": {"type": "integer", "format": "int64", "minimum": 0}});
    assert_eq!(schema::<BTreeMap<String, u64>>(), counts);
    assert_eq!(schema::<HashMap<Uuid, u64>>(), counts);
    assert_eq!(schema::<BTreeMap<char, u64>>(), counts);
    assert_eq!(schema::<HashMap<i32, u64>>(), counts);
    assert_eq!(schema::<HashMap<&str, u64>>(), counts);
    assert_eq!(schema::<HashMap<Cow<'static, str>, u64>>(), counts);
    assert_eq!(schema::<HashMap<Box<str>, u64>>(), counts);
    assert_eq!(schema::<HashMap<Arc<str>, u64>>(), counts);
    struct Marker;
    assert_eq!(schema::<BTreeMap<Id<Marker>, u64>>(), counts);
}

#[test]
fn nullable_adds_null_in_the_form_the_schema_allows() {
    assert_eq!(
        nullable(json!({"type": "string"})),
        json!({"type": ["string", "null"]})
    );
    assert_eq!(
        nullable(json!({"type": ["string", "null"]})),
        json!({"type": ["string", "null"]})
    );
    assert_eq!(
        nullable(json!({"type": ["integer", "string"]})),
        json!({"type": ["integer", "string", "null"]})
    );
    assert_eq!(nullable(json!({})), json!({}), "any value, null included");
    assert_eq!(nullable(json!(true)), json!(true), "not an object schema");
    let reference = json!({"$ref": "#/components/schemas/Post"});
    assert_eq!(
        nullable(reference.clone()),
        json!({"anyOf": [reference, {"type": "null"}]})
    );
    let already = json!({"anyOf": [reference, {"type": "null"}]});
    assert_eq!(nullable(already.clone()), already);
    let other = json!({"anyOf": [{"type": "string"}, {"type": "integer"}]});
    assert_eq!(
        nullable(other.clone()),
        json!({"anyOf": [other, {"type": "null"}]})
    );
}

#[test]
fn non_null_removes_null() {
    assert_eq!(
        non_null(json!({"type": ["string", "null"]})),
        json!({"type": "string"})
    );
    assert_eq!(
        non_null(json!({"type": ["integer", "string", "null"]})),
        json!({"type": ["integer", "string"]})
    );
    assert_eq!(
        non_null(json!({"type": "string"})),
        json!({"type": "string"})
    );
    let reference = json!({"$ref": "#/components/schemas/Post"});
    assert_eq!(
        non_null(json!({"anyOf": [reference.clone(), {"type": "null"}]})),
        reference
    );
    let three = json!({"anyOf": [{"type": "string"}, {"type": "integer"}, {"type": "null"}]});
    assert_eq!(
        non_null(three.clone()),
        three,
        "only the two-branch nullable form is undone"
    );
    assert_eq!(non_null(json!({})), json!({}));
}

#[test]
fn string_enums_list_their_values_and_may_carry_a_description() {
    assert_eq!(
        string_enum(&["draft", "live"], None),
        json!({"type": "string", "enum": ["draft", "live"]})
    );
    assert_eq!(
        string_enum(&["draft"], Some("Where a post is.")),
        json!({"type": "string", "enum": ["draft"], "description": "Where a post is."})
    );
}

/// A hand-written object using every builder option.
struct Handwritten;

impl OpenApiSchema for Handwritten {
    fn schema(generator: &mut SchemaGenerator) -> Schema {
        generator.component::<Self>(Some("Handwritten"), |generator| {
            let mut object = ObjectSchema::new();
            object
                .description("Written by hand.")
                .field::<String>(
                    generator,
                    "name",
                    Field::new()
                        .doc("The name.")
                        .example("ada")
                        .alias("nick")
                        .alias("handle"),
                )
                .field::<Option<String>>(generator, "bio", Field::new().optional())
                .field::<Option<String>>(generator, "email", Field::new().format("email"))
                .field::<Option<Vec<u8>>>(
                    generator,
                    "bytes",
                    Field::new().nonnull().example("[1, 2]"),
                )
                .field::<Option<Handwritten>>(generator, "friend", Field::new().format("self"))
                .field::<String>(generator, "never", Field::new().optional().required())
                .accept(["password", "token"]);
            object.into_schema()
        })
    }
}

#[test]
fn the_object_builder_follows_its_options() {
    let (reference, components) = schema_of::<Handwritten>().expect("describable");
    assert_eq!(
        reference,
        json!({"$ref": "#/components/schemas/Handwritten"})
    );
    let object = &components["Handwritten"];
    assert_eq!(object["description"], "Written by hand.");
    assert_eq!(object["additionalProperties"], false);
    assert_eq!(
        object["required"],
        json!(["bytes", "email", "friend", "name", "never"])
    );
    let properties = &object["properties"];
    assert_eq!(
        properties["name"],
        json!({"type": "string", "description": "The name.", "examples": ["ada"]})
    );
    assert_eq!(properties["bio"], json!({"type": ["string", "null"]}));
    assert_eq!(
        properties["email"],
        json!({"type": ["string", "null"], "format": "email"})
    );
    assert_eq!(
        properties["bytes"],
        json!({"type": "array", "items": {"type": "integer", "format": "int32", "minimum": 0, "maximum": 255}, "examples": [[1, 2]]})
    );
    // The format of a nullable reference goes to the reference's branch.
    assert_eq!(
        properties["friend"],
        json!({"anyOf": [{"$ref": "#/components/schemas/Handwritten", "format": "self"}, {"type": "null"}]})
    );
    // What a request may carry but the document does not publish is internal.
    assert!(object.get("x-forge-accepts").is_none(), "{object}");
    assert!(object.get("x-forge-aliases").is_none(), "{object}");
}

macro_rules! failing_type {
    ($name:ident, |$generator:ident| $body:block) => {
        struct $name;

        impl OpenApiSchema for $name {
            fn schema($generator: &mut SchemaGenerator) -> Schema {
                $body
            }
        }
    };
}

failing_type!(WrittenTwice, |generator| {
    let mut object = ObjectSchema::new();
    object
        .field::<u8>(generator, "id", Field::new())
        .field::<String>(generator, "id", Field::new());
    object.into_schema()
});

failing_type!(FlattenedTwice, |generator| {
    let mut object = ObjectSchema::new();
    object
        .field::<u8>(generator, "k", Field::new())
        .flatten::<BTreeMap<String, u8>>(generator, "first")
        .flatten::<BTreeMap<String, u8>>(generator, "second");
    object.into_schema()
});

failing_type!(FlattenedNotAnObject, |generator| {
    let mut object = ObjectSchema::new();
    object.flatten::<u8>(generator, "count");
    object.into_schema()
});

failing_type!(Described, |generator| {
    generator.fail("first reason");
    generator.fail("second reason");
    Schema::Null
});

/// Members flattened from a type that also has a member of the same name.
struct Inner;

impl OpenApiSchema for Inner {
    fn schema(generator: &mut SchemaGenerator) -> Schema {
        let mut object = ObjectSchema::new();
        object.field::<u8>(generator, "id", Field::new());
        object.into_schema()
    }

    fn flatten(generator: &mut SchemaGenerator) -> Option<ObjectSchema> {
        let mut object = ObjectSchema::new();
        object.field::<u8>(generator, "id", Field::new());
        Some(object)
    }
}

failing_type!(FlattenedDuplicate, |generator| {
    let mut object = ObjectSchema::new();
    object
        .field::<u8>(generator, "id", Field::new())
        .flatten::<Inner>(generator, "inner");
    object.into_schema()
});

#[test]
fn the_object_builder_refuses_what_it_cannot_express() {
    for (error, expected) in [
        (
            schema_of::<WrittenTwice>().err(),
            "the member \"id\" is written twice",
        ),
        (
            schema_of::<FlattenedTwice>().err(),
            "two flattened maps collect the other members",
        ),
        (
            schema_of::<FlattenedNotAnObject>().err(),
            "the flattened member \"count\"",
        ),
        (
            schema_of::<FlattenedDuplicate>().err(),
            "written twice (flattened from \"inner\")",
        ),
        (schema_of::<Described>().err(), "first reason"),
    ] {
        let message = error.map(|error| error.to_string()).unwrap_or_default();
        assert!(message.contains(expected), "{expected}: {message}");
        assert!(!message.contains("second reason"), "{message}");
    }
}

#[test]
fn a_flattened_option_contributes_optional_members() {
    let (reference, components) = {
        struct Holder;
        impl OpenApiSchema for Holder {
            fn schema(generator: &mut SchemaGenerator) -> Schema {
                generator.component::<Self>(Some("Holder"), |generator| {
                    let mut object = ObjectSchema::new();
                    object
                        .field::<u8>(generator, "own", Field::new())
                        .flatten_optional::<Inner>(generator, "inner");
                    object.into_schema()
                })
            }
        }
        schema_of::<Holder>().expect("describable")
    };
    assert_eq!(reference, json!({"$ref": "#/components/schemas/Holder"}));
    assert_eq!(components["Holder"]["required"], json!(["own"]));
    assert!(components["Holder"]["properties"]["id"].is_object());
}

#[test]
fn nested_flattening_is_bounded() {
    struct Deep;
    impl OpenApiSchema for Deep {
        fn schema(generator: &mut SchemaGenerator) -> Schema {
            generator.subschema::<Self>()
        }
    }
    let message = schema_of::<Deep>()
        .err()
        .map(|error| error.to_string())
        .unwrap_or_default();
    assert!(message.contains("contains itself"), "{message}");
}
