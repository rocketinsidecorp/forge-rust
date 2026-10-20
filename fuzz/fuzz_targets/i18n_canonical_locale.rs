//! Locale names and translation file names (forge_i18n::canonical_locale,
//! file_locale): request input (`locale` cookie, `Accept-Language`) must
//! never name a file.
//!
//! Properties: canonicalization changes only letter case, is stable, never
//! yields a path or file-name metacharacter, and a file named after the
//! canonical locale names its own locale.
#![no_main]

use forge_i18n::{canonical_locale, file_locale};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(name) = std::str::from_utf8(data) else {
        return;
    };
    if let Ok(canonical) = canonical_locale(name) {
        assert!(
            canonical.eq_ignore_ascii_case(name),
            "{name:?} became {canonical:?}"
        );
        assert_eq!(
            canonical_locale(&canonical).as_deref(),
            Ok(canonical.as_str())
        );
        assert!(
            canonical
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'),
            "{canonical:?} holds a metacharacter"
        );
        assert_eq!(
            file_locale(&format!("{canonical}.json")).as_deref(),
            Ok(canonical.as_str())
        );
    }
    if let Ok(locale) = file_locale(name) {
        assert!(!name.contains(['/', '\\']));
        assert!(name.ends_with(".json"));
        assert_eq!(canonical_locale(&locale).as_deref(), Ok(locale.as_str()));
    }
});
