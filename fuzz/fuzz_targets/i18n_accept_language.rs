//! `Accept-Language` and locale matching (forge_i18n, ADR 0037).
//!
//! Properties: at most 32 well-formed ranges; every range is `*` or a short
//! `-`-separated alphanumeric tag (never a path or file-name character);
//! negotiation always returns an available locale.
#![no_main]

use forge_i18n::{Options, Translator, parse_accept_language};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(header) = std::str::from_utf8(data) else {
        return;
    };
    let ranges = parse_accept_language(header);
    assert!(ranges.len() <= forge_i18n::MAX_ACCEPT_LANGUAGE_ENTRIES);
    for range in &ranges {
        assert!(range.len() <= 64);
        assert!(
            range == "*"
                || range.split('-').all(|part| (1..=8).contains(&part.len())
                    && part.bytes().all(|byte| byte.is_ascii_alphanumeric())),
            "malformed range {range:?}"
        );
    }
    let files = [
        ("en.json", r#"{"en":{"hello":"hello"}}"#),
        ("pt-BR.json", r#"{"pt-BR":{"hello":"olá"}}"#),
        ("es.json", r#"{"es":{"hello":"hola"}}"#),
    ];
    let Ok(translator) = Translator::from_files(&files, Options::new()) else {
        return;
    };
    if let Some(matched) = translator.match_accept_language(header) {
        assert!(translator.locales().iter().any(|locale| locale == matched));
    }
    let matched = translator.match_locale(header);
    assert!(translator.locales().iter().any(|locale| locale == matched));
});
