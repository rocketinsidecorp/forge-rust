//! Building the OpenAPI 3.1 document from routes and their descriptions.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::sync::Arc;

use forge_core::ErrorReport;
use serde_json::{Map, Value, json};

use crate::json::to_canonical;
use crate::operation::{Body, Operation, Param, ParamLocation};
use crate::schema::{
    ACCEPTS_KEY, ALIASES_KEY, Component, Field, OpenApiSchema, REF_PLACEHOLDER, SchemaGenerator,
    non_null,
};
use crate::status;

/// The OpenAPI version of the documents [`Document::build`] produces.
pub const VERSION: &str = "3.1.0";
/// The JSON Schema dialect of every schema in the document.
pub const JSON_SCHEMA_DIALECT: &str = "https://json-schema.org/draft/2020-12/schema";
/// The security scheme of routes that need a signed-in session.
pub const SESSION_SCHEME: &str = "sessionCookie";
/// The cookie Forge sessions authenticate with.
pub const SESSION_COOKIE: &str = "__Host-forge_session";

pub(crate) const JSON_MEDIA_TYPE: &str = "application/json";
pub(crate) const TEXT_MEDIA_TYPE: &str = "text/plain";
const METHODS: [&str; 8] = [
    "get", "put", "post", "delete", "options", "head", "patch", "trace",
];

/// The document's `info` object.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Info {
    title: String,
    version: String,
    description: Option<String>,
}

impl Info {
    /// The API's title (generated applications use their name) and version
    /// (the API's, not OpenAPI's).
    #[must_use]
    pub fn new(title: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            version: version.into(),
            description: None,
        }
    }

    /// A description of the API.
    #[must_use]
    pub fn description(mut self, text: impl Into<String>) -> Self {
        self.description = Some(text.into());
        self
    }

    /// The title.
    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }
}

/// A registered route: its method, path and, when it is described, its
/// operation.
#[derive(Clone, Debug)]
pub struct Route {
    /// The HTTP method, such as `GET`.
    pub method: String,
    /// The route's path, such as `/v1/posts/{id}`.
    pub path: String,
    /// The description; `None` lists the route as undocumented.
    pub operation: Option<Arc<Operation>>,
}

/// A description the document cannot express accurately: an error rather
/// than a guess.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BuildError(String);

impl BuildError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for BuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "openapi: {}", self.0)
    }
}

impl std::error::Error for BuildError {}

/// A built OpenAPI 3.1 document.
#[derive(Clone, Debug)]
pub struct Document {
    json: Arc<[u8]>,
    pub(crate) tree: Arc<Value>,
    undocumented: Vec<String>,
    pub(crate) routes: Arc<[DocumentedRoute]>,
}

/// A documented route and how a request path matches it.
#[derive(Debug)]
pub(crate) struct DocumentedRoute {
    pub(crate) method: String,
    pub(crate) path: String,
    pub(crate) segments: Vec<Segment>,
}

/// One segment of a route's path.
#[derive(Debug)]
pub(crate) enum Segment {
    /// Text compared with the request's segment.
    Static(String),
    /// A `{name}` parameter: any one non-empty segment.
    Param(String),
}

