//! The document's canonical bytes: object keys sorted, two-space indent, a
//! trailing newline and Go's `encoding/json` escaping with HTML escaping off,
//! so Forge-Go and Forge-Rust print equivalent documents identically.

use std::fmt::Write as _;

use serde_json::Value;

/// Writes `value` canonically.
pub(crate) fn to_canonical(value: &Value) -> Vec<u8> {
    let mut out = String::new();
    write_value(value, 0, &mut out);
    out.push('\n');
    out.into_bytes()
}

fn write_value(value: &Value, depth: usize, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(flag) => out.push_str(if *flag { "true" } else { "false" }),
        Value::Number(number) => {
            let _ = write!(out, "{number}");
        }
        Value::String(text) => write_string(text, out),
        Value::Array(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                newline(depth + 1, out);
                write_value(item, depth + 1, out);
            }
            newline(depth, out);
            out.push(']');
        }
        Value::Object(members) => {
            if members.is_empty() {
                out.push_str("{}");
                return;
            }
            let mut keys: Vec<&String> = members.keys().collect();
            keys.sort_unstable();
            out.push('{');
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                newline(depth + 1, out);
                write_string(key, out);
                out.push_str(": ");
                if let Some(member) = members.get(key) {
                    write_value(member, depth + 1, out);
                }
            }
            newline(depth, out);
            out.push('}');
        }
    }
}

fn newline(depth: usize, out: &mut String) {
    out.push('\n');
    for _ in 0..depth {
        out.push_str("  ");
    }
}

/// Go's string escaping (`SetEscapeHTML(false)`): quote, backslash, the
/// short escapes for `\b \f \n \r \t`, `\u00XX` for other control
/// characters, and ` `/` `.
fn write_string(text: &str, out: &mut String) {
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            ch if u32::from(ch) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", u32::from(ch));
            }
            ch => out.push(ch),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn keys_are_sorted_indented_and_escaped_like_go() {
        let value = json!({
            "b": [1, "x"],
            "a": {"z": null, "y": true},
            "e": [],
            "f": {},
            "s": "<a href=\"x\">\u{1}\u{2028}\t\\",
        });
        let text = String::from_utf8(to_canonical(&value)).unwrap_or_default();
        assert_eq!(
            text,
            "{\n  \"a\": {\n    \"y\": true,\n    \"z\": null\n  },\n  \"b\": [\n    1,\n    \"x\"\n  ],\n  \
             \"e\": [],\n  \"f\": {},\n  \"s\": \"<a href=\\\"x\\\">\\u0001\\u2028\\t\\\\\"\n}\n"
        );
    }
}
