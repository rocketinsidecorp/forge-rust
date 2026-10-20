//! Contract checks: a real response against the published document.

use std::fmt;

use serde_json::Value;

use crate::document::{Document, DocumentedRoute, JSON_MEDIA_TYPE, Segment, TEXT_MEDIA_TYPE};
use crate::schema::UUID_V7_PATTERN;

/// A response that does not match the document, or a request no documented
/// operation serves.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ContractError {
    /// No documented operation serves `method path`; nothing was checked.
    Undocumented {
        /// The request method.
        method: String,
        /// The request path.
        path: String,
    },
    /// The response differs from what the document declares.
    Mismatch(String),
}

impl fmt::Display for ContractError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Undocumented { method, path } => write!(
                formatter,
                "openapi: no documented operation serves the request: {method} {path}"
            ),
            Self::Mismatch(reason) => write!(
                formatter,
                "openapi: response does not match the document: {reason}"
            ),
        }
    }
}

impl std::error::Error for ContractError {}

impl Document {
    /// Checks a response a documented route returned against the document:
    /// the status must be declared (or fall under `default`), a body must
    /// have a declared media type and match its schema, and a response
    /// declared without a body must have none. `method` and `path` are the
    /// request's, such as `GET` and `/v1/posts/0192...`; the route is found
    /// with the router's rules (static segments win over `{name}`). It
    /// validates the subset of JSON Schema that [`Document::build`] emits.
    pub fn validate_response(
        &self,
        method: &str,
        path: &str,
        status: u16,
        content_type: Option<&str>,
        body: &[u8],
    ) -> Result<(), ContractError> {
        let route = self
            .find(method, path)
            .ok_or_else(|| ContractError::Undocumented {
                method: method.to_owned(),
                path: path.to_owned(),
            })?;
        self.check(route, method, status, content_type, body)
    }

    /// [`Document::validate_response`] for a request the router matched to
    /// the route `route` (its pattern, such as `/v1/posts/{id}`).
    pub fn validate_route_response(
        &self,
        method: &str,
        route: &str,
        status: u16,
        content_type: Option<&str>,
        body: &[u8],
    ) -> Result<(), ContractError> {
        let found = self
            .routes
            .iter()
            .find(|documented| {
                documented.method.eq_ignore_ascii_case(method) && documented.path == route
            })
            .ok_or_else(|| ContractError::Undocumented {
                method: method.to_owned(),
                path: route.to_owned(),
            })?;
        self.check(found, method, status, content_type, body)
    }

    /// The documented route serving `method path`: of the routes whose
    /// segments match, the one with static text where the others capture.
    fn find(&self, method: &str, path: &str) -> Option<&DocumentedRoute> {
        let path = path.split(['?', '#']).next().unwrap_or_default();
        let rest = path.strip_prefix('/')?;
        let segments: Vec<&str> = if rest.is_empty() {
            Vec::new()
        } else {
            rest.split('/').collect()
        };
        self.routes
            .iter()
            .filter(|route| {
                route.method.eq_ignore_ascii_case(method)
                    && route.segments.len() == segments.len()
                    && route.segments.iter().zip(&segments).all(
                        |(pattern, segment)| match pattern {
                            Segment::Static(text) => text == segment,
                            Segment::Param(_) => !segment.is_empty(),
                        },
                    )
            })
            .max_by(|a, b| {
                let shape = |route: &DocumentedRoute| -> Vec<bool> {
                    route
                        .segments
                        .iter()
                        .map(|segment| matches!(segment, Segment::Static(_)))
                        .collect()
                };
                shape(a).cmp(&shape(b))
            })
    }

