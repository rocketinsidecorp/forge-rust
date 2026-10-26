//! Query-string decoding (`forge_http::Request::query`, strict
//! `application/x-www-form-urlencoded`).
//!
//! The grammar is written out a second time here, independently of the
//! decoder, and the two must agree on every input: whether it is accepted,
//! what each name and value decodes to, and which refusal comes first
//! (malformed or repeated). A decoded query survives re-encoding. A typed
//! target with `deny_unknown_fields` accepts exactly what its field types
//! accept. Input: the text after the `?`.
#![no_main]

use std::collections::HashMap;
use std::net::SocketAddr;

use bytes::Bytes;
use forge_http::{HeaderMap, Method, QueryErrorKind, Request, Uri};
use libfuzzer_sys::fuzz_target;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Search {
    q: Option<String>,
    page: Option<u32>,
    exact: Option<bool>,
}

/// `+` is a space and `%XX` an escape; the bytes must be UTF-8.
fn decode(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => out.push(b' '),
            b'%' => {
                let hex = bytes.get(index + 1..index + 3)?;
                let text = std::str::from_utf8(hex).ok()?;
                if !hex.iter().all(u8::is_ascii_hexdigit) {
                    return None;
                }
                out.push(u8::from_str_radix(text, 16).ok()?);
                index += 2;
            }
            byte => out.push(byte),
        }
        index += 1;
    }
    String::from_utf8(out).ok()
}

/// Segments in order; the first problem decides the kind, as in the decoder.
fn reference(query: &str) -> Result<Vec<(String, String)>, QueryErrorKind> {
    let mut pairs: Vec<(String, String)> = Vec::new();
    if query.is_empty() {
        return Ok(pairs);
    }
    for segment in query.split('&') {
        let Some((name, value)) = segment.split_once('=') else {
            return Err(QueryErrorKind::Malformed);
        };
        if name.is_empty() {
            return Err(QueryErrorKind::Malformed);
        }
        let name = decode(name).ok_or(QueryErrorKind::Malformed)?;
        let value = decode(value).ok_or(QueryErrorKind::Malformed)?;
        if pairs.iter().any(|(seen, _)| *seen == name) {
            return Err(QueryErrorKind::DuplicateParameter);
        }
        pairs.push((name, value));
    }
    Ok(pairs)
}

/// Everything but the unreserved characters is escaped.
fn encode(text: &str) -> String {
    let mut out = String::new();
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn build_request(query: &str) -> Option<Request> {
    let uri: Uri = format!("/search?{query}").parse().ok()?;
    Some(Request::new_for_test(
        Method::GET,
        uri,
        HeaderMap::new(),
        Bytes::new(),
        SocketAddr::from(([127, 0, 0, 1], 9)),
        uuid::Uuid::nil(),
    ))
}

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let Some(request) = build_request(text) else {
        return;
    };
    // What the decoder is handed: the URI's own query.
    let query = request.uri().query().unwrap_or_default().to_owned();
    let expected = reference(&query);

    // As a map: accepted exactly when the grammar accepts it, with the same
    // names and values.
    match (request.query::<HashMap<String, String>>(), &expected) {
        (Ok(map), Ok(pairs)) => {
            assert_eq!(map.len(), pairs.len(), "{query:?}");
            for (name, value) in pairs {
                assert_eq!(map.get(name), Some(value), "{query:?}");
            }
            // A decoded query survives being encoded again.
            let again: Vec<String> = pairs
                .iter()
                .map(|(name, value)| format!("{}={}", encode(name), encode(value)))
                .collect();
            let encoded = again.join("&");
            let reparsed = build_request(&encoded).expect("an encoded query is a URI");
            assert_eq!(
                reparsed.query::<HashMap<String, String>>().ok(),
                Some(map),
                "{query:?} -> {encoded:?}"
            );
        }
        (Err(error), Err(kind)) => {
            assert_eq!(error.kind(), *kind, "{query:?}");
            assert!(error.parameter().map_or(0, |name| name.chars().count()) <= 64);
        }
        (Ok(map), Err(kind)) => panic!("{query:?} decoded to {map:?}, expected {kind:?}"),
        (Err(error), Ok(_)) => panic!("{query:?} refused ({error}), expected to decode"),
    }

    // Into `()` only the empty query is accepted.
    assert_eq!(request.query::<()>().is_ok(), query.is_empty(), "{query:?}");

    // A typed target accepts exactly the queries its fields accept.
    let typed = request.query::<Search>();
    let accepted = expected.as_ref().is_ok_and(|pairs| {
        pairs.iter().all(|(name, value)| match name.as_str() {
            "q" => true,
            "page" => value.parse::<u32>().is_ok(),
            "exact" => value == "true" || value == "false",
            _ => false,
        })
    });
    assert_eq!(typed.is_ok(), accepted, "{query:?}: {typed:?}");
    if let (Ok(search), Ok(pairs)) = (typed, expected) {
        let find = |name: &str| pairs.iter().find(|(seen, _)| seen == name).map(|(_, v)| v);
        assert_eq!(search.q.as_ref(), find("q"));
        assert_eq!(
            search.page,
            find("page").and_then(|value| value.parse().ok())
        );
        assert_eq!(search.exact, find("exact").map(|value| value == "true"));
    }
});
