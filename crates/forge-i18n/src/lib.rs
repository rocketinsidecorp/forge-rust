//! Internationalization for Forge applications, like Rails I18n and the
//! port of Forge-Go's `i18n` package (ADR 0037, `docs/i18n.md`).
//!
//! Translations live in JSON files under `config/locales` (`en.json`,
//! `posts.pt-BR.json`), one object whose only key is the locale. Keys are
//! dotted paths (`users.welcome`), values interpolate named arguments
//! (`"Welcome, %{name}!"`) and choose CLDR plural forms by `count`. The file
//! format, key rules, plural rules, locale matching and missing-translation
//! behavior are language-neutral: Forge-Go reads the same files with the
//! same results.
//!
//! - [`Translator`]: loads the files ([`Translator::from_files`] for the
//!   table an application embeds at build time, [`Translator::load_dir`]),
//!   translates ([`Translator::translate`], [`Translator::translate_html`]),
//!   matches locales ([`Translator::match_locale`],
//!   [`Translator::match_accept_language`]) and formats numbers and dates
//!   ([`Translator::format_number`], [`Translator::format_time`]).
//! - [`Args`]: `%{name}` values and the plural `count`.
//! - [`canonical_locale`], [`file_locale`], [`parse_accept_language`]: the
//!   validated BCP 47 subset, so request input never names a file.
//! - [`Category`], [`Operands`], [`plural_rule_for`]: CLDR 46 cardinal rules.
//!
//! The request side (locale negotiation, the `locale` cookie,
//! `Content-Language`, the template helpers) is in the HTTP adapter ring,
//! `forge_views`. This crate has no HTTP or template dependency, so jobs and
//! mailers translate with it directly.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

mod args;
mod catalog;
mod format;
mod locale;
mod negotiate;
mod plural;
mod translator;

use std::borrow::Cow;

pub use args::{Args, Count, IntoCount, Value};
pub use catalog::{
    MAX_DEPTH, MAX_FILE_BYTES, MAX_FILES, MAX_KEY_LENGTH, MAX_KEYS_PER_LOCALE, file_locale,
};
pub use format::{DateTime, IntoNumber, Number};
pub use locale::canonical_locale;
pub use negotiate::{
    MAX_ACCEPT_LANGUAGE_BYTES, MAX_ACCEPT_LANGUAGE_ENTRIES, parse_accept_language,
};
pub use plural::{Category, Operands, PluralRule, plural_rule_for};
pub use translator::{
    DEFAULT_LOCALE, MAX_WARNINGS, Options, Translator, escape_html, humanize, is_html_key,
};

/// A translation file, locale name or option that cannot be used. The
/// message names the file and the fix; loading stops the application at
/// startup instead of serving partial translations.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error("i18n: {message}")]
pub struct LoadError {
    message: Cow<'static, str>,
}

impl LoadError {
    pub(crate) fn new(message: impl Into<Cow<'static, str>>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// The message without the `i18n: ` prefix.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}
