//! Content types whose header value is not readable text are neither forms
//! nor JSON.

use http::{HeaderMap, HeaderValue, header};

use crate::{form::is_form, json::is_json};

#[test]
fn a_content_type_that_is_not_visible_ascii_is_never_accepted() {
    for raw in [
        &b"application/x-www-form-urlencoded\xff"[..],
        b"application/json\xff",
        b"\xff",
    ] {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_bytes(raw).expect("opaque header value"),
        );
        assert!(!is_form(&headers), "{raw:?}");
        assert!(!is_json(&headers), "{raw:?}");
    }
}
