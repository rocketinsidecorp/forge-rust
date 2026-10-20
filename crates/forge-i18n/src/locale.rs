//! Locale names: the BCP 47 subset Forge accepts, canonicalized.

use crate::LoadError;

/// The longest locale name the grammar accepts: a 3-letter language, a
/// script and a 3-digit region (`zzz-Zzzz-999`).
const MAX_LOCALE_LENGTH: usize = 12;

/// Validates `name` against the subset of BCP 47 Forge supports and returns
/// its canonical spelling: a 2 or 3 letter language (lowercase), an
/// optional 4 letter script (titlecase) and an optional region of 2 letters
/// (uppercase) or 3 digits, separated by `-`. `pt-br` becomes `pt-BR`,
/// `zh-hant-tw` becomes `zh-Hant-TW`. `_` separators, extensions, variants
/// and private-use subtags are refused, so a valid name never contains a
/// path separator, a dot or anything else a file name could misuse.
///
/// ```
/// assert_eq!(forge_i18n::canonical_locale("pt-br").as_deref(), Ok("pt-BR"));
/// assert!(forge_i18n::canonical_locale("../en").is_err());
/// ```
pub fn canonical_locale(name: &str) -> Result<String, LoadError> {
    let invalid = || {
        LoadError::new(format!(
            "invalid locale {:?}: expected a language such as en or pt-BR",
            truncate(name)
        ))
    };
    if name.is_empty() || name.len() > MAX_LOCALE_LENGTH {
        return Err(invalid());
    }
    let parts: Vec<&str> = name.split('-').collect();
    let language = parts.first().copied().unwrap_or_default();
    if parts.len() > 3 || !is_letters(language) || !(2..=3).contains(&language.len()) {
        return Err(invalid());
    }
    let mut canonical = language.to_ascii_lowercase();
    let mut rest = &parts[1..];
    if let Some(script) = rest.first()
        && script.len() == 4
        && is_letters(script)
    {
        canonical.push('-');
        canonical.push_str(&script[..1].to_ascii_uppercase());
        canonical.push_str(&script[1..].to_ascii_lowercase());
        rest = &rest[1..];
    }
    if let Some(region) = rest.first() {
        canonical.push('-');
        if region.len() == 2 && is_letters(region) {
            canonical.push_str(&region.to_ascii_uppercase());
        } else if region.len() == 3 && is_digits(region) {
            canonical.push_str(region);
        } else {
            return Err(LoadError::new(format!(
                "invalid locale {:?}: the region must be 2 letters or 3 digits",
                truncate(name)
            )));
        }
        rest = &rest[1..];
    }
    if !rest.is_empty() {
        return Err(LoadError::new(format!(
            "invalid locale {:?}: expected language[-Script][-REGION]",
            truncate(name)
        )));
    }
    Ok(canonical)
}

/// The fallback chain of a canonical locale, most specific first:
/// `zh-Hant-TW` gives `zh-Hant-TW`, `zh-Hant`, `zh`.
pub(crate) fn parents(locale: &str) -> impl Iterator<Item = &str> {
    let mut next = Some(locale);
    std::iter::from_fn(move || {
        let current = next?;
        next = current.rfind('-').map(|cut| &current[..cut]);
        Some(current)
    })
}

/// The language subtag of a canonical locale.
pub(crate) fn language(locale: &str) -> &str {
    locale.split('-').next().unwrap_or(locale)
}

pub(crate) fn is_letters(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_alphabetic())
}

pub(crate) fn is_digits(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit())
}

/// Keeps messages short when the input came from a request: at most 32
/// bytes (on a character boundary), then `...`.
pub(crate) fn truncate(text: &str) -> String {
    if text.len() <= 32 {
        return text.to_owned();
    }
    let mut end = 32;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalizes_the_supported_subset() {
        for (input, want) in [
            ("en", "en"),
            ("EN", "en"),
            ("pt-br", "pt-BR"),
            ("pt-BR", "pt-BR"),
            ("zh-hant-tw", "zh-Hant-TW"),
            ("es-419", "es-419"),
            ("sr-Latn", "sr-Latn"),
            ("fil", "fil"),
        ] {
            assert_eq!(
                canonical_locale(input).ok().as_deref(),
                Some(want),
                "{input}"
            );
        }
    }

    #[test]
    fn refuses_everything_else() {
        for input in [
            "",
            "e",
            "english",
            "pt_BR",
            "../en",
            "en/../../etc/passwd",
            "en.json",
            "en-",
            "-en",
            "en--US",
            "en-US-x-private",
            "en-USA",
            "en-1",
            "en-Latn-US-extra",
            "e1",
            "en-12",
            "pt-BR\0",
            "%2e%2e",
            "en US",
            "en\\x",
            "éé",
        ] {
            assert!(canonical_locale(input).is_err(), "{input:?}");
        }
    }

    #[test]
    fn parents_walk_to_the_language() {
        assert_eq!(
            parents("zh-Hant-TW").collect::<Vec<_>>(),
            ["zh-Hant-TW", "zh-Hant", "zh"]
        );
        assert_eq!(parents("en").collect::<Vec<_>>(), ["en"]);
        assert_eq!(language("pt-BR"), "pt");
    }

    #[test]
    fn truncate_respects_character_boundaries() {
        assert_eq!(truncate("short"), "short");
        let long = "é".repeat(40);
        let cut = truncate(&long);
        assert!(cut.ends_with("..."));
        assert!(cut.len() <= 35);
    }
}
