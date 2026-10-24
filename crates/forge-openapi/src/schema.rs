//! Rust types as JSON Schema 2020-12, following what `serde_json` writes.

use std::any::{TypeId, type_name};
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, LinkedList, VecDeque};
use std::rc::Rc;
use std::sync::Arc;

use forge_core::{ErrorReport, Id, Timestamp, Uuid, Version};
use serde_json::{Map, Value, json};

/// A JSON Schema (2020-12) value, as it appears in the document.
pub type Schema = Value;

/// The canonical (lowercase) text of a UUIDv7, which is what
/// [`forge_core::Id`] writes.
pub const UUID_V7_PATTERN: &str =
    "^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$";

/// How many newtypes, `as` targets and flattened members may nest inside one
/// another before the description is refused: a type that refers back to
/// itself through them (`struct A(Option<Box<A>>)`, or a flattened
/// `Option<Box<Self>>`) has no named object for a `$ref` to end at.
const MAX_NESTING: usize = 64;

/// Where an object schema keeps the member names a request may use without
/// the document listing them (`skip_serializing` members, `alias`es). It is
/// read by `unknown_member` and removed when the document is built, so it is
/// never published.
pub(crate) const ACCEPTS_KEY: &str = "x-forge-accepts";

/// The `$ref` text of a component before names are assigned; rewritten when
/// the document is built, so names do not depend on the order types are met.
pub(crate) const REF_PLACEHOLDER: &str = "\u{0}forge-component:";

/// A type whose JSON encoding the document can describe.
///
/// Derive it with `#[derive(OpenApiSchema)]` next to `Serialize`: the derive
/// follows serde's attributes. Implement it by hand for a type that encodes
/// itself differently from its structure (the counterpart of Forge-Go's
/// `openapi.Typer`), usually by delegating to a type that encodes the same
/// way:
///
/// ```
/// use forge_openapi::{OpenApiSchema, Schema, SchemaGenerator};
///
/// /// Written as its text, like a `String`.
/// struct Sku(String);
///
/// impl OpenApiSchema for Sku {
///     fn schema(generator: &mut SchemaGenerator) -> Schema {
///         String::schema(generator)
///     }
/// }
/// ```
pub trait OpenApiSchema {
    /// The schema of a value of this type as `serde_json` writes it. Named
    /// object types return a reference made with
    /// [`SchemaGenerator::component`].
    fn schema(generator: &mut SchemaGenerator) -> Schema;

    /// The members this type contributes to an enclosing object when it is
    /// `#[serde(flatten)]`ed, or `None` when it is not an object (a build
    /// error at the flattening member).
    fn flatten(generator: &mut SchemaGenerator) -> Option<ObjectSchema> {
        let _ = generator;
        None
    }
}

/// Collects the component schemas of a document while types describe
/// themselves. Every named object type is one component, which also ends
/// recursion: a type met again while it is being described is a reference.
#[derive(Debug, Default)]
pub struct SchemaGenerator {
    pub(crate) components: Vec<Component>,
    index: HashMap<TypeId, usize>,
    error: Option<String>,
    depth: usize,
}

#[derive(Debug)]
pub(crate) struct Component {
    pub(crate) type_name: &'static str,
    pub(crate) explicit_name: Option<String>,
    pub(crate) schema: Value,
}

impl SchemaGenerator {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// The schema of `T`. The derive calls it for newtypes and `as` targets,
    /// the places a type can contain itself without a named object in
    /// between; nesting beyond a fixed depth is a build error rather than a
    /// stack overflow.
    pub fn subschema<T: OpenApiSchema + ?Sized>(&mut self) -> Schema {
        self.nested::<T, Schema>(Schema::Null, T::schema)
    }

    /// The members `T` contributes when it is flattened, with the same
    /// recursion guard as [`SchemaGenerator::subschema`].
    pub fn flatten_of<T: OpenApiSchema + ?Sized>(&mut self) -> Option<ObjectSchema> {
        self.nested::<T, Option<ObjectSchema>>(None, T::flatten)
    }