impl Document {
    /// Describes `routes` whose operation is given. Routes are documented in
    /// path and method order regardless of the order given; a description
    /// the document cannot express accurately (a path parameter the path
    /// lacks, a duplicate operation ID, a body on `204`, two types named
    /// alike) is an error.
    pub fn build(info: &Info, routes: &[Route]) -> Result<Self, BuildError> {
        if info.title.trim().is_empty() || info.version.trim().is_empty() {
            return Err(BuildError::new("the document needs a title and a version"));
        }
        let mut generator = SchemaGenerator::new();
        let error_schema = ErrorReport::schema(&mut generator);
        let mut sorted: Vec<&Route> = routes.iter().collect();
        sorted.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.method.cmp(&b.method)));
        let mut paths: BTreeMap<String, Map<String, Value>> = BTreeMap::new();
        let mut operation_ids: HashMap<String, String> = HashMap::new();
        let mut undocumented = Vec::new();
        let mut documented = Vec::new();
        let mut uses_session = false;
        for route in sorted {
            let pattern = format!("{} {}", route.method, route.path);
            let Some(operation) = &route.operation else {
                undocumented.push(pattern);
                continue;
            };
            let fail = |reason: String| BuildError::new(format!("{pattern}: {reason}"));
            if let Some(reason) = &operation.invalid {
                return Err(fail(reason.clone()));
            }
            let method = route.method.to_ascii_lowercase();
            if !METHODS.contains(&method.as_str()) {
                return Err(fail(format!(
                    "cannot describe: OpenAPI 3.1 has no {} operations",
                    route.method
                )));
            }
            let segments = parse_path(&route.path).map_err(fail)?;
            let wildcards: Vec<&str> = segments
                .iter()
                .filter_map(|segment| match segment {
                    Segment::Param(name) => Some(name.as_str()),
                    Segment::Static(_) => None,
                })
                .collect();
            let built = operation_object(
                &mut generator,
                &method,
                &route.path,
                &wildcards,
                operation,
                &error_schema,
            )
            .map_err(fail)?;
            if let Some(reason) = generator.take_error() {
                return Err(fail(format!("cannot describe: {reason}")));
            }
            let id = built
                .get("operationId")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            if let Some(previous) = operation_ids.get(&id) {
                return Err(BuildError::new(format!(
                    "{previous} and {pattern} share the operation ID {id:?}; set Operation::id"
                )));
            }
            operation_ids.insert(id, pattern.clone());
            uses_session |= operation.permission.is_some() || operation.authenticated;
            paths
                .entry(route.path.clone())
                .or_default()
                .insert(method.clone(), Value::Object(built));
            documented.push(DocumentedRoute {
                method: route.method.to_ascii_uppercase(),
                path: route.path.clone(),
                segments,
            });
        }
        if let Some(reason) = generator.take_error() {
            return Err(BuildError::new(format!("cannot describe: {reason}")));
        }
        let names = assign_names(&generator.components)?;
        let mut schemas = Map::new();
        for (component, name) in generator.components.iter().zip(&names) {
            schemas.insert(name.clone(), component.schema.clone());
        }
        let mut components = Map::new();
        components.insert("schemas".to_owned(), Value::Object(schemas));
        if uses_session {
            components.insert(
                "securitySchemes".to_owned(),
                json!({
                    SESSION_SCHEME: {
                        "type": "apiKey",
                        "in": "cookie",
                        "name": SESSION_COOKIE,
                        "description": "A Forge session: the cookie signing in sets.",
                    }
                }),
            );
        }
        let mut info_object = json!({ "title": info.title, "version": info.version });
        if let (Some(description), Some(object)) = (&info.description, info_object.as_object_mut())
        {
            object.insert("description".to_owned(), json!(description));
        }
        let paths: Map<String, Value> = paths
            .into_iter()
            .map(|(path, item)| (path, Value::Object(item)))
            .collect();
        let mut tree = json!({
            "openapi": VERSION,
            "jsonSchemaDialect": JSON_SCHEMA_DIALECT,
            "info": info_object,
            "paths": paths,
            "components": components,
        });
        resolve_references(&mut tree, &names);
        let json = to_canonical(&tree);
        Ok(Self {
            json: json.into(),
            tree: Arc::new(tree),
            undocumented,
            routes: documented.into(),
        })
    }

    /// The document: object keys sorted, indented with two spaces, ending in
    /// a newline, so the same routes and types always produce the same
    /// bytes and the file can be committed and diffed.
    #[must_use]
    pub fn json(&self) -> &[u8] {
        &self.json
    }

    /// The document as a JSON value.
    #[must_use]
    pub fn as_value(&self) -> &Value {
        &self.tree
    }

    /// The `METHOD /path` of every route that carries no description, sorted.
    #[must_use]
    pub fn undocumented(&self) -> &[String] {
        &self.undocumented
    }

    /// The documented paths, sorted.
    #[must_use]
    pub fn paths(&self) -> Vec<String> {
        self.tree
            .get("paths")
            .and_then(Value::as_object)
            .map(|paths| {
                let mut list: Vec<String> = paths.keys().cloned().collect();
                list.sort_unstable();
                list
            })
            .unwrap_or_default()
    }
}

