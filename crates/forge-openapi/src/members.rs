//! Unknown members: the check behind `additionalProperties: false`.
//!
//! The document says an object lists its members and no others, while serde
//! ignores unknown members unless the type says `deny_unknown_fields`. A
//! typed handler enforces what the document publishes: it asks which member
//! of a request body the type's own schema does not list.

use std::any::TypeId;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, PoisonError};

use serde_json::Value;

use crate::schema::{OpenApiSchema, REF_PLACEHOLDER, SchemaGenerator};

/// The schema of one type with the components it refers to, in the form the
/// generator collects them (references still point at placeholders).
struct Shape {
    root: Value,
    components: Vec<Value>,
}

static SHAPES: LazyLock<Mutex<HashMap<TypeId, Arc<Shape>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Deeper values than serde_json parses (128) are not walked.
const MAX_DEPTH: usize = 160;

fn shape_of<T: OpenApiSchema + ?Sized + 'static>() -> Arc<Shape> {
    let id = TypeId::of::<T>();
    if let Some(shape) = SHAPES
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&id)
    {
        return Arc::clone(shape);
    }
    let mut generator = SchemaGenerator::new();
    let root = T::schema(&mut generator);
    // A description that cannot be built stops the document from building;
    // here it only means nothing can be enforced.
    let shape = Arc::new(if generator.take_error().is_some() {
        Shape {
            root: Value::Null,
            components: Vec::new(),
        }
    } else {
        Shape {
            root,
            components: generator
                .components
                .iter()
                .map(|component| component.schema.clone())
                .collect(),
        }
    });
    SHAPES
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(id, Arc::clone(&shape));
    shape
}

/// The first member of `value` (a decoded request body) that the schema of
/// `T` does not list, as a path such as `address.street` or `items[2].sku`,
/// or `None` when every member is known.
///
/// It follows what the document publishes: objects list their members and
/// no others (`additionalProperties: false`), maps and `serde_json::Value`
/// accept any, and a `serde(alias)` is not a listed member.
#[must_use]
pub fn unknown_member<T: OpenApiSchema + ?Sized + 'static>(value: &Value) -> Option<String> {
    let shape = shape_of::<T>();
    walk(&shape, &shape.root, value, "", 0)
}

fn resolve<'a>(shape: &'a Shape, schema: &'a Value) -> &'a Value {
    schema
        .get("$ref")
        .and_then(Value::as_str)
        .and_then(|target| target.strip_prefix(REF_PLACEHOLDER))
        .and_then(|index| index.parse::<usize>().ok())
        .and_then(|index| shape.components.get(index))
        .unwrap_or(schema)
}

fn walk(shape: &Shape, schema: &Value, value: &Value, at: &str, depth: usize) -> Option<String> {
    if depth > MAX_DEPTH {
        return None;
    }
    let schema = resolve(shape, schema);
    if let Some(branches) = schema.get("anyOf").and_then(Value::as_array) {
        // A nullable reference: `anyOf [schema, {"type": "null"}]`.
        return branches
            .iter()
            .find(|branch| branch.get("type") != Some(&Value::from("null")))
            .filter(|_| !value.is_null())
            .and_then(|branch| walk(shape, branch, value, at, depth + 1));
    }
    match value {
        Value::Object(members) => {
            let properties = schema.get("properties").and_then(Value::as_object);
            let additional = schema.get("additionalProperties");
            for (name, member) in members {
                let path = if at.is_empty() {
                    name.clone()
                } else {
                    format!("{at}.{name}")
                };
                let expected = match (properties.and_then(|listed| listed.get(name)), additional) {
                    (Some(listed), _) => listed,
                    (None, Some(Value::Bool(false))) => return Some(path),
                    (None, Some(map @ Value::Object(_))) => map,
                    (None, _) => continue,
                };
                if let Some(found) = walk(shape, expected, member, &path, depth + 1) {
                    return Some(found);
                }
            }
            None
        }
        Value::Array(items) => {
            let expected = schema.get("items")?;
            items.iter().enumerate().find_map(|(index, item)| {
                walk(shape, expected, item, &format!("{at}[{index}]"), depth + 1)
            })
        }
        _ => None,
    }
}