    /// Runs `build` one level deeper, or records a build error and returns
    /// `fallback` when the nesting is too deep.
    fn nested<T: ?Sized, R>(&mut self, fallback: R, build: impl FnOnce(&mut Self) -> R) -> R {
        if self.depth >= MAX_NESTING {
            self.fail(format!(
                "{} contains itself through newtypes or flattened members, so there is no \
                 object for a reference to point at; give the recursion a named struct member \
                 (for example a field of type Option<Box<{0}>>)",
                type_name::<T>()
            ));
            return fallback;
        }
        self.depth += 1;
        let result = build(self);
        self.depth -= 1;
        result
    }

    /// Registers `T` as a component, built once by `build`, and returns a
    /// reference to it. The component is named after the Rust type (`Post`,
    /// `Page_Post` for `Page<Post>`) unless `name` is given.
    pub fn component<T: ?Sized + 'static>(
        &mut self,
        name: Option<&str>,
        build: impl FnOnce(&mut Self) -> Schema,
    ) -> Schema {
        let id = TypeId::of::<T>();
        if let Some(&index) = self.index.get(&id) {
            return reference(index);
        }
        let index = self.components.len();
        self.components.push(Component {
            type_name: type_name::<T>(),
            explicit_name: name.map(str::to_owned),
            schema: Value::Null,
        });
        self.index.insert(id, index);
        let schema = build(self);
        if let Some(component) = self.components.get_mut(index) {
            component.schema = schema;
        }
        reference(index)
    }

    /// Records a description that cannot be expressed accurately; building
    /// the document then fails with the first such message.
    pub fn fail(&mut self, message: impl Into<String>) {
        if self.error.is_none() {
            self.error = Some(message.into());
        }
    }

    pub(crate) fn take_error(&mut self) -> Option<String> {
        self.error.take()
    }
}

fn reference(index: usize) -> Value {
    json!({ "$ref": format!("{REF_PLACEHOLDER}{index}") })
}

/// How one object member is described.
#[derive(Clone, Debug, Default)]
pub struct Field {
    optional: bool,
    required: bool,
    nonnull: bool,
    doc: Option<String>,
    example: Option<String>,
    format: Option<String>,
    aliases: Vec<String>,
}

impl Field {
    /// A member that is always written (`required`).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The member may be absent (`skip_serializing_if`).
    #[must_use]
    pub fn optional(mut self) -> Self {
        self.optional = true;
        self
    }

    /// Required even if it may be absent.
    #[must_use]
    pub fn required(mut self) -> Self {
        self.required = true;
        self
    }

    /// Not nullable even if it is an `Option`.
    #[must_use]
    pub fn nonnull(mut self) -> Self {
        self.nonnull = true;
        self
    }

    /// The member's `description`.
    #[must_use]
    pub fn doc(mut self, text: impl Into<String>) -> Self {
        self.doc = Some(text.into());
        self
    }

    /// An example value: parsed as JSON when it is JSON, otherwise text.
    #[must_use]
    pub fn example(mut self, text: impl Into<String>) -> Self {
        self.example = Some(text.into());
        self
    }

    /// Another name a request may use for the member (`#[serde(alias)]`). It
    /// is not part of the document: typed decoding accepts it, nothing more.
    #[must_use]
    pub fn alias(mut self, name: impl Into<String>) -> Self {
        self.aliases.push(name.into());
        self
    }

    /// The member's `format`, such as `email`.
    #[must_use]
    pub fn format(mut self, format: impl Into<String>) -> Self {
        self.format = Some(format.into());
        self
    }

    /// Applies `nonnull`, `format`, `doc` and `example` to `schema`.
    pub(crate) fn annotate(&self, mut schema: Value) -> Value {
        if self.nonnull {
            schema = non_null(schema);
        }
        if let Some(format) = &self.format {
            insert_format(&mut schema, format);
        }
        if let Some(object) = schema.as_object_mut() {
            if let Some(doc) = &self.doc {
                object.insert("description".to_owned(), Value::String(doc.clone()));
            }
            if let Some(example) = &self.example {
                let value = serde_json::from_str(example)
                    .unwrap_or_else(|_| Value::String(example.clone()));
                object.insert("examples".to_owned(), Value::Array(vec![value]));
            }
        }
        schema
    }