/// The schema of `T` with the components it refers to, named as a document
/// would name them: for tests and documentation of a type's description.
pub fn schema_of<T: OpenApiSchema + ?Sized>() -> Result<(Value, Map<String, Value>), BuildError> {
    let mut generator = SchemaGenerator::new();
    let mut schema = T::schema(&mut generator);
    if let Some(reason) = generator.take_error() {
        return Err(BuildError::new(format!("cannot describe: {reason}")));
    }
    let names = assign_names(&generator.components)?;
    let mut components = Map::new();
    for (component, name) in generator.components.iter().zip(&names) {
        components.insert(name.clone(), component.schema.clone());
    }
    let mut components = Value::Object(components);
    resolve_references(&mut schema, &names);
    resolve_references(&mut components, &names);
    let components = match components {
        Value::Object(components) => components,
        _ => Map::new(),
    };
    Ok((schema, components))
}

/// The segments of `path`.
fn parse_path(path: &str) -> Result<Vec<Segment>, String> {
    let Some(rest) = path.strip_prefix('/') else {
        return Err(format!(
            "cannot describe: the path {path:?} is not absolute"
        ));
    };
    if rest.is_empty() {
        return Ok(Vec::new());
    }
    rest.split('/')
        .map(|segment| {
            if !segment.contains(['{', '}']) {
                return Ok(Segment::Static(segment.to_owned()));
            }
            match segment
                .strip_prefix('{')
                .and_then(|inner| inner.strip_suffix('}'))
            {
                Some(name) if !name.is_empty() && !name.contains(['{', '}']) => {
                    Ok(Segment::Param(name.to_owned()))
                }
                _ => Err(format!("cannot describe: wildcard segment {segment:?}")),
            }
        })
        .collect()
}

fn operation_object(
    generator: &mut SchemaGenerator,
    method: &str,
    path: &str,
    wildcards: &[&str],
    operation: &Operation,
    error_schema: &Value,
) -> Result<Map<String, Value>, String> {
    let mut result = Map::new();
    let id = operation
        .id
        .clone()
        .unwrap_or_else(|| operation_id(method, path));
    result.insert("operationId".to_owned(), json!(id));
    if let Some(summary) = &operation.summary {
        result.insert("summary".to_owned(), json!(summary));
    }
    if let Some(description) = &operation.description {
        result.insert("description".to_owned(), json!(description));
    }
    if !operation.tags.is_empty() {
        result.insert("tags".to_owned(), json!(operation.tags));
    }
    if operation.deprecated {
        result.insert("deprecated".to_owned(), json!(true));
    }
    let parameters = parameters(generator, wildcards, &operation.params)?;
    if !parameters.is_empty() {
        result.insert("parameters".to_owned(), Value::Array(parameters));
    }
    // What the framework answers on its own for this kind of operation; a
    // declared response with the same status takes precedence.
    let mut implied: Vec<(u16, Option<Body>)> = Vec::new();
    if let Some(request) = &operation.request {
        let body = request.schema(generator);
        result.insert(
            "requestBody".to_owned(),
            json!({ "required": true, "content": { JSON_MEDIA_TYPE: { "schema": body } } }),
        );
        // The server's body limit (text), then what decode_json answers: 415
        // for a Content-Type other than application/json, 422 for a body
        // that is not valid JSON for the type or has a member it lacks.
        implied.push((413, Some(Body::Text)));
        implied.push((415, None));
        implied.push((422, None));
    }
    if operation.permission.is_some() || operation.authenticated {
        result.insert("security".to_owned(), json!([{ SESSION_SCHEME: [] }]));
        implied.push((401, Some(Body::Text)));
    }
    if let Some(permission) = &operation.permission {
        result.insert("x-forge-permission".to_owned(), json!(permission));
        implied.push((403, Some(Body::Text)));
    }
    if operation.responses.is_empty() {
        return Err("cannot describe: the operation declares no response".to_owned());
    }
    let mut responses = Map::new();
    for response in &operation.responses {
        if !(100..=599).contains(&response.status) {
            return Err(format!(
                "cannot describe: response status {} is not an HTTP status",
                response.status
            ));
        }
        let key = response.status.to_string();
        if responses.contains_key(&key) {
            return Err(format!(
                "cannot describe: response {} is declared twice",
                response.status
            ));
        }
        let description = response
            .description
            .clone()
            .or_else(|| status::text(response.status).map(str::to_owned))
            .unwrap_or_else(|| key.clone());
        let mut object = Map::new();
        object.insert("description".to_owned(), json!(description));
        if let Some(body) = &response.body {
            if matches!(response.status, 204 | 304) {
                return Err(format!(
                    "cannot describe: a {} response has no body",
                    response.status
                ));
            }
            object.insert("content".to_owned(), content(generator, *body));
        }
        if !response.headers.is_empty() {
            let headers: Map<String, Value> = response
                .headers
                .iter()
                .map(|name| {
                    (
                        canonical_header(name),
                        json!({ "schema": { "type": "string" } }),
                    )
                })
                .collect();
            object.insert("headers".to_owned(), Value::Object(headers));
        }
        responses.insert(key, Value::Object(object));
    }
    for status in &operation.errors {
        if !(400..=599).contains(status) {
            return Err(format!(
                "cannot describe: error status {status} is not 4xx or 5xx"
            ));
        }
        let key = status.to_string();
        if responses.contains_key(&key) {
            return Err(format!(
                "cannot describe: status {status} is both a response and an error"
            ));
        }
        responses.insert(
            key,
            error_response(status::text(*status).unwrap_or("Error"), error_schema),
        );
    }
    for (status, body) in implied {
        let key = status.to_string();
        if responses.contains_key(&key) {
            continue;
        }
        let description = status::text(status).unwrap_or("Error");
        let response = match body {
            Some(body) => json!({
                "description": description,
                "content": content(generator, body),
            }),
            None => error_response(description, error_schema),
        };
        responses.insert(key, response);
    }
    responses.insert("default".to_owned(), error_response("Error", error_schema));
    result.insert("responses".to_owned(), Value::Object(responses));
    Ok(result)
}

