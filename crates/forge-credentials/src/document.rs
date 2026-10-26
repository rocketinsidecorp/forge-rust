//! The plaintext document: one JSON object, no duplicate keys at any depth.

use std::fmt;

use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Number, Value};
use zeroize::Zeroizing;

use crate::{CredentialsError, MAX_PLAINTEXT_BYTES};

const DUPLICATE: &str = "forge-credentials: duplicate key";

/// Validates a credentials document: a single JSON object with no duplicate
/// keys (JSON parsers disagree on which duplicate wins, which is a trap for
/// secrets), at most [`MAX_PLAINTEXT_BYTES`]. Errors name a line and column,
/// never content.
pub fn validate_document(data: &[u8]) -> Result<(), CredentialsError> {
    let mut document = parse_document(data)?;
    wipe_map(&mut document);
    Ok(())
}

pub(crate) fn parse_document(data: &[u8]) -> Result<Map<String, Value>, CredentialsError> {
    if data.len() > MAX_PLAINTEXT_BYTES {
        return Err(CredentialsError::Document(format!(
            "credentials: document exceeds {MAX_PLAINTEXT_BYTES} bytes"
        )));
    }
    let Strict(value) = serde_json::from_slice(data).map_err(|error| locate(&error))?;
    match value {
        Value::Object(map) => Ok(map),
        mut other => {
            wipe(&mut other);
            Err(CredentialsError::Document(
                "credentials: the document must be a JSON object".to_owned(),
            ))
        }
    }
}

fn locate(error: &serde_json::Error) -> CredentialsError {
    let at = format!("line {}, column {}", error.line(), error.column());
    let message = match error.classify() {
        serde_json::error::Category::Eof => {
            "credentials: document is not valid JSON: unexpected end of input".to_owned()
        }
        serde_json::error::Category::Syntax => {
            format!("credentials: document is not valid JSON: syntax error at {at}")
        }
        serde_json::error::Category::Data if error.to_string().starts_with(DUPLICATE) => {
            format!("credentials: duplicate key at {at}")
        }
        _ => "credentials: document is not valid JSON".to_owned(),
    };
    CredentialsError::Document(message)
}

/// A JSON value whose objects were checked for duplicate keys.
struct Strict(Value);

impl<'de> Deserialize<'de> for Strict {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(StrictVisitor).map(Strict)
    }
}

struct StrictVisitor;