    pub(crate) fn is_required(&self) -> bool {
        self.required || !self.optional
    }
}

/// An object schema under construction: the members serde writes, which of
/// them are required, and no other members (`additionalProperties: false`)
/// unless a flattened map collects them.
#[derive(Clone, Debug, Default)]
pub struct ObjectSchema {
    properties: BTreeMap<String, Value>,
    required: BTreeSet<String>,
    additional: Option<Value>,
    description: Option<String>,
    accepted: BTreeSet<String>,
}

impl ObjectSchema {
    /// An object without members.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds the member `name` of type `T`. A name given twice is a build
    /// error.
    pub fn field<T: OpenApiSchema + ?Sized>(
        &mut self,
        generator: &mut SchemaGenerator,
        name: &str,
        options: Field,
    ) -> &mut Self {
        let schema = options.annotate(T::schema(generator));
        if self.properties.contains_key(name) {
            generator.fail(format!("the member {name:?} is written twice"));
            return self;
        }
        self.properties.insert(name.to_owned(), schema);
        if options.is_required() {
            self.required.insert(name.to_owned());
        }
        self.accepted.extend(options.aliases);
        self
    }

    /// Names a request may carry although the object does not describe them:
    /// members serde reads but never writes (`skip_serializing`, such as a
    /// password) and aliases. Typed decoding accepts them (their values are
    /// not checked); the document does not list them.
    pub fn accept<'a>(&mut self, names: impl IntoIterator<Item = &'a str>) -> &mut Self {
        self.accepted.extend(names.into_iter().map(str::to_owned));
        self
    }

    /// Adds the members of `T`, as `#[serde(flatten)]` writes them. `member`
    /// names the flattened member in errors.
    pub fn flatten<T: OpenApiSchema + ?Sized>(
        &mut self,
        generator: &mut SchemaGenerator,
        member: &str,
    ) -> &mut Self {
        self.flatten_members::<T>(generator, member, false)
    }

    /// [`ObjectSchema::flatten`] for a flattened member that serde may skip
    /// as a whole (`#[serde(flatten, skip_serializing_if = "...")]`): none of
    /// its members is then written, so none of them is `required`.
    pub fn flatten_optional<T: OpenApiSchema + ?Sized>(
        &mut self,
        generator: &mut SchemaGenerator,
        member: &str,
    ) -> &mut Self {
        self.flatten_members::<T>(generator, member, true)
    }

    fn flatten_members<T: OpenApiSchema + ?Sized>(
        &mut self,
        generator: &mut SchemaGenerator,
        member: &str,
        optional: bool,
    ) -> &mut Self {
        let Some(inner) = generator.flatten_of::<T>() else {
            generator.fail(format!(
                "the flattened member {member:?} ({}) is not written as an object",
                type_name::<T>()
            ));
            return self;
        };
        let inner = if optional {
            inner.all_optional()
        } else {
            inner
        };
        for (name, schema) in inner.properties {
            if self.properties.contains_key(&name) {
                generator.fail(format!(
                    "the member {name:?} is written twice (flattened from {member:?})"
                ));
                return self;
            }
            self.properties.insert(name, schema);
        }
        self.required.extend(inner.required);
        self.accepted.extend(inner.accepted);
        if let Some(additional) = inner.additional {
            if self.additional.is_some() {
                generator.fail(format!(
                    "two flattened maps collect the other members ({member:?})"
                ));
                return self;
            }
            self.additional = Some(additional);
        }
        self
    }

    /// The object's `description`.
    pub fn description(&mut self, text: impl Into<String>) -> &mut Self {
        self.description = Some(text.into());
        self
    }

    /// The finished schema.
    #[must_use]
    pub fn into_schema(self) -> Schema {
        let mut object = Map::new();
        object.insert("type".to_owned(), json!("object"));
        object.insert(
            "properties".to_owned(),
            Value::Object(self.properties.into_iter().collect()),
        );
        object.insert(
            "additionalProperties".to_owned(),
            self.additional.unwrap_or(Value::Bool(false)),
        );
        if !self.required.is_empty() {
            object.insert(
                "required".to_owned(),
                Value::Array(self.required.into_iter().map(Value::String).collect()),
            );
        }
        if let Some(description) = self.description {
            object.insert("description".to_owned(), Value::String(description));
        }
        if !self.accepted.is_empty() {
            object.insert(
                ACCEPTS_KEY.to_owned(),
                Value::Array(self.accepted.into_iter().map(Value::String).collect()),
            );
        }
        Value::Object(object)
    }

    /// Every member optional: a flattened `Option` writes nothing when it is
    /// `None`.
    fn all_optional(mut self) -> Self {
        self.required.clear();
        self
    }
}

