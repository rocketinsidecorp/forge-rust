//! The translator: lookup, plural forms, interpolation, escaping and the
//! missing-translation rule (ADR 0037).

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Mutex, PoisonError};

use tracing::warn;

use crate::LoadError;
use crate::args::{Args, Count, Value, is_argument_name};
use crate::catalog::{
    Catalog, MAX_KEY_LENGTH, Node, Tree, find, is_plural, is_segment, leaf_keys, read_catalog,
    read_dir,
};
use crate::locale::{canonical_locale, parents, truncate};
use crate::plural::{Category, PluralRule, rule_in, used_categories};

/// The default locale when [`Options::default_locale`] is not set.
pub const DEFAULT_LOCALE: &str = "en";

/// How many distinct missing translations are logged, so keys built from
/// request input cannot grow memory or flood the log.
pub const MAX_WARNINGS: usize = 1024;

/// How a [`Translator`] loads and behaves.
#[derive(Clone, Debug, Default)]
pub struct Options {
    default_locale: Option<String>,
    show_missing: bool,
    plural_rules: Vec<(String, PluralRule)>,
}

impl Options {
    /// The defaults: default locale `en`, missing translations fall back.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The locale used when a request asks for none Forge has, and the last
    /// fallback for missing translations in production. It must have a
    /// translation file.
    #[must_use]
    pub fn default_locale(mut self, locale: impl Into<String>) -> Self {
        self.default_locale = Some(locale.into());
        self
    }

    /// Makes a missing translation visible as `translation missing:
    /// LOCALE.KEY` (development and test). Otherwise (production) the
    /// default locale's text is used, then the key's last segment humanized
    /// (`welcome_back` gives "Welcome back"). Both log a warning once per
    /// locale and key.
    #[must_use]
    pub fn show_missing(mut self, show: bool) -> Self {
        self.show_missing = show;
        self
    }

    /// Adds or replaces the plural rule of a locale (and the locales below
    /// it), for languages without a built-in rule.
    #[must_use]
    pub fn plural_rule(mut self, locale: impl Into<String>, rule: PluralRule) -> Self {
        self.plural_rules.push((locale.into(), rule));
        self
    }
}

#[derive(Debug, Default)]
struct Warnings {
    seen: HashSet<String>,
    suppressed: u64,
}

/// An application's translations. Immutable after loading and safe to
/// share between threads (wrap it in an `Arc`).
///
/// ```
/// use forge_i18n::{Args, Options, Translator};
///
/// let translator = Translator::from_files(
///     &[
///         ("en.json", r#"{"en": {"inbox": {"one": "%{count} message", "other": "%{count} messages"}}}"#),
///         ("pt-BR.json", r#"{"pt-BR": {"inbox": {"one": "%{count} mensagem", "other": "%{count} mensagens"}}}"#),
///     ],
///     Options::new(),
/// )?;
/// assert_eq!(translator.translate("pt-br", "inbox", &Args::new().count(0)), "0 mensagem");
/// assert_eq!(translator.translate("en", "inbox", &Args::new().count(2)), "2 messages");
/// # Ok::<(), forge_i18n::LoadError>(())
/// ```
#[derive(Debug)]
pub struct Translator {
    pub(crate) default_locale: String,
    locales: Vec<String>,
    pub(crate) catalog: Catalog,
    show_missing: bool,
    rules: Vec<(String, PluralRule)>,
    warnings: Mutex<Warnings>,
}

impl Translator {
    /// Loads translation files given as (file name, contents), such as the
    /// table `forge_views::build::embed_locales` writes: `LOCALE.json` and
    /// `SCOPE.LOCALE.json`, each one object whose only key is the locale.
    /// Files that do not end in `.json` are ignored. The files of one
    /// locale are merged; a key defined twice is an error. The available
    /// locales are the ones with files.
    pub fn from_files<N, C>(files: &[(N, C)], options: Options) -> Result<Self, LoadError>
    where
        N: AsRef<str>,
        C: AsRef<str>,
    {
        let default_locale =
            canonical_locale(options.default_locale.as_deref().unwrap_or(DEFAULT_LOCALE))
                .map_err(|error| LoadError::new(format!("default locale: {}", error.message())))?;
        let catalog = read_catalog(files)?;
        if !catalog.contains_key(&default_locale) {
            return Err(LoadError::new(format!(
                "the default locale {default_locale} has no translation file \
                 ({default_locale}.json)"
            )));
        }
        let mut rules = Vec::with_capacity(options.plural_rules.len());
        for (locale, rule) in options.plural_rules {
            let canonical = canonical_locale(&locale)
                .map_err(|error| LoadError::new(format!("plural rule: {}", error.message())))?;
            rules.retain(|(existing, _): &(String, PluralRule)| *existing != canonical);
            rules.push((canonical, rule));
        }
        let mut locales: Vec<String> = catalog.keys().cloned().collect();
        locales.sort();
        Ok(Self {
            default_locale,
            locales,
            catalog,
            show_missing: options.show_missing,
            rules,
            warnings: Mutex::new(Warnings::default()),
        })
    }