fn content(generator: &mut SchemaGenerator, body: Body) -> Value {
    match body {
        Body::Json(body) => {
            json!({ JSON_MEDIA_TYPE: { "schema": body.schema(generator) } })
        }
        Body::Text => json!({ TEXT_MEDIA_TYPE: { "schema": { "type": "string" } } }),
    }
}

fn error_response(description: &str, schema: &Value) -> Value {
    json!({
        "description": description,
        "content": { JSON_MEDIA_TYPE: { "schema": schema } },
    })
}

/// The path's parameters and the declared ones, sorted by location and name.
fn parameters(
    generator: &mut SchemaGenerator,
    wildcards: &[&str],
    params: &[Param],
) -> Result<Vec<Value>, String> {
    let mut list: Vec<(ParamLocation, String, Value)> = Vec::new();
    for param in params {
        if param.name.is_empty() {
            return Err(format!(
                "cannot describe: a {} parameter has no name",
                param.location.as_str()
            ));
        }
        // A parameter is text and is never null; an absent one is
        // expressed by required.
        let mut field = Field::new();
        if let Some(example) = &param.example {
            field = field.example(example.clone());
        }
        if let Some(format) = &param.format {
            field = field.format(format.clone());
        }
        let schema = field.annotate(non_null(param.schema.schema(generator)));
        let mut object = Map::new();
        object.insert("name".to_owned(), json!(param.name));
        object.insert("in".to_owned(), json!(param.location.as_str()));
        object.insert("schema".to_owned(), schema);
        if let Some(description) = &param.description {
            object.insert("description".to_owned(), json!(description));
        }
        if param.location == ParamLocation::Path {
            if !wildcards.contains(&param.name.as_str()) {
                return Err(format!(
                    "cannot describe: path parameter {:?} is not a wildcard of the route",
                    param.name
                ));
            }
            object.insert("required".to_owned(), json!(true));
        } else if param.required {
            object.insert("required".to_owned(), json!(true));
        }
        list.push((param.location, param.name.clone(), Value::Object(object)));
    }
    for wildcard in wildcards {
        let declared = params
            .iter()
            .any(|param| param.location == ParamLocation::Path && param.name == *wildcard);
        if !declared {
            list.push((
                ParamLocation::Path,
                (*wildcard).to_owned(),
                json!({
                    "name": wildcard,
                    "in": "path",
                    "required": true,
                    "schema": { "type": "string" },
                }),
            ));
        }
    }
    list.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    for pair in list.windows(2) {
        if pair[0].0 == pair[1].0 && pair[0].1 == pair[1].1 {
            return Err(format!(
                "cannot describe: {} parameter {:?} is declared twice",
                pair[0].0.as_str(),
                pair[0].1
            ));
        }
    }
    Ok(list.into_iter().map(|(_, _, object)| object).collect())
}