impl<'de> Visitor<'de> for StrictVisitor {
    type Value = Value;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Value, E> {
        Ok(Value::Bool(value))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Value, E> {
        Ok(Value::Number(value.into()))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Value, E> {
        Ok(Value::Number(value.into()))
    }

    fn visit_f64<E: de::Error>(self, value: f64) -> Result<Value, E> {
        Number::from_f64(value)
            .map(Value::Number)
            .ok_or_else(|| E::custom("number out of range"))
    }

    fn visit_str<E>(self, value: &str) -> Result<Value, E> {
        Ok(Value::String(value.to_owned()))
    }

    fn visit_string<E>(self, value: String) -> Result<Value, E> {
        Ok(Value::String(value))
    }

    fn visit_unit<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_none<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let mut items = Vec::new();
        while let Some(Strict(item)) = seq.next_element()? {
            items.push(item);
        }
        Ok(Value::Array(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Value, A::Error> {
        let mut map = Map::new();
        while let Some(key) = access.next_key::<String>()? {
            if map.contains_key(&key) {
                wipe_map(&mut map);
                return Err(de::Error::custom(DUPLICATE));
            }
            let Strict(value) = access.next_value()?;
            map.insert(key, value);
        }
        Ok(Value::Object(map))
    }
}

/// The document's structure with every value replaced by `"[REDACTED]"`:
/// keys, nesting and array lengths stay, keys sorted, indented with two
/// spaces (what `forge credentials show` prints).
pub fn redact(data: &[u8]) -> Result<Zeroizing<Vec<u8>>, CredentialsError> {
    fn redact_value(value: &mut Value) {
        match value {
            Value::Object(map) => map.values_mut().for_each(redact_value),
            Value::Array(items) => items.iter_mut().for_each(redact_value),
            other => {
                wipe(other);
                *other = Value::String("[REDACTED]".to_owned());
            }
        }
    }
    let mut document = Value::Object(parse_document(data)?);
    redact_value(&mut document);
    let mut out = serde_json::to_vec_pretty(&document)
        .map_err(|_| CredentialsError::Document("cannot encode the document".to_owned()))?;
    out.push(b'\n');
    Ok(Zeroizing::new(out))
}

/// Overwrites every string of `value` before it is dropped.
pub(crate) fn wipe(value: &mut Value) {
    use zeroize::Zeroize as _;

    match value {
        Value::String(text) => text.zeroize(),
        Value::Array(items) => items.iter_mut().for_each(wipe),
        Value::Object(map) => wipe_map(map),
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

pub(crate) fn wipe_map(map: &mut Map<String, Value>) {
    map.values_mut().for_each(wipe);
}

/// Re-indents valid JSON text with two spaces, keeping every token (key
/// order, number spelling, string escapes) exactly as written, like Go's
/// `json.Indent`. Empty objects and arrays stay `{}` and `[]`.
pub fn indent(json: &[u8]) -> Zeroizing<Vec<u8>> {
    let mut out = Zeroizing::new(Vec::with_capacity(json.len() + json.len() / 4));
    let mut depth = 0_usize;
    let mut in_string = false;
    let mut escaped = false;
    let mut open = false; // an object or array was just opened
    let newline = |out: &mut Vec<u8>, depth: usize| {
        out.push(b'\n');
        out.extend(std::iter::repeat_n(b' ', depth * 2));
    };
    for &byte in json {
        if in_string {
            out.push(byte);
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        if matches!(byte, b' ' | b'\t' | b'\n' | b'\r') {
            continue;
        }
        if open {
            open = false;
            if matches!(byte, b'}' | b']') {
                depth = depth.saturating_sub(1);
                out.push(byte);
                continue;
            }
            newline(&mut out, depth);
        }
        match byte {
            b'{' | b'[' => {
                out.push(byte);
                depth += 1;
                open = true;
            }
            b'}' | b']' => {
                depth = depth.saturating_sub(1);
                newline(&mut out, depth);
                out.push(byte);
            }
            b',' => {
                out.push(byte);
                newline(&mut out, depth);
            }
            b':' => out.extend_from_slice(b": "),
            b'"' => {
                in_string = true;
                out.push(byte);
            }
            other => out.push(other),
        }
    }
    out.push(b'\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(data: &str) -> String {
        validate_document(data.as_bytes())
            .expect_err("invalid")
            .to_string()
    }

    #[test]
    fn documents_are_one_object_without_duplicates() {
        validate_document(br#"{"a": {"b": [1, {"c": "d"}]}, "e": null}"#).expect("valid");
        validate_document(b"{}\n").expect("valid");
        assert_eq!(
            message("{\n  \"a\": \"sk_live_1\",\n  \"a\": \"sk_live_2\"\n}"),
            "credentials: duplicate key at line 3, column 5"
        );
        assert!(message(r#"{"x": {"a": 1, "a": 2}}"#).starts_with("credentials: duplicate key"));
        assert!(message(r#"{"x": [{"a": 1, "a": 2}]}"#).starts_with("credentials: duplicate key"));
        assert_eq!(
            message(r#"["sk_live_secret"]"#),
            "credentials: the document must be a JSON object"
        );
        assert_eq!(
            message("\"sk_live_secret\""),
            "credentials: the document must be a JSON object"
        );
        let syntax = message("{\"token\": \"sk_live_secret\",, }");
        assert!(
            syntax.starts_with("credentials: document is not valid JSON: syntax error at line 1"),
            "{syntax}"
        );
        assert!(!syntax.contains("sk_live"));
        assert_eq!(
            message("{\"token\": \"sk_live_secret"),
            "credentials: document is not valid JSON: unexpected end of input"
        );
        let trailing = message("{} {}");
        assert!(trailing.contains("syntax error"), "{trailing}");
        let large = format!("{{\"a\": \"{}\"}}", "x".repeat(MAX_PLAINTEXT_BYTES));
        assert!(message(&large).contains("exceeds"));
    }

    /// A deserializer of a self-describing format with an absent optional.
    struct Absent;

    impl<'de> Deserializer<'de> for Absent {
        type Error = de::value::Error;

        fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
            visitor.visit_none()
        }

        serde::forward_to_deserialize_any! {
            bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string bytes
            byte_buf option unit unit_struct newtype_struct seq tuple tuple_struct map
            struct enum identifier ignored_any
        }
    }

    #[test]
    fn the_strict_reader_takes_every_json_value_and_names_what_it_expects() {
        use serde::de::IntoDeserializer;

        // An owned string and an absent optional are values like any other.
        let owned: de::value::StringDeserializer<de::value::Error> =
            "sk_live_owned".to_owned().into_deserializer();
        let Strict(value) = Strict::deserialize(owned).expect("an owned string");
        assert_eq!(value, Value::String("sk_live_owned".to_owned()));
        let Strict(value) = Strict::deserialize(Absent).expect("an absent optional");
        assert_eq!(value, Value::Null);
        // Anything JSON cannot express is refused, and the message names the
        // expectation, not the content.
        let bytes = de::value::BytesDeserializer::<de::value::Error>::new(b"sk_live_bytes");
        let error = Strict::deserialize(bytes)
            .err()
            .expect("bytes are not JSON");
        assert!(error.to_string().contains("a JSON value"), "{error}");
        assert!(!error.to_string().contains("sk_live"));
        // Numbers that JSON cannot hold are refused too.
        let error = Strict::deserialize(de::value::F64Deserializer::<de::value::Error>::new(
            f64::NAN,
        ))
        .err()
        .expect("NaN is not JSON");
        assert!(error.to_string().contains("number out of range"), "{error}");
        // Every scalar of a document survives.
        validate_document(
            br#"{"t": true, "i": -3, "u": 18446744073709551615, "f": 1.5, "n": null}"#,
        )
        .expect("valid");
    }

    #[test]
    fn redaction_keeps_only_the_structure() {
        let redacted =
            redact(br#"{"z": {"key": "sk_live_x", "n": [1, true, null]}, "a": 2}"#).expect("valid");
        assert_eq!(
            String::from_utf8(redacted.to_vec()).expect("utf8"),
            "{\n  \"a\": \"[REDACTED]\",\n  \"z\": {\n    \"key\": \"[REDACTED]\",\n    \"n\": [\n      \"[REDACTED]\",\n      \"[REDACTED]\",\n      \"[REDACTED]\"\n    ]\n  }\n}\n"
        );
    }

    #[test]
    fn indentation_keeps_tokens_and_order() {
        let text = br#"{"z":1.50,"a":[],"m":{},"s":"a\"b,{c}:","n":[1,{"k":true}]}"#;
        let expected = "{\n  \"z\": 1.50,\n  \"a\": [],\n  \"m\": {},\n  \"s\": \"a\\\"b,{c}:\",\n  \"n\": [\n    1,\n    {\n      \"k\": true\n    }\n  ]\n}\n";
        assert_eq!(
            String::from_utf8(indent(text).to_vec()).expect("utf8"),
            expected
        );
        assert_eq!(
            &*indent(expected.as_bytes()),
            expected.as_bytes(),
            "idempotent"
        );
    }
}