    fn check(
        &self,
        route: &DocumentedRoute,
        method: &str,
        status: u16,
        content_type: Option<&str>,
        body: &[u8],
    ) -> Result<(), ContractError> {
        let at = format!("{method} {} {status}", route.path);
        let mismatch = |reason: String| ContractError::Mismatch(format!("{at}: {reason}"));
        let operation = self
            .tree
            .get("paths")
            .and_then(|paths| paths.get(&route.path))
            .and_then(|item| item.get(route.method.to_ascii_lowercase()))
            .ok_or_else(|| mismatch("the document has no such operation".to_owned()))?;
        let responses = operation.get("responses");
        let response = responses
            .and_then(|responses| responses.get(status.to_string()))
            .or_else(|| responses.and_then(|responses| responses.get("default")))
            .ok_or_else(|| mismatch("the status is not declared".to_owned()))?;
        if method.eq_ignore_ascii_case("HEAD") {
            // HEAD answers a GET route's status and headers without the body.
            return Ok(());
        }
        let Some(content) = response.get("content").and_then(Value::as_object) else {
            if body.iter().any(|byte| !byte.is_ascii_whitespace()) {
                return Err(mismatch("declared without a body but has one".to_owned()));
            }
            return Ok(());
        };
        let media_type = content_type
            .and_then(|value| value.split(';').next())
            .map(|value| value.trim().to_ascii_lowercase())
            .unwrap_or_default();
        let Some(media) = content.get(&media_type) else {
            let declared: Vec<&str> = content.keys().map(String::as_str).collect();
            return Err(mismatch(format!(
                "has Content-Type {:?}, want {}",
                content_type.unwrap_or_default(),
                declared.join(" or ")
            )));
        };
        match media_type.as_str() {
            JSON_MEDIA_TYPE => {
                let value: Value = serde_json::from_slice(body)
                    .map_err(|error| mismatch(format!("body is not JSON: {error}")))?;
                let schema = media.get("schema").unwrap_or(&Value::Null);
                Validator {
                    document: &self.tree,
                    depth: 0,
                }
                .validate(schema, &value, "")
                .map_err(mismatch)
            }
            TEXT_MEDIA_TYPE => std::str::from_utf8(body)
                .map(|_| ())
                .map_err(|_| mismatch("body is not UTF-8 text".to_owned())),
            _ => Ok(()),
        }
    }
}

/// Validates the JSON Schema subset `Document::build` emits: `$ref`, `anyOf`,
/// `type`, `enum`, `properties`, `required`, `additionalProperties`,
/// `items`, item counts, lengths, ranges, the UUIDv7 `pattern`, and the
/// `date-time` and `uuid` formats.
struct Validator<'a> {
    document: &'a Value,
    depth: usize,
}

impl<'a> Validator<'a> {
    fn validate(&mut self, schema: &Value, value: &Value, at: &str) -> Result<(), String> {
        let Some(schema) = schema.as_object() else {
            return Err(format!("{}: the document has no schema here", pointer(at)));
        };
        self.depth += 1;
        let result = self.validate_object_schema(schema, value, at);
        self.depth -= 1;
        result
    }