/// `{"type": "null"}`: what serde_json writes for `()` and unit structs.
#[must_use]
pub fn null_schema() -> Schema {
    json!({ "type": "null" })
}

/// A string limited to `values`, as serde writes a unit-only enum.
#[must_use]
pub fn string_enum(values: &[&str], description: Option<&str>) -> Schema {
    let mut schema = json!({ "type": "string", "enum": values });
    if let (Some(description), Some(object)) = (description, schema.as_object_mut()) {
        object.insert("description".to_owned(), json!(description));
    }
    schema
}

/// Adds `null` to what `schema` accepts: `["T", "null"]`, or `anyOf` for a
/// reference.
#[must_use]
pub fn nullable(mut schema: Schema) -> Schema {
    let Some(object) = schema.as_object_mut() else {
        return schema;
    };
    match object.get_mut("type") {
        Some(Value::String(kind)) => {
            let kind = std::mem::take(kind);
            object.insert("type".to_owned(), json!([kind, "null"]));
            return schema;
        }
        Some(Value::Array(kinds)) => {
            if !kinds.iter().any(|kind| kind == "null") {
                kinds.push(json!("null"));
            }
            return schema;
        }
        _ => {}
    }
    if object.is_empty() {
        return schema; // any value, null included
    }
    if let Some(Value::Array(branches)) = object.get("anyOf")
        && branches
            .iter()
            .any(|branch| branch.get("type") == Some(&json!("null")))
    {
        return schema;
    }
    json!({ "anyOf": [schema, { "type": "null" }] })
}

/// Removes `null` from what `schema` accepts.
#[must_use]
pub fn non_null(mut schema: Schema) -> Schema {
    if let Some(Value::Array(kinds)) = schema.get_mut("type") {
        kinds.retain(|kind| kind != "null");
        if kinds.len() == 1 {
            let only = kinds.remove(0);
            if let Some(object) = schema.as_object_mut() {
                object.insert("type".to_owned(), only);
            }
        }
        return schema;
    }
    if let Some(Value::Array(branches)) = schema.get("anyOf")
        && branches.len() == 2
        && branches[1].get("type") == Some(&json!("null"))
    {
        return branches[0].clone();
    }
    schema
}

/// Sets the format of the non-null branch of a nullable reference, or of the
/// schema itself.
fn insert_format(schema: &mut Value, format: &str) {
    let nullable_reference =
        matches!(schema.get("anyOf"), Some(Value::Array(branches)) if branches.len() == 2);
    let target = if nullable_reference {
        schema
            .get_mut("anyOf")
            .and_then(|branches| branches.get_mut(0))
    } else {
        Some(schema)
    };
    if let Some(Value::Object(object)) = target {
        object.insert("format".to_owned(), Value::String(format.to_owned()));
    }
}

// Standard types, written as serde_json writes them.

macro_rules! schema_json {
    ($($rust:ty => $schema:tt),* $(,)?) => {
        $(
            impl OpenApiSchema for $rust {
                fn schema(_generator: &mut SchemaGenerator) -> Schema {
                    json!($schema)
                }
            }
        )*
    };
}

