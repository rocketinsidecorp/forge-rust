//! Choosing a locale: matching a requested name to an available locale and
//! reading `Accept-Language` (RFC 9110 / RFC 4647). The HTTP layer
//! (`forge_views::LocaleNegotiation`) applies the order of sources.

use crate::Translator;
use crate::locale::{canonical_locale, is_digits, language, parents};

/// `Accept-Language` headers longer than this are ignored.
pub const MAX_ACCEPT_LANGUAGE_BYTES: usize = 4096;
/// Only the first entries of `Accept-Language` are read.
pub const MAX_ACCEPT_LANGUAGE_ENTRIES: usize = 32;

/// One `Accept-Language` entry.
struct LanguageRange<'a> {
    tag: &'a str,
    /// In thousandths.
    quality: u16,
}

/// The language ranges of an `Accept-Language` header with a non-zero
/// quality, highest quality first (ties keep header order). Malformed
/// entries are skipped; a header longer than 4096 bytes is ignored, and
/// only its first 32 entries are read. `*` is kept as a range.
///
/// ```
/// assert_eq!(
///     forge_i18n::parse_accept_language("en;q=0.5, pt-BR, fr;q=0"),
///     ["pt-BR", "en"]
/// );
/// ```
#[must_use]
pub fn parse_accept_language(header: &str) -> Vec<String> {
    ranges(header)
        .into_iter()
        .map(|range| range.tag.to_owned())
        .collect()
}

fn ranges(header: &str) -> Vec<LanguageRange<'_>> {
    if header.is_empty() || header.len() > MAX_ACCEPT_LANGUAGE_BYTES {
        return Vec::new();
    }
    let mut ranges = Vec::new();
    for entry in header.split(',').take(MAX_ACCEPT_LANGUAGE_ENTRIES) {
        let mut parts = entry.split(';');
        let tag = parts.next().unwrap_or_default().trim();
        if !is_language_range(tag) {
            continue;
        }
        let mut quality = Some(1000);
        for parameter in parts {
            let Some((name, value)) = parameter.trim().split_once('=') else {
                continue;
            };
            if name.trim().eq_ignore_ascii_case("q") {
                quality = parse_quality(value.trim());
            }
        }
        match quality {
            Some(quality) if quality > 0 => ranges.push(LanguageRange { tag, quality }),
            _ => {}
        }
    }
    // Stable: ties keep header order.
    ranges.sort_by_key(|range| std::cmp::Reverse(range.quality));
    ranges
}

/// `*` and RFC 4647 language ranges: 1-8 letters, then `-` and 1-8
/// letters or digits per subtag, at most 64 bytes.
fn is_language_range(tag: &str) -> bool {
    if tag == "*" {
        return true;
    }
    if tag.is_empty() || tag.len() > 64 {
        return false;
    }
    tag.split('-').enumerate().all(|(index, subtag)| {
        !subtag.is_empty()
            && subtag.len() <= 8
            && subtag
                .bytes()
                .all(|byte| byte.is_ascii_alphabetic() || (index > 0 && byte.is_ascii_digit()))
    })
}

/// An RFC 9110 qvalue (`0`, `0.5`, `1.000`) in thousandths.
fn parse_quality(value: &str) -> Option<u16> {
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    if (whole != "0" && whole != "1") || fraction.len() > 3 || !is_digits(&format!("{fraction}0")) {
        return None;
    }
    let thousandths: u16 = format!("{fraction:0<3}").parse().ok()?;
    if whole == "1" {
        return (thousandths == 0).then_some(1000);
    }
    Some(thousandths)
}

impl Translator {
    /// The available locale that best serves a requested one: the locale
    /// itself, else its closest parent (`pt-PT` → `pt`), else another
    /// variant of its language (`pt` → `pt-BR`, the default locale's
    /// variant first, then the first in sorted order), else the default
    /// locale. Invalid names get the default locale.
    #[must_use]
    pub fn match_locale(&self, requested: &str) -> &str {
        self.find_locale(requested)
            .unwrap_or(self.default_locale.as_str())
    }

    /// Like [`Translator::match_locale`], but `None` instead of the default
    /// locale when nothing matches.
    #[must_use]
    pub fn find_locale(&self, requested: &str) -> Option<&str> {
        let canonical = canonical_locale(requested).ok()?;
        for candidate in parents(&canonical) {
            if let Some((available, _)) = self.catalog.get_key_value(candidate) {
                return Some(available);
            }
        }
        let wanted = language(&canonical);
        if language(&self.default_locale) == wanted {
            return Some(&self.default_locale);
        }
        self.locales()
            .iter()
            .find(|available| language(available) == wanted)
            .map(String::as_str)
    }

    /// The available locale named exactly by `name` (ignoring case), such
    /// as a path segment or a language switcher's choice.
    #[must_use]
    pub fn exact_locale(&self, name: &str) -> Option<&str> {
        let canonical = canonical_locale(name).ok()?;
        self.catalog
            .get_key_value(&canonical)
            .map(|(available, _)| available.as_str())
    }

    /// The available locale for an `Accept-Language` header, by RFC 4647
    /// lookup with truncation of unsupported subtags, then by language. `*`
    /// picks the default locale.
    #[must_use]
    pub fn match_accept_language(&self, header: &str) -> Option<&str> {
        for range in ranges(header) {
            if range.tag == "*" {
                return Some(&self.default_locale);
            }
            let mut tag = range.tag;
            loop {
                if let Some(locale) = self.find_locale(tag) {
                    return Some(locale);
                }
                match tag.rfind('-') {
                    Some(cut) => tag = &tag[..cut],
                    None => break,
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accept_language_parsing_matches_forge_go() {
        let cases: Vec<(&str, Vec<&str>)> = vec![
            ("", vec![]),
            ("pt-BR,pt;q=0.9,en;q=0.8", vec!["pt-BR", "pt", "en"]),
            ("en;q=0.5, fr ; q=1, de", vec!["fr", "de", "en"]),
            ("en;q=0, fr", vec!["fr"]),
            ("*", vec!["*"]),
            (
                "en;q=1.5, fr;q=-1, de;q=abc, it;q=0.1234, es;q=0.123",
                vec!["es"],
            ),
            ("e1n, 12, en_US, en-, -en, en--US, , ;q=1, fr", vec!["fr"]),
            (
                "en-US-x-private;q=0.3, zh-Hant-TW",
                vec!["zh-Hant-TW", "en-US-x-private"],
            ),
            ("en;Q=0.2,fr;level=1", vec!["fr", "en"]),
            ("en;q=1.000,fr;q=1.0001", vec!["en"]),
            ("en;q=0., fr;q=1.", vec!["fr"]),
        ];
        for (header, want) in cases {
            assert_eq!(parse_accept_language(header), want, "{header:?}");
        }
        let oversized = format!("en,{}", "x".repeat(4096));
        assert!(parse_accept_language(&oversized).is_empty());
        let many = format!("{}fr", "xx,".repeat(40));
        assert!(!parse_accept_language(&many).contains(&"fr".to_owned()));
    }
}
