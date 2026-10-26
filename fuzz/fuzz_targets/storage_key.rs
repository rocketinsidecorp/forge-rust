//! Storage keys (`forge_storage::StorageKey::parse`, ADR 0036).
//!
//! A key names a blob below a storage root in every adapter, so the property
//! that matters is that a key that parses can never leave that root: it is
//! relative, made of plain segments, and joining it to a directory only ever
//! descends. The grammar is also written out a second time here, independently
//! of the parser, and the two must agree on every input. Input: the key text,
//! with a NUL splitting it from an upload prefix for `StorageKey::unique`.
#![no_main]

use std::path::{Component, Path};

use forge_storage::{MAX_KEY_LENGTH, StorageKey};
use libfuzzer_sys::fuzz_target;

const MAX_SEGMENT_LENGTH: usize = 128;
const MAX_KEY_DEPTH: usize = 8;

/// The documented grammar, written without looking at the parser: 1 to 8
/// non-empty segments of at most 128 ASCII letters, digits, `.`, `_` and `-`,
/// none starting with a dot, 512 bytes at most.
fn grammar(key: &str) -> bool {
    if key.is_empty() || key.len() > MAX_KEY_LENGTH {
        return false;
    }
    let segments: Vec<&str> = key.split('/').collect();
    segments.len() <= MAX_KEY_DEPTH
        && segments.iter().all(|segment| {
            !segment.is_empty()
                && segment.len() <= MAX_SEGMENT_LENGTH
                && !segment.starts_with('.')
                && segment
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        })
}

/// What every accepted key must satisfy, whatever the grammar says.
fn check_safe(key: &StorageKey) {
    let text = key.as_str();
    assert_eq!(key.to_string(), text);
    assert!(!text.starts_with('/'), "absolute key {text:?}");
    assert!(!text.ends_with('/'), "directory key {text:?}");
    assert!(!text.contains('\\'), "backslash in {text:?}");
    assert!(!text.contains('\0'), "NUL in {text:?}");
    assert!(!text.contains('%'), "percent escape in {text:?}");
    assert!(text.is_ascii() && !text.bytes().any(|byte| byte.is_ascii_control()));
    assert!(text.len() <= MAX_KEY_LENGTH);
    let segments: Vec<&str> = key.segments().collect();
    assert_eq!(segments.join("/"), text, "segments are the key");
    assert!(segments.len() <= MAX_KEY_DEPTH);
    for segment in &segments {
        // `..` as a segment is the traversal; "a..b" is an ordinary name.
        assert!(
            !segment.is_empty() && *segment != "." && *segment != "..",
            "dot or empty segment in {text:?}"
        );
        assert!(!segment.starts_with('.'), "hidden segment in {text:?}");
        assert!(segment.len() <= MAX_SEGMENT_LENGTH);
    }
    // The only way a key reaches the file system is joined to a root: it
    // must only descend, whatever the platform's path rules.
    let components: Vec<Component<'_>> = Path::new(text).components().collect();
    assert_eq!(components.len(), segments.len(), "{text:?}");
    assert!(
        components
            .iter()
            .all(|component| matches!(component, Component::Normal(_))),
        "{text:?} is not a plain relative path"
    );
}

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let (key, prefix) = match text.split_once('\0') {
        Some((key, prefix)) => (key, Some(prefix)),
        None => (text, None),
    };

    // The same error for every refusal: it never repeats what was refused.
    let refused = format!("{:?}", StorageKey::parse("/").unwrap_err());
    match StorageKey::parse(key) {
        Ok(parsed) => {
            assert!(grammar(key), "accepted {key:?}");
            assert_eq!(parsed.as_str(), key);
            check_safe(&parsed);
            // Parsing is stable: the printed key is the key.
            assert_eq!(
                StorageKey::parse(parsed.as_str()).ok().as_ref(),
                Some(&parsed)
            );
        }
        Err(error) => {
            assert!(!grammar(key), "refused {key:?}");
            assert_eq!(
                format!("{error:?}"),
                refused,
                "the error depends on the key"
            );
        }
    }

    // `unique` appends a UUID to a prefix that is itself validated like a key.
    if let Some(prefix) = prefix {
        match StorageKey::unique(Some(prefix)) {
            Ok(unique) => {
                check_safe(&unique);
                let (head, id) = unique.as_str().rsplit_once('/').expect("prefix and id");
                assert_eq!(head, prefix);
                assert_eq!(id.len(), 36, "a UUID: {id:?}");
                assert_eq!(id.as_bytes()[14], b'7', "UUIDv7: {id:?}");
            }
            Err(_) => assert!(!grammar(&format!(
                "{prefix}/0192b2c4-7d1e-7000-8000-000000000000"
            ))),
        }
    }
});
