//! Canonical bytes beyond the common cases.

use serde_json::json;

use super::*;

fn canonical(value: &Value) -> String {
    String::from_utf8(to_canonical(value)).expect("the output is text")
}

#[test]
fn scalars_are_written_like_go() {
    assert_eq!(canonical(&json!(null)), "null\n");
    assert_eq!(canonical(&json!(false)), "false\n");
    assert_eq!(canonical(&json!(true)), "true\n");
    assert_eq!(canonical(&json!(-12)), "-12\n");
    assert_eq!(canonical(&json!(1.5)), "1.5\n");
    assert_eq!(
        canonical(&json!(18_446_744_073_709_551_615_u64)),
        "18446744073709551615\n"
    );
    assert_eq!(canonical(&json!([])), "[]\n");
    assert_eq!(canonical(&json!({})), "{}\n");
}

#[test]
fn every_short_escape_and_control_character_is_escaped_and_html_is_not() {
    assert_eq!(
        canonical(&json!("\u{8}\u{c}\n\r\t\"\\")),
        "\"\\b\\f\\n\\r\\t\\\"\\\\\"\n"
    );
    assert_eq!(canonical(&json!("\u{0}\u{1f}")), "\"\\u0000\\u001f\"\n");
    assert_eq!(
        canonical(&json!("\u{2028}\u{2029}")),
        "\"\\u2028\\u2029\"\n"
    );
    // HTML escaping is off, and other non-ASCII text is written as is.
    assert_eq!(
        canonical(&json!("<&>é\u{1f600}\u{7f}")),
        "\"<&>é\u{1f600}\u{7f}\"\n"
    );
}

#[test]
fn nested_containers_indent_by_two_spaces_per_level_and_sort_keys_at_every_level() {
    let value = json!({"b": {"d": [ {"y": 1, "x": 2} ], "c": []}, "a": null});
    assert_eq!(
        canonical(&value),
        "{\n  \"a\": null,\n  \"b\": {\n    \"c\": [],\n    \"d\": [\n      {\n        \"x\": 2,\n        \"y\": 1\n      }\n    ]\n  }\n}\n"
    );
}