    /// Reads the `*.json` files of `dir` (subdirectories, dot files and
    /// other files are ignored; symbolic links are refused) and loads them
    /// like [`Translator::from_files`]. Applications embed their files at
    /// build time instead; this is for tools and tests.
    pub fn load_dir(dir: impl AsRef<Path>, options: Options) -> Result<Self, LoadError> {
        let files = read_dir(dir.as_ref())?;
        Self::from_files(&files, options)
    }

    /// The default locale.
    #[must_use]
    pub fn default_locale(&self) -> &str {
        &self.default_locale
    }

    /// The available locales, sorted.
    #[must_use]
    pub fn locales(&self) -> &[String] {
        &self.locales
    }

    /// The translation keys of a locale, sorted; plural forms count as one
    /// key. An unknown locale has none.
    #[must_use]
    pub fn keys(&self, locale: &str) -> Vec<String> {
        self.catalog.get(locale).map(leaf_keys).unwrap_or_default()
    }

    /// The plural forms the locale's rule uses for the counts 0 to 1000:
    /// the forms each plural translation of that locale should provide.
    #[must_use]
    pub fn plural_categories(&self, locale: &str) -> Vec<Category> {
        used_categories(rule_in(&self.rules, locale))
    }

    /// The forms a plural translation defines in a locale, or `None` when
    /// `key` is not a plural translation there.
    #[must_use]
    pub fn plural_forms(&self, locale: &str, key: &str) -> Option<Vec<Category>> {
        let Node::Map(forms) = find(self.catalog.get(locale)?, key)? else {
            return None;
        };
        if !is_plural(forms) {
            return None;
        }
        Some(
            Category::ALL
                .into_iter()
                .filter(|category| forms.contains_key(category.as_str()))
                .collect(),
        )
    }

    /// Whether `key` has a translation in `locale` itself (not through a
    /// fallback).
    #[must_use]
    pub fn has(&self, locale: &str, key: &str) -> bool {
        let Some(tree) = self.catalog.get(locale) else {
            return false;
        };
        match find(tree, key) {
            Some(Node::Text(_)) => true,
            Some(Node::Map(forms)) => is_plural(forms),
            _ => false,
        }
    }

    /// The text of `key` in `locale`, which is matched to an available
    /// locale like an `Accept-Language` entry (unknown locales use the
    /// default). `args` fill `%{name}`; a `count` also selects the plural
    /// form. The text is returned as written: for an `_html` key that is
    /// markup with HTML-escaped arguments, for other keys plain text the
    /// caller must escape for its context. Never panics: problems are
    /// logged and visible in the result (see [`Options::show_missing`]).
    #[must_use]
    pub fn translate(&self, locale: &str, key: &str, args: &Args<'_>) -> String {
        self.translate_matched(self.match_locale(locale), key, args)
            .0
    }

    /// [`Translator::translate`] for an HTML context: `_html` keys give
    /// their markup with escaped arguments, any other key its text
    /// HTML-escaped. A missing `_html` key is never markup.
    #[must_use]
    pub fn translate_html(&self, locale: &str, key: &str, args: &Args<'_>) -> String {
        let (text, markup) = self.translate_matched(self.match_locale(locale), key, args);
        if markup { text } else { escape_html(&text) }
    }

