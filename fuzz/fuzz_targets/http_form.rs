//! Form bodies (`forge_http::Request::form` and `form_value`, strict
//! `application/x-www-form-urlencoded` with HTML form semantics).
//!
//! As for the query string, the grammar is written out a second time here and
//! must agree with the decoder on every input: the media type check, the
//! 1000 pair bound, which refusal comes first, what every value decodes to,
//! and that the framework fields (`csrf_token`, `_method`) never reach the
//! application's type. A typed target accepts exactly what its field types
//! accept: scalars once, sequences any number of times, blank optionals as
//! `None`, checkboxes as `true`/`on`/`1` and `false`/`0`. Input: one byte
//! choosing the `Content-Type`, then the body.
#![no_main]

use std::collections::BTreeMap;
use std::net::SocketAddr;

use bytes::Bytes;
use forge_http::{
    FORM_CONTENT_TYPE, FormErrorKind, HeaderMap, HeaderValue, MAX_FORM_FIELDS, Method, Request,
    Uri, header,
};
use libfuzzer_sys::fuzz_target;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Product {
    name: String,
    price: u32,
    #[serde(default)]
    published: bool,
    notes: Option<String>,
    stock: Option<u32>,
    #[serde(default)]
    tags: Vec<String>,
}

type Pairs = Vec<(String, String)>;

/// The values of each name, in order, built once per input.
type ByName<'p> = BTreeMap<&'p str, Vec<&'p str>>;

fn by_name(pairs: &Pairs) -> ByName<'_> {
    let mut map = ByName::new();
    for (name, value) in pairs {
        map.entry(name.as_str()).or_default().push(value.as_str());
    }
    map
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
                if !hex.iter().all(u8::is_ascii_hexdigit) {
                    return None;
                }
                out.push(u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?);
                index += 2;
            }
            byte => out.push(byte),
        }
        index += 1;
    }
    String::from_utf8(out).ok()
}

/// The body as `name`/`value` pairs in order, or the first refusal, in the
/// order the decoder checks: UTF-8, the pair bound, then segment by segment.
fn reference(body: &[u8]) -> Result<Pairs, FormErrorKind> {
    let text = std::str::from_utf8(body).map_err(|_| FormErrorKind::Malformed)?;
    let mut pairs = Pairs::new();
    if text.is_empty() {
        return Ok(pairs);
    }
    if text.split('&').count() > MAX_FORM_FIELDS {
        return Err(FormErrorKind::TooManyFields);
    }
    for segment in text.split('&') {
        let (name, value) = segment
            .split_once('=')
            .filter(|(name, _)| !name.is_empty())
            .ok_or(FormErrorKind::Malformed)?;
        let name = decode(name).ok_or(FormErrorKind::Malformed)?;
        let value = decode(value).ok_or(FormErrorKind::Malformed)?;
        pairs.push((name, value));
    }
    Ok(pairs)
}

fn values<'p>(map: &ByName<'p>, name: &str) -> Vec<&'p str> {
    map.get(name).cloned().unwrap_or_default()
}

/// Whether `Product` accepts the pairs (reserved fields already removed).
fn product_accepts(map: &ByName<'_>) -> bool {
    let known = ["name", "price", "published", "notes", "stock", "tags"];
    if map.keys().any(|name| !known.contains(name)) {
        return false;
    }
    let once = |name: &str| matches!(values(map, name).as_slice(), [_]);
    let at_most_once = |name: &str| values(map, name).len() <= 1;
    let number = |name: &str| {
        values(map, name)
            .first()
            .is_none_or(|v| v.parse::<u32>().is_ok())
    };
    let optional_number = |name: &str| match values(map, name).as_slice() {
        [] => true,
        [blank] if blank.is_empty() => true,
        [value] => value.parse::<u32>().is_ok(),
        _ => false,
    };
    let checkbox = match values(map, "published").as_slice() {
        [] => true,
        [value] => matches!(*value, "true" | "on" | "1" | "false" | "0"),
        _ => false,
    };
    once("name")
        && once("price")
        && number("price")
        && checkbox
        && at_most_once("notes")
        && optional_number("stock")
}

