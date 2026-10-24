//! Unknown members: the check behind `additionalProperties: false`.
//!
//! The document says an object lists its members and no others, while serde
//! ignores unknown members unless the type says `deny_unknown_fields`. A
//! typed handler enforces what the document publishes: it asks which member
//! of a request body the type's own schema does not list.

use std::any::TypeId;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, PoisonError, RwLock};

use serde_json::Value;

use crate::schema::{ACCEPTS_KEY, ALIASES_KEY, OpenApiSchema, REF_PLACEHOLDER, SchemaGenerator};

/// The schema of one type with the components it refers to, in the form the
/// generator collects them (references still point at placeholders).
struct Shape {
    root: Value,
    components: Vec<Value>,
    /// Why the schema cannot be built: nothing can then be checked, and the
    /// caller must not pretend it was.
    error: Option<String>,
}

static SHAPES: LazyLock<RwLock<HashMap<TypeId, Arc<Shape>>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// Deeper JSON than serde_json parses (128) is refused, not walked.
const MAX_DEPTH: usize = 160;
/// Schema hops (`$ref`, `anyOf`) in a row without descending into the value.
const MAX_HOPS: usize = 64;
/// Steps the walk may take at least, however small the body: the work of a
/// walk is otherwise bounded by the body only for simple schemas. `anyOf`
/// tries one branch after another, and branches that share a recursive member
/// would walk the same subtree again and again (exponentially in the depth),
/// so the walk has a budget of `max(10_000, 4 x values in the body)` steps and
/// is refused, not waved through, when it runs out.
const MIN_BUDGET: usize = 10_000;

fn count_values(value: &Value) -> usize {
    match value {
        Value::Object(members) => 1 + members.values().map(count_values).sum::<usize>(),
        Value::Array(items) => 1 + items.iter().map(count_values).sum::<usize>(),
        _ => 1,
    }
}

fn shape_of<T: OpenApiSchema + ?Sized + 'static>() -> Arc<Shape> {
    let id = TypeId::of::<T>();
    if let Some(shape) = SHAPES
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&id)
    {
        return Arc::clone(shape);
    }
    let mut generator = SchemaGenerator::new();
    let root = T::schema(&mut generator);
    let shape = Arc::new(match generator.take_error() {
        Some(error) => Shape {
            root: Value::Null,
            components: Vec::new(),
            error: Some(error),
        },
        None => Shape {
            root,
            components: generator
                .components
                .iter()
                .map(|component| component.schema.clone())
                .collect(),
            error: None,
        },
    });
    SHAPES
        .write()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(id, Arc::clone(&shape));
    shape
}