    fn validate_object_schema(
        &mut self,
        schema: &serde_json::Map<String, Value>,
        value: &Value,
        at: &str,
    ) -> Result<(), String> {
        if self.depth > 256 {
            return Err(format!("{}: value nests too deeply", pointer(at)));
        }
        if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
            let target = self.resolve(reference)?;
            self.validate(target, value, at)?;
        }
        if let Some(branches) = schema.get("anyOf").and_then(Value::as_array) {
            let mut failures = Vec::new();
            let mut matched = false;
            for branch in branches {
                match self.validate(branch, value, at) {
                    Ok(()) => {
                        matched = true;
                        break;
                    }
                    Err(failure) => failures.push(failure),
                }
            }
            if !matched {
                return Err(format!(
                    "{}: matches no alternative ({})",
                    pointer(at),
                    failures.join("; ")
                ));
            }
        }
        if let Some(kinds) = types_of(schema.get("type"))
            && !kinds.iter().any(|kind| is_kind(value, kind))
        {
            return Err(format!(
                "{}: {} is not of type {}",
                pointer(at),
                describe(value),
                kinds.join(" or ")
            ));
        }
        if let Some(allowed) = schema.get("enum").and_then(Value::as_array)
            && !allowed.contains(value)
        {
            return Err(format!(
                "{}: {value} is not one of {allowed:?}",
                pointer(at)
            ));
        }
        match value {
            Value::String(text) => validate_string(schema, text, at),
            Value::Number(number) => validate_number(schema, number, at),
            Value::Array(items) => {
                let count = items.len() as f64;
                if let Some(minimum) = schema.get("minItems").and_then(Value::as_f64)
                    && count < minimum
                {
                    return Err(format!(
                        "{}: has {count} items, fewer than {minimum}",
                        pointer(at)
                    ));
                }
                if let Some(maximum) = schema.get("maxItems").and_then(Value::as_f64)
                    && count > maximum
                {
                    return Err(format!(
                        "{}: has {count} items, more than {maximum}",
                        pointer(at)
                    ));
                }
                if let Some(item_schema) = schema.get("items") {
                    for (index, item) in items.iter().enumerate() {
                        self.validate(item_schema, item, &format!("{at}/{index}"))?;
                    }
                }
                Ok(())
            }
            Value::Object(members) => self.validate_members(schema, members, at),
            Value::Null | Value::Bool(_) => Ok(()),
        }
    }

    fn validate_members(
        &mut self,
        schema: &serde_json::Map<String, Value>,
        members: &serde_json::Map<String, Value>,
        at: &str,
    ) -> Result<(), String> {
        if let Some(required) = schema.get("required").and_then(Value::as_array) {
            for name in required.iter().filter_map(Value::as_str) {
                if !members.contains_key(name) {
                    return Err(format!(
                        "{}: required member {name:?} is missing",
                        pointer(at)
                    ));
                }
            }
        }
        let properties = schema.get("properties").and_then(Value::as_object);
        let mut keys: Vec<&String> = members.keys().collect();
        keys.sort_unstable();
        for key in keys {
            let member_at = format!("{at}/{}", key.replace('~', "~0").replace('/', "~1"));
            let Some(member) = members.get(key) else {
                continue;
            };
            if let Some(property) = properties.and_then(|properties| properties.get(key)) {
                self.validate(property, member, &member_at)?;
                continue;
            }
            match schema.get("additionalProperties") {
                Some(Value::Bool(false)) => {
                    return Err(format!(
                        "{}: member {key:?} is not in the schema",
                        pointer(at)
                    ));
                }
                Some(additional @ Value::Object(_)) => {
                    self.validate(additional, member, &member_at)?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn resolve(&self, reference: &str) -> Result<&'a Value, String> {
        let name = reference
            .strip_prefix("#/components/schemas/")
            .ok_or_else(|| format!("unsupported reference {reference:?}"))?;
        let document: &'a Value = self.document;
        document
            .get("components")
            .and_then(|components| components.get("schemas"))
            .and_then(|schemas| schemas.get(name))
            .ok_or_else(|| format!("unknown reference {reference:?}"))
    }
}

fn validate_string(
    schema: &serde_json::Map<String, Value>,
    text: &str,
    at: &str,
) -> Result<(), String> {
    let length = text.chars().count() as f64;
    if let Some(minimum) = schema.get("minLength").and_then(Value::as_f64)
        && length < minimum
    {
        return Err(format!(
            "{}: {text:?} is shorter than {minimum}",
            pointer(at)
        ));
    }
    if let Some(maximum) = schema.get("maxLength").and_then(Value::as_f64)
        && length > maximum
    {
        return Err(format!(
            "{}: {text:?} is longer than {maximum}",
            pointer(at)
        ));
    }
    match schema.get("format").and_then(Value::as_str) {
        Some("date-time") if !is_date_time(text) => {
            return Err(format!(
                "{}: {text:?} is not an RFC 3339 date-time",
                pointer(at)
            ));
        }
        Some("uuid") if !is_uuid(text) => {
            return Err(format!("{}: {text:?} is not a UUID", pointer(at)));
        }
        _ => {}
    }
    if let Some(pattern) = schema.get("pattern").and_then(Value::as_str) {
        if pattern != UUID_V7_PATTERN {
            return Err(format!(
                "{}: the validator does not support the pattern {pattern:?}",
                pointer(at)
            ));
        }
        if !is_uuid_v7(text) {
            return Err(format!(
                "{}: {text:?} does not match {pattern}",
                pointer(at)
            ));
        }
    }
    Ok(())
}

fn validate_number(
    schema: &serde_json::Map<String, Value>,
    number: &serde_json::Number,
    at: &str,
) -> Result<(), String> {
    let Some(value) = number.as_f64() else {
        return Err(format!("{}: {number} is not a number", pointer(at)));
    };
    if let Some(minimum) = schema.get("minimum").and_then(Value::as_f64)
        && value < minimum
    {
        return Err(format!("{}: {number} is less than {minimum}", pointer(at)));
    }
    if let Some(maximum) = schema.get("maximum").and_then(Value::as_f64)
        && value > maximum
    {
        return Err(format!(
            "{}: {number} is greater than {maximum}",
            pointer(at)
        ));
    }
    Ok(())
}

fn types_of(raw: Option<&Value>) -> Option<Vec<&str>> {
    match raw? {
        Value::String(kind) => Some(vec![kind.as_str()]),
        Value::Array(kinds) => Some(kinds.iter().filter_map(Value::as_str).collect()),
        _ => None,
    }
}

fn is_kind(value: &Value, kind: &str) -> bool {
    match value {
        Value::Null => kind == "null",
        Value::Bool(_) => kind == "boolean",
        Value::String(_) => kind == "string",
        Value::Array(_) => kind == "array",
        Value::Object(_) => kind == "object",
        Value::Number(number) => {
            kind == "number"
                || (kind == "integer"
                    && (number.is_i64()
                        || number.is_u64()
                        || number.as_f64().is_some_and(|value| value.fract() == 0.0)))
        }
    }
}

fn describe(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
        Value::Number(_) => "a number",
    }
}

fn pointer(at: &str) -> String {
    format!("body{at}")
}

/// Any UUID in its 8-4-4-4-12 hexadecimal text.
fn is_uuid(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => *byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        })
}