    /// The text of `key` and whether it is markup (an `_html` key that was
    /// found). `locale` must be available.
    pub(crate) fn translate_matched(
        &self,
        locale: &str,
        key: &str,
        args: &Args<'_>,
    ) -> (String, bool) {
        let count = match args.plural_count() {
            Ok(count) => count,
            Err(problem) => {
                self.warn("i18n arguments", locale, key, Some(&problem));
                None
            }
        };
        if let Some(name) = args.names().find(|name| !is_argument_name(name)) {
            let problem = format!(
                "argument {:?} is not a name such as \"count\"",
                truncate(name)
            );
            self.warn("i18n arguments", locale, key, Some(&problem));
        }
        if !valid_key(key) {
            self.warn("invalid translation key", locale, key, None);
            return (self.missing(locale, key), false);
        }
        let mut found = self.lookup(locale, key, count);
        if found.is_none() {
            self.warn("translation missing", locale, key, None);
            if !self.show_missing {
                found = parents(&self.default_locale)
                    .find_map(|fallback| self.lookup(fallback, key, count));
            }
        }
        let Some((text, found_in)) = found else {
            return (self.missing(locale, key), false);
        };
        let markup = is_html_key(key);
        let (result, unknown) = interpolate(text, args, markup);
        if let Some(name) = unknown {
            let problem = format!("no value for %{{{name}}}");
            self.warn(
                "translation argument missing",
                found_in,
                key,
                Some(&problem),
            );
        }
        (result, markup)
    }

    /// Finds `key` in `locale` and its parents, choosing the plural form
    /// when a count is given. Returns the text and the locale it came from.
    fn lookup<'s>(
        &'s self,
        locale: &'s str,
        key: &str,
        count: Option<Count>,
    ) -> Option<(&'s str, &'s str)> {
        for candidate in parents(locale) {
            let Some(tree) = self.catalog.get(candidate) else {
                continue;
            };
            match find(tree, key) {
                Some(Node::Text(text)) => return Some((text, candidate)),
                Some(Node::Map(forms)) if is_plural(forms) => {
                    return Some((self.plural_form(candidate, forms, count), candidate));
                }
                _ => {}
            }
        }
        None
    }

    /// An explicit `zero` form is used for 0 in every language (as in
    /// Rails); a form the rule selects but the file lacks falls back to
    /// `other`, which every plural object has.
    fn plural_form<'s>(&self, locale: &str, forms: &'s Tree, count: Option<Count>) -> &'s str {
        let text = |category: Category| match forms.get(category.as_str()) {
            Some(Node::Text(text)) => Some(text.as_str()),
            _ => None,
        };
        let other = text(Category::Other).unwrap_or_default();
        let Some(count) = count else {
            return other;
        };
        let operands = count.operands();
        if operands.n == 0.0
            && operands.v == 0
            && let Some(zero) = text(Category::Zero)
        {
            return zero;
        }
        text(rule_in(&self.rules, locale)(operands)).unwrap_or(other)
    }

    /// The text shown for a translation that was not found.
    fn missing(&self, locale: &str, key: &str) -> String {
        if self.show_missing {
            format!("translation missing: {locale}.{}", truncate(key))
        } else {
            humanize(key)
        }
    }

    /// Logs a problem once per locale and key, for the first
    /// [`MAX_WARNINGS`] distinct problems. The first one past that bound
    /// logs a single "suppressed" line, and the rest are only counted
    /// ([`Translator::suppressed_warnings`]).
    fn warn(&self, message: &str, locale: &str, key: &str, problem: Option<&str>) {
        // Longer keys are invalid anyway; bound what a warning remembers.
        let mut end = key.len().min(MAX_KEY_LENGTH + 1);
        while !key.is_char_boundary(end) {
            end -= 1;
        }
        let id = format!("{message}\0{locale}\0{}", &key[..end]);
        let (log, announce) = {
            let mut warnings = self.warnings.lock().unwrap_or_else(PoisonError::into_inner);
            if warnings.seen.contains(&id) {
                (false, false)
            } else if warnings.seen.len() >= MAX_WARNINGS {
                warnings.suppressed += 1;
                (false, warnings.suppressed == 1)
            } else {
                warnings.seen.insert(id);
                (true, false)
            }
        };
        if announce {
            warn!(
                limit = MAX_WARNINGS,
                "missing-translation warnings suppressed"
            );
        }
        if !log {
            return;
        }
        let key = truncate(key);
        match problem {
            Some(problem) => warn!(locale, key, error = problem, "{message}"),
            None => warn!(locale, key, "{message}"),
        }
    }

    /// How many translation warnings were dropped because 1,024 distinct
    /// ones were already logged. It counts occurrences of new problems, not
    /// distinct ones (those are no longer remembered), so a non-zero value
    /// tells an operator the log is no longer complete.
    #[must_use]
    pub fn suppressed_warnings(&self) -> u64 {
        self.warnings
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .suppressed
    }
}