/// The first member of `value` (a decoded request body) that the schema of
/// `T` does not list, as a path such as `address.street` or `items[2].sku`,
/// or `Ok(None)` when every member is known. `Err` carries the reason the
/// description of `T` cannot be built (the document cannot be built either):
/// nothing can be checked, and callers should refuse the request.
///
/// It follows what the document publishes: objects list their members and
/// no others (`additionalProperties: false`), maps and `serde_json::Value`
/// accept any. Besides the listed members, an object accepts the names its
/// derive marked as read but never written (`skip_serializing`), whose values
/// are not checked, and `serde(alias)`es, whose values are checked like the
/// member they stand for. The work is bounded (see the budget below): a body
/// that exhausts it is reported as unknown at the point it stopped. Of the combinators,
/// `anyOf` and `oneOf` accept a value when any branch of a matching `type`
/// does; `allOf` and the other keywords are not interpreted (nothing is
/// refused because of them). A value nested deeper than 160 levels is
/// reported as unknown at the level where the walk stopped.
pub fn unknown_member<T: OpenApiSchema + ?Sized + 'static>(
    value: &Value,
) -> Result<Option<String>, String> {
    let shape = shape_of::<T>();
    if let Some(error) = &shape.error {
        return Err(error.clone());
    }
    let mut budget = MIN_BUDGET.max(4 * count_values(value));
    Ok(walk(&shape, &shape.root, value, "", 0, 0, &mut budget))
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

/// Whether the `type` of `schema` (if it has one) admits `value`'s kind.
fn kind_matches(shape: &Shape, schema: &Value, value: &Value) -> bool {
    let schema = resolve(shape, schema);
    let admits = |kind: &str| match kind {
        "null" => value.is_null(),
        "boolean" => value.is_boolean(),
        "integer" => value.is_i64() || value.is_u64(),
        "number" => value.is_number(),
        "string" => value.is_string(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        _ => true,
    };
    match schema.get("type") {
        Some(Value::String(kind)) => admits(kind),
        Some(Value::Array(kinds)) => kinds.iter().filter_map(Value::as_str).any(admits),
        _ => true,
    }
}

/// `path` of the first unknown member of `value` under `schema`; `depth`
/// counts JSON levels, `hops` schema steps taken without descending, and
/// `budget` the steps left (see [`MIN_BUDGET`]).
fn walk(
    shape: &Shape,
    schema: &Value,
    value: &Value,
    at: &str,
    depth: usize,
    hops: usize,
    budget: &mut usize,
) -> Option<String> {
    if depth > MAX_DEPTH || hops > MAX_HOPS || *budget == 0 {
        return Some(at.to_owned());
    }
    *budget -= 1;
    let schema = resolve(shape, schema);
    for keyword in ["anyOf", "oneOf"] {
        let Some(branches) = schema.get(keyword).and_then(Value::as_array) else {
            continue;
        };
        // The nullable form `[schema, {"type": "null"}]`.
        if let [inner, null] = branches.as_slice()
            && null.get("type") == Some(&Value::from("null"))
        {
            return if value.is_null() {
                None
            } else {
                walk(shape, inner, value, at, depth, hops + 1, budget)
            };
        }
        let mut first = None;
        let mut applicable = false;
        for branch in branches {
            if !kind_matches(shape, branch, value) {
                continue;
            }
            applicable = true;
            match walk(shape, branch, value, at, depth, hops + 1, budget) {
                None => return None,
                found => first = first.or(found),
            }
        }
        return if applicable { first } else { None };
    }
    match value {
        Value::Object(members) => {
            let properties = schema.get("properties").and_then(Value::as_object);
            let accepted = schema.get(ACCEPTS_KEY).and_then(Value::as_array);
            let aliases = schema.get(ALIASES_KEY).and_then(Value::as_object);
            let additional = schema.get("additionalProperties");
            for (name, member) in members {
                let path = if at.is_empty() {
                    name.clone()
                } else {
                    format!("{at}.{name}")
                };
                if accepted.is_some_and(|names| names.iter().any(|accepted| accepted == name)) {
                    continue;
                }
                // An alias is the member it stands for: its value is checked
                // with that member's schema.
                let canonical = aliases
                    .and_then(|aliases| aliases.get(name))
                    .and_then(Value::as_str)
                    .and_then(|member| properties.and_then(|listed| listed.get(member)));
                let listed = properties.and_then(|listed| listed.get(name)).or(canonical);
                let expected = match (listed, additional) {
                    (Some(listed), _) => listed,
                    (None, Some(Value::Bool(false))) => return Some(path),
                    (None, Some(map @ Value::Object(_))) => map,
                    (None, _) => continue,
                };
                if let Some(found) = walk(shape, expected, member, &path, depth + 1, 0, budget) {
                    return Some(found);
                }
            }
            None
        }
        Value::Array(items) => {
            let expected = schema.get("items")?;
            items.iter().enumerate().find_map(|(index, item)| {
                walk(
                    shape,
                    expected,
                    item,
                    &format!("{at}[{index}]"),
                    depth + 1,
                    0,
                    budget,
                )
            })
        }
        _ => None,
    }
}