/// `getV1PostsById` for `GET /v1/posts/{id}`.
pub(crate) fn operation_id(method: &str, path: &str) -> String {
    let mut id = method.to_owned();
    for segment in path.split('/') {
        let mut segment = segment;
        if segment.starts_with('{') {
            id.push_str("By");
            segment = segment.trim_matches(|ch| ch == '{' || ch == '}');
        }
        for word in segment
            .split(|ch: char| !ch.is_alphanumeric())
            .filter(|word| !word.is_empty())
        {
            id.push_str(&upper_first(word));
        }
    }
    id
}

/// `Content-Type` for `content-type`, as Go canonicalizes header names.
fn canonical_header(name: &str) -> String {
    if !name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
    {
        return name.to_owned();
    }
    let mut upper = true;
    name.chars()
        .map(|ch| {
            let converted = if upper {
                ch.to_ascii_uppercase()
            } else {
                ch.to_ascii_lowercase()
            };
            upper = ch == '-';
            converted
        })
        .collect()
}

fn upper_first(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().collect::<String>() + chars.as_str()
    })
}

/// How much of a type's path its component name carries.
#[derive(Clone, Copy, Eq, PartialEq)]
enum Qualification {
    /// `Page_Post`.
    Bare,
    /// `pagination.Page_http.Post`: the module of each type.
    Module,
    /// `app.pagination.Page_app.adapters.http.Post`.
    Path,
}

/// Names every component after its Rust type (or its chosen name) in upper
/// camel case, with type arguments reduced to their names (`Page<Post>` is
/// `Page_Post`). Types that would share a name are qualified with their
/// module, then with their full path; names that still collide (a chosen
/// name equal to another type's) are an error naming both types, never one
/// schema standing for two.
fn assign_names(components: &[Component]) -> Result<Vec<String>, BuildError> {
    let mut order: Vec<usize> = (0..components.len()).collect();
    order.sort_by(|&a, &b| {
        let (a, b) = (&components[a], &components[b]);
        a.type_name.cmp(b.type_name).then_with(|| {
            component_name(a, Qualification::Path).cmp(&component_name(b, Qualification::Path))
        })
    });
    let mut groups: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for &index in &order {
        groups
            .entry(component_name(&components[index], Qualification::Bare))
            .or_default()
            .push(index);
    }
    let mut names = vec![String::new(); components.len()];
    for (bare, group) in groups {
        if let [only] = group.as_slice() {
            names[*only] = bare;
            continue;
        }
        let distinct = |level| {
            let mut seen = std::collections::HashSet::new();
            group
                .iter()
                .all(|&index| seen.insert(component_name(&components[index], level)))
        };
        let level = if distinct(Qualification::Module) {
            Qualification::Module
        } else {
            Qualification::Path
        };
        for &index in &group {
            names[index] = component_name(&components[index], level);
        }
    }
    let mut owners: HashMap<&str, &'static str> = HashMap::new();
    for &index in &order {
        let name = names[index].as_str();
        let type_name = components[index].type_name;
        if let Some(previous) = owners.insert(name, type_name) {
            return Err(BuildError::new(format!(
                "cannot describe: the types {previous} and {type_name} would both be the \
                 component {name:?}; give one a distinct name with #[openapi(name = \"...\")]"
            )));
        }
    }
    Ok(names)
}