/// The `Content-Type` headers of each variant and whether they make a form.
fn content_types(choice: u8) -> (Vec<&'static str>, bool) {
    match choice % 9 {
        0 => (vec![FORM_CONTENT_TYPE], true),
        1 => (
            vec!["application/x-www-form-urlencoded; charset=UTF-8"],
            true,
        ),
        2 => (
            vec!["application/x-www-form-urlencoded;charset=\"utf-8\""],
            true,
        ),
        3 => (vec!["Application/X-WWW-Form-Urlencoded"], true),
        4 => (
            vec!["application/x-www-form-urlencoded; charset=latin1"],
            false,
        ),
        5 => (vec!["application/x-www-form-urlencoded; boundary=x"], false),
        6 => (vec!["text/plain"], false),
        7 => (vec![], false),
        _ => (vec![FORM_CONTENT_TYPE, FORM_CONTENT_TYPE], false),
    }
}

fuzz_target!(|data: &[u8]| {
    let Some((&choice, body)) = data.split_first() else {
        return;
    };
    let (content_types, is_form) = content_types(choice);
    let mut headers = HeaderMap::new();
    for content_type in content_types {
        headers.append(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    }
    let request = Request::new_for_test(
        Method::POST,
        Uri::from_static("/products"),
        headers,
        Bytes::copy_from_slice(body),
        SocketAddr::from(([127, 0, 0, 1], 9)),
        uuid::Uuid::nil(),
    );
    assert_eq!(request.has_form_body(), is_form);

    let typed = request.form::<Product>();
    if !is_form {
        let error = typed.expect_err("not a form");
        assert_eq!(error.kind(), FormErrorKind::UnsupportedMediaType);
        let error = request.form_value("name").expect_err("not a form");
        assert_eq!(error.kind(), FormErrorKind::UnsupportedMediaType);
        return;
    }

    let pairs = match reference(body) {
        Ok(pairs) => pairs,
        Err(kind) => {
            // A body that is not a form is refused whatever it is read as.
            assert_eq!(typed.expect_err("refused").kind(), kind);
            assert_eq!(
                request.form_value("name").expect_err("refused").kind(),
                kind
            );
            return;
        }
    };

    // `form_value` sees every field, the reserved ones included.
    let all = by_name(&pairs);
    let mut names: Vec<&str> = vec!["name", "price", "tags", "csrf_token", "_method", "absent"];
    names.extend(all.keys().take(16));
    for name in names {
        let found = values(&all, name);
        match request.form_value(name) {
            Ok(None) => assert!(found.is_empty(), "{name:?} is present in {pairs:?}"),
            Ok(Some(value)) => assert_eq!(found, [value.as_str()], "{name:?}"),
            Err(error) => {
                assert_eq!(error.kind(), FormErrorKind::DuplicateField, "{name:?}");
                assert!(found.len() > 1, "{name:?} is not repeated in {pairs:?}");
            }
        }
    }

    // The typed target never sees the framework fields.
    let mut application = all.clone();
    application.remove("csrf_token");
    application.remove("_method");
    let accepted = product_accepts(&application);
    assert_eq!(typed.is_ok(), accepted, "{pairs:?}: {typed:?}");
    if let Ok(product) = typed {
        assert_eq!(
            Some(product.name.as_str()),
            values(&application, "name").first().copied()
        );
        assert_eq!(
            Some(product.price),
            values(&application, "price")
                .first()
                .and_then(|v| v.parse().ok())
        );
        assert_eq!(
            product.tags.iter().map(String::as_str).collect::<Vec<_>>(),
            values(&application, "tags")
        );
        let published = values(&application, "published");
        assert_eq!(
            product.published,
            matches!(published.first().copied(), Some("true" | "on" | "1"))
        );
        let notes = values(&application, "notes");
        assert_eq!(
            product.notes.as_deref(),
            notes.first().copied().filter(|value| !value.is_empty())
        );
        let stock = values(&application, "stock");
        assert_eq!(
            product.stock,
            stock.first().and_then(|value| value.parse().ok())
        );
    }
});