/// [`UUID_V7_PATTERN`]: lowercase, version 7, RFC 9562 variant.
fn is_uuid_v7(text: &str) -> bool {
    let bytes = text.as_bytes();
    is_uuid(text)
        && !bytes.iter().any(u8::is_ascii_uppercase)
        && bytes[14] == b'7'
        && matches!(bytes[19], b'8' | b'9' | b'a' | b'b')
}

/// RFC 3339 `date-time`: `YYYY-MM-DDTHH:MM:SS[.fraction](Z|±HH:MM)`.
fn is_date_time(text: &str) -> bool {
    let bytes = text.as_bytes();
    let digits = |range: std::ops::Range<usize>| {
        bytes
            .get(range)
            .is_some_and(|part| part.iter().all(u8::is_ascii_digit))
    };
    let number = |range: std::ops::Range<usize>| -> u32 {
        bytes.get(range).map_or(0, |part| {
            part.iter()
                .fold(0, |total, digit| total * 10 + u32::from(digit - b'0'))
        })
    };
    if bytes.len() < 20
        || !digits(0..4)
        || bytes[4] != b'-'
        || !digits(5..7)
        || bytes[7] != b'-'
        || !digits(8..10)
        || !matches!(bytes[10], b'T' | b't')
        || !digits(11..13)
        || bytes[13] != b':'
        || !digits(14..16)
        || bytes[16] != b':'
        || !digits(17..19)
    {
        return false;
    }
    if !(1..=12).contains(&number(5..7))
        || !(1..=31).contains(&number(8..10))
        || number(11..13) > 23
        || number(14..16) > 59
        || number(17..19) > 60
    {
        return false;
    }
    let mut rest = &bytes[19..];
    if let Some(fraction) = rest.strip_prefix(b".") {
        let count = fraction
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
        if count == 0 {
            return false;
        }
        rest = &fraction[count..];
    }
    match rest {
        [b'Z' | b'z'] => true,
        [b'+' | b'-', h1, h2, b':', m1, m2] => {
            [h1, h2, m1, m2].iter().all(|byte| byte.is_ascii_digit())
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_are_checked() {
        assert!(is_date_time("2026-10-20T12:34:56Z"));
        assert!(is_date_time("2026-10-20T12:34:56.123456789+02:00"));
        assert!(!is_date_time("2026-13-20T12:34:56Z"));
        assert!(!is_date_time("2026-10-20 12:34"));
        assert!(is_uuid("0192f1a0-0000-7000-8000-000000000000"));
        assert!(is_uuid_v7("0192f1a0-0000-7000-8000-000000000000"));
        assert!(!is_uuid_v7("0192f1a0-0000-4000-8000-000000000000"));
        assert!(!is_uuid_v7("0192F1A0-0000-7000-8000-000000000000"));
    }
}