schema_json! {
    bool => { "type": "boolean" },
    i8 => { "type": "integer", "format": "int32", "minimum": i8::MIN, "maximum": i8::MAX },
    i16 => { "type": "integer", "format": "int32", "minimum": i16::MIN, "maximum": i16::MAX },
    i32 => { "type": "integer", "format": "int32" },
    i64 => { "type": "integer", "format": "int64" },
    isize => { "type": "integer", "format": "int64" },
    u8 => { "type": "integer", "format": "int32", "minimum": 0, "maximum": u8::MAX },
    u16 => { "type": "integer", "format": "int32", "minimum": 0, "maximum": u16::MAX },
    u32 => { "type": "integer", "format": "int64", "minimum": 0 },
    u64 => { "type": "integer", "format": "int64", "minimum": 0 },
    usize => { "type": "integer", "format": "int64", "minimum": 0 },
    f32 => { "type": "number", "format": "float" },
    f64 => { "type": "number", "format": "double" },
    String => { "type": "string" },
    str => { "type": "string" },
    char => { "type": "string", "minLength": 1, "maxLength": 1 },
    () => { "type": "null" },
    Value => {},
    Map<String, Value> => { "type": "object", "additionalProperties": {} },
    Uuid => { "type": "string", "format": "uuid" },
    Timestamp => { "type": "string", "format": "date-time" },
    Version => { "type": "integer", "format": "int64", "minimum": 1 },
}

impl<T> OpenApiSchema for Id<T> {
    fn schema(_generator: &mut SchemaGenerator) -> Schema {
        json!({
            "type": "string",
            "format": "uuid",
            "pattern": UUID_V7_PATTERN,
            "description": "UUIDv7 (RFC 9562)",
        })
    }
}

/// The body of every error Forge answers with `forge::web::error`:
/// `{"code", "category", "message", "metadata"?, "retryable"}`.
impl OpenApiSchema for ErrorReport<'_> {
    fn schema(generator: &mut SchemaGenerator) -> Schema {
        generator.component::<ErrorReport<'static>>(Some("ErrorReport"), |generator| {
            let mut object = ObjectSchema::new();
            object
                .description("A caller-safe error (forge::web::error).")
                .field::<str>(
                    generator,
                    "code",
                    Field::new().doc("Stable machine-readable code."),
                )
                .field::<str>(
                    generator,
                    "category",
                    Field::new().doc(
                        "validation, not_found, conflict, unauthenticated, forbidden, \
                         rate_limited, infrastructure or internal.",
                    ),
                )
                .field::<str>(
                    generator,
                    "message",
                    Field::new().doc("Human-readable message safe to show."),
                )
                .field::<Map<String, Value>>(
                    generator,
                    "metadata",
                    Field::new()
                        .optional()
                        .doc("Structured caller-safe context."),
                )
                .field::<bool>(
                    generator,
                    "retryable",
                    Field::new().doc("Whether retrying may succeed."),
                );
            object.into_schema()
        })
    }
}

macro_rules! schema_as_inner {
    ($($wrapper:ident),*) => {
        $(
            impl<T: OpenApiSchema + ?Sized> OpenApiSchema for $wrapper<T> {
                fn schema(generator: &mut SchemaGenerator) -> Schema {
                    T::schema(generator)
                }
                fn flatten(generator: &mut SchemaGenerator) -> Option<ObjectSchema> {
                    T::flatten(generator)
                }
            }
        )*
    };
}

schema_as_inner!(Box, Arc, Rc);

impl<T: OpenApiSchema + ?Sized> OpenApiSchema for &T {
    fn schema(generator: &mut SchemaGenerator) -> Schema {
        T::schema(generator)
    }
    fn flatten(generator: &mut SchemaGenerator) -> Option<ObjectSchema> {
        T::flatten(generator)
    }
}

