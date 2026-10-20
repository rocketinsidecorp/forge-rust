//! Translation files (forge_i18n::Translator::from_files, ADR 0037): the
//! JSON catalogs under config/locales, embedded at build time and read by
//! `forge i18n check`.
//!
//! Input: a catalog file. Properties: a file either fails to load or every
//! key it defines translates without a panic, and interpolated values are
//! HTML-escaped in `_html` keys and in `translate_html`.
#![no_main]

use forge_i18n::{Args, Options, Translator};
use libfuzzer_sys::fuzz_target;

const PAYLOAD: &str = "<i>zq7&\"'</i>";

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let files = [("en.json", text)];
    let Ok(translator) = Translator::from_files(&files, Options::new()) else {
        return;
    };
    // A catalog that spells the payload itself would make the check below a
    // false alarm.
    let literal = text.contains("zq7");
    for key in translator.keys("en") {
        for count in [0_i64, 1, 2, 5, 11, 100] {
            let args = Args::new().with("name", PAYLOAD).count(count);
            let _ = translator.translate("en", &key, &args);
            let html = translator.translate_html("en", &key, &args);
            assert!(
                literal || !html.contains(PAYLOAD),
                "{key}: an interpolated value reached HTML unescaped"
            );
        }
    }
});