fn component_name(component: &Component, level: Qualification) -> String {
    if let Some(name) = &component.explicit_name {
        let name = sanitize(name);
        if !name.is_empty() {
            return name;
        }
    }
    let text = component.type_name;
    let mut name = String::new();
    let mut start = 0;
    for (index, ch) in text.char_indices() {
        if "<>,;&[]() *".contains(ch) {
            if index > start {
                identifier(&text[start..index], level, &mut name);
            }
            name.push('_');
            start = index + ch.len_utf8();
        }
    }
    if start < text.len() {
        identifier(&text[start..], level, &mut name);
    }
    let mut collapsed = name;
    while collapsed.contains("__") {
        collapsed = collapsed.replace("__", "_");
    }
    sanitize(collapsed.trim_matches('_'))
}

/// One path such as `app::http::Post` at `level`.
fn identifier(text: &str, level: Qualification, out: &mut String) {
    let parts: Vec<&str> = text.split("::").filter(|part| !part.is_empty()).collect();
    let Some((name, modules)) = parts.split_last() else {
        return;
    };
    match level {
        Qualification::Bare => {}
        Qualification::Module => {
            if let Some(module) = modules.last() {
                out.push_str(module);
                out.push('.');
            }
        }
        Qualification::Path => {
            if !modules.is_empty() {
                out.push_str(&modules.join("."));
                out.push('.');
            }
        }
    }
    for part in name.split('_') {
        out.push_str(&upper_first(part));
    }
}

/// Keeps the characters OpenAPI allows in component names.
fn sanitize(name: &str) -> String {
    name.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

/// Rewrites the placeholder references to `#/components/schemas/NAME`.
fn resolve_references(value: &mut Value, names: &[String]) {
    match value {
        Value::Object(object) => {
            // Internal: the names typed decoding accepts, never published.
            // Only at an object schema node (`"type": "object"` as the
            // derive writes it), never in a map of member names.
            if object.get("type") == Some(&Value::from("object"))
                && object.contains_key("properties")
            {
                object.remove(ACCEPTS_KEY);
                object.remove(ALIASES_KEY);
            }
            for (key, member) in object.iter_mut() {
                if key == "$ref"
                    && let Value::String(text) = member
                    && let Some(index) = text
                        .strip_prefix(REF_PLACEHOLDER)
                        .and_then(|index| index.parse::<usize>().ok())
                    && let Some(name) = names.get(index)
                {
                    *text = format!("#/components/schemas/{name}");
                } else {
                    resolve_references(member, names);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                resolve_references(item, names);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operation_ids_follow_the_method_and_path() {
        assert_eq!(operation_id("get", "/v1/posts/{id}"), "getV1PostsById");
        assert_eq!(
            operation_id("post", "/v1/status_checks"),
            "postV1StatusChecks"
        );
        assert_eq!(operation_id("get", "/"), "get");
    }

    #[test]
    fn headers_are_canonical() {
        assert_eq!(canonical_header("location"), "Location");
        assert_eq!(canonical_header("x-request-id"), "X-Request-Id");
        assert_eq!(canonical_header("bad header"), "bad header");
    }

    fn component(type_name: &'static str) -> Component {
        Component {
            type_name,
            explicit_name: None,
            schema: Value::Null,
        }
    }

    #[test]
    fn component_names_reduce_type_arguments_and_qualify_collisions() {
        let names = assign_names(&[
            component("forge::pagination::Page<app::http::Post>"),
            component("app::v1::Item"),
            component("app::v2::Item"),
            component("app::status_response"),
        ])
        .unwrap_or_default();
        assert_eq!(names, ["Page_Post", "v1.Item", "v2.Item", "StatusResponse"]);
        let deep =
            assign_names(&[component("a::x::Item"), component("b::x::Item")]).unwrap_or_default();
        assert_eq!(deep, ["a.x.Item", "b.x.Item"]);
        let named = |type_name| {
            let mut chosen = component(type_name);
            chosen.explicit_name = Some("Item".to_owned());
            chosen
        };
        let error = assign_names(&[named("app::Report"), named("app::Summary")]).err();
        assert!(
            error.is_some_and(|error| error.to_string().contains("app::Report")),
            "a collision names both types"
        );
    }
}