/// Whether `key` names trusted markup: its last segment is `html` or ends
/// in `_html`, as in Rails.
#[must_use]
pub fn is_html_key(key: &str) -> bool {
    let last = key.rsplit('.').next().unwrap_or(key);
    last == "html" || last.ends_with("_html")
}

/// Turns a key's last segment into text: `users.welcome_back` gives
/// "Welcome back"; a trailing `_html` is dropped.
#[must_use]
pub fn humanize(key: &str) -> String {
    let last = key.rsplit('.').next().unwrap_or(key);
    let last = last.strip_suffix("_html").unwrap_or(last);
    let spaced = last.replace(['_', '-'], " ");
    let trimmed = spaced.trim();
    let mut characters = trimmed.chars();
    match characters.next() {
        Some(first) => first.to_uppercase().chain(characters).collect(),
        None => String::new(),
    }
}

/// Escapes text for HTML element content and quoted attributes, like
/// Forge-Go's `template.HTMLEscapeString`.
#[must_use]
pub fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&#34;"),
            '\'' => out.push_str("&#39;"),
            '\0' => out.push('\u{FFFD}'),
            other => out.push(other),
        }
    }
    out
}

fn valid_key(key: &str) -> bool {
    !key.is_empty() && key.len() <= MAX_KEY_LENGTH && key.split('.').all(is_segment)
}

/// Replaces `%{name}` with the arguments and `%%` with `%`. In markup, text
/// values are HTML-escaped. Also returns the first placeholder without a
/// value, which stays as written.
fn interpolate<'t>(text: &'t str, args: &Args<'_>, markup: bool) -> (String, Option<&'t str>) {
    if !text.contains('%') {
        return (text.to_owned(), None);
    }
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut unknown = None;
    let mut copied = 0;
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'%' || index + 1 == bytes.len() {
            index += 1;
            continue;
        }
        match bytes[index + 1] {
            b'%' => {
                out.push_str(&text[copied..index]);
                out.push('%');
                index += 2;
                copied = index;
            }
            b'{' => {
                let rest = &text[index + 2..];
                let Some(end) = rest.find('}') else {
                    index += 1;
                    continue;
                };
                let name = &rest[..end];
                if !is_argument_name(name) {
                    index += 1;
                    continue;
                }
                out.push_str(&text[copied..index]);
                let placeholder_end = index + 3 + end;
                match args.get(name) {
                    Some(Value::Text(value)) if markup => out.push_str(&escape_html(value)),
                    Some(Value::Text(value) | Value::Markup(value)) => out.push_str(value),
                    None => {
                        unknown.get_or_insert(name);
                        out.push_str(&text[index..placeholder_end]);
                    }
                }
                index = placeholder_end;
                copied = index;
            }
            _ => index += 1,
        }
    }
    out.push_str(&text[copied..]);
    (out, unknown)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interpolation_is_not_recursive() {
        let args = Args::new().with("a", "%{b}").with("b", "x");
        assert_eq!(interpolate("%{a}|%{b}", &args, false).0, "%{b}|x");
        assert_eq!(interpolate("100%%", &args, false).0, "100%");
        assert_eq!(interpolate("%{", &args, false).0, "%{");
        assert_eq!(interpolate("%{bad name}%", &args, false).0, "%{bad name}%");
        assert_eq!(interpolate("é%{a}é", &args, false).0, "é%{b}é");
        let (text, unknown) = interpolate("%{zz} %{zz}", &args, false);
        assert_eq!((text.as_str(), unknown), ("%{zz} %{zz}", Some("zz")));
    }

    #[test]
    fn html_keys_and_humanized_keys() {
        for (key, want) in [
            ("a.b_html", true),
            ("html", true),
            ("a.html", true),
            ("a.htmlx", false),
            ("a_html.b", false),
            ("a.xhtml", false),
        ] {
            assert_eq!(is_html_key(key), want, "{key}");
        }
        for (key, want) in [
            ("users.welcome_back", "Welcome back"),
            ("a.intro_html", "Intro"),
            ("x", "X"),
            ("a.é_bom", "É bom"),
            ("", ""),
        ] {
            assert_eq!(humanize(key), want, "{key}");
        }
    }

    #[test]
    fn escaping_matches_forge_go() {
        assert_eq!(
            escape_html("<script>\"'&\0"),
            "&lt;script&gt;&#34;&#39;&amp;\u{FFFD}"
        );
    }
}