impl<T: OpenApiSchema + ToOwned + ?Sized> OpenApiSchema for Cow<'_, T> {
    fn schema(generator: &mut SchemaGenerator) -> Schema {
        T::schema(generator)
    }
    fn flatten(generator: &mut SchemaGenerator) -> Option<ObjectSchema> {
        T::flatten(generator)
    }
}

impl<T: OpenApiSchema> OpenApiSchema for Option<T> {
    fn schema(generator: &mut SchemaGenerator) -> Schema {
        nullable(T::schema(generator))
    }
    fn flatten(generator: &mut SchemaGenerator) -> Option<ObjectSchema> {
        T::flatten(generator).map(ObjectSchema::all_optional)
    }
}

fn array<T: OpenApiSchema + ?Sized>(generator: &mut SchemaGenerator) -> Schema {
    json!({ "type": "array", "items": T::schema(generator) })
}

impl<T: OpenApiSchema> OpenApiSchema for [T] {
    fn schema(generator: &mut SchemaGenerator) -> Schema {
        array::<T>(generator)
    }
}

impl<T: OpenApiSchema> OpenApiSchema for Vec<T> {
    fn schema(generator: &mut SchemaGenerator) -> Schema {
        array::<T>(generator)
    }
}

impl<T: OpenApiSchema> OpenApiSchema for VecDeque<T> {
    fn schema(generator: &mut SchemaGenerator) -> Schema {
        array::<T>(generator)
    }
}

impl<T: OpenApiSchema> OpenApiSchema for LinkedList<T> {
    fn schema(generator: &mut SchemaGenerator) -> Schema {
        array::<T>(generator)
    }
}

impl<T: OpenApiSchema> OpenApiSchema for BTreeSet<T> {
    fn schema(generator: &mut SchemaGenerator) -> Schema {
        array::<T>(generator)
    }
}

impl<T: OpenApiSchema, S> OpenApiSchema for HashSet<T, S> {
    fn schema(generator: &mut SchemaGenerator) -> Schema {
        array::<T>(generator)
    }
}

impl<T: OpenApiSchema, const N: usize> OpenApiSchema for [T; N] {
    fn schema(generator: &mut SchemaGenerator) -> Schema {
        json!({
            "type": "array",
            "items": T::schema(generator),
            "minItems": N,
            "maxItems": N,
        })
    }
}

/// A type serde_json writes as an object key: text, or a number written as
/// text.
pub trait MapKey {}

macro_rules! map_keys {
    ($($key:ty),*) => { $(impl MapKey for $key {})* };
}

map_keys!(
    String, str, char, i8, i16, i32, i64, isize, u8, u16, u32, u64, usize, Uuid
);

impl<T> MapKey for Id<T> {}
impl<K: MapKey + ?Sized> MapKey for &K {}
impl<K: MapKey + ToOwned + ?Sized> MapKey for Cow<'_, K> {}
impl<K: MapKey + ?Sized> MapKey for Box<K> {}
impl<K: MapKey + ?Sized> MapKey for Arc<K> {}

fn map<V: OpenApiSchema>(generator: &mut SchemaGenerator) -> Schema {
    json!({ "type": "object", "additionalProperties": V::schema(generator) })
}

fn map_members<V: OpenApiSchema>(generator: &mut SchemaGenerator) -> ObjectSchema {
    ObjectSchema {
        additional: Some(V::schema(generator)),
        ..ObjectSchema::default()
    }
}

impl<K: MapKey, V: OpenApiSchema> OpenApiSchema for BTreeMap<K, V> {
    fn schema(generator: &mut SchemaGenerator) -> Schema {
        map::<V>(generator)
    }
    fn flatten(generator: &mut SchemaGenerator) -> Option<ObjectSchema> {
        Some(map_members::<V>(generator))
    }
}

impl<K: MapKey, V: OpenApiSchema, S> OpenApiSchema for HashMap<K, V, S> {
    fn schema(generator: &mut SchemaGenerator) -> Schema {
        map::<V>(generator)
    }
    fn flatten(generator: &mut SchemaGenerator) -> Option<ObjectSchema> {
        Some(map_members::<V>(generator))
    }
}
