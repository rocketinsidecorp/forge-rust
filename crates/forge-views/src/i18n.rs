//! The request side of translations (ADR 0037): [`LocaleNegotiation`]
//! picks each request's locale, [`I18n`] carries it with the translator to
//! handlers and templates (`t`, `l`, `number`, `locale`), and translated
//! responses get `Content-Language` and `Vary`.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use cookie::Cookie;
use forge_http::{
    BoxResponseFuture, HeaderValue, Middleware, Next, Request, Response, Uri, header,
};
use forge_i18n::{Args, DateTime, IntoCount, IntoNumber, Translator};
use thiserror::Error;
use tracing::warn;

/// The cookie that remembers a visitor's locale.
pub const LOCALE_COOKIE_NAME: &str = "locale";

/// How long the locale cookie lasts: a year.
const LOCALE_COOKIE_MAX_AGE_SECS: u32 = 365 * 24 * 60 * 60;

/// A locale option or choice that cannot be used.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum I18nError {
    /// The query parameter name is not an identifier.
    #[error("invalid locale parameter name {0:?}")]
    InvalidParam(String),
    /// The cookie name is not 1-64 ASCII letters, digits, `_` or `-`.
    #[error("invalid locale cookie name {0:?}")]
    InvalidCookie(String),
    /// The locale is not one of the translator's locales.
    #[error("the locale cookie needs an available locale")]
    UnavailableLocale,
}

/// A request's locale and the application's translator: what templates use
/// as `t`, `l`, `number` and `locale`. [`LocaleNegotiation`] attaches one
/// to every request ([`I18n::from_request`]); jobs, mailers and fragments
/// rendered without a request build one with [`I18n::new`]. Cloning is
/// cheap.
///
/// Using it (any lookup, or [`I18n::locale`]) marks the response as
/// translated: the layer then adds `Content-Language` and `Vary:
/// Accept-Language, Cookie`, so shared caches never serve one visitor's
/// language to another, while responses that never touched it (digested
/// assets, JSON) stay cacheable.
#[derive(Clone)]
pub struct I18n {
    translator: Arc<Translator>,
    locale: Arc<str>,
    translated: Arc<AtomicBool>,
}

impl fmt::Debug for I18n {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("I18n")
            .field("locale", &self.locale)
            .finish_non_exhaustive()
    }
}

impl I18n {
    /// Binds `locale`, matched to an available one (unknown locales get
    /// the default).
    #[must_use]
    pub fn new(translator: Arc<Translator>, locale: &str) -> Self {
        let locale = Arc::from(translator.match_locale(locale));
        Self {
            translator,
            locale,
            translated: Arc::new(AtomicBool::new(false)),
        }
    }

    /// The binding [`LocaleNegotiation`] attached to `request`, if the layer
    /// is installed.
    #[must_use]
    pub fn from_request(request: &Request) -> Option<Self> {
        request.extensions().get::<Self>().cloned()
    }

    fn mark(&self) {
        self.translated.store(true, Ordering::Relaxed);
    }

    /// Whether anything was translated or localized with this binding.
    #[must_use]
    pub fn is_translated(&self) -> bool {
        self.translated.load(Ordering::Relaxed)
    }

    /// The locale, for `<html lang="{{ layout.i18n.locale() }}">`.
    #[must_use]
    pub fn locale(&self) -> &str {
        self.mark();
        &self.locale
    }

    /// The translator.
    #[must_use]
    pub fn translator(&self) -> &Arc<Translator> {
        &self.translator
    }

    /// The translation of `key`: add arguments with [`Translation::with`]
    /// and [`Translation::count`]. In a template it renders as HTML (see
    /// [`Translation`]); in Rust code [`Translation::text`] gives the text
    /// as written, for a flash message or an email subject.
    ///
    /// ```text
    /// <h1>{{ layout.i18n.t("users.welcome").with("name", user.name) }}</h1>
    /// <p>{{ layout.i18n.t("users.inbox").count(unread) }}</p>
    /// ```
    #[must_use]
    pub fn t<'a>(&'a self, key: &'a str) -> Translation<'a> {
        Translation {
            i18n: self,
            key,
            args: Args::new(),
        }
    }

    /// Formats a date or time with the locale's `time.formats.FORMAT`, or
    /// the full key a format with a dot names (`"date.formats.long"`).
    #[must_use]
    pub fn l(&self, value: &DateTime, format: &str) -> String {
        self.mark();
        self.translator.format_time(&self.locale, value, format)
    }

    /// Formats a number with the locale's separator and delimiter: integers
    /// without decimals, floats with `number.format.precision`.
    #[must_use]
    pub fn number(&self, value: impl IntoNumber) -> String {
        self.mark();
        self.translator.format_number(&self.locale, value, None)
    }

    /// Formats a number with `precision` decimals.
    #[must_use]
    pub fn number_with_precision(&self, value: impl IntoNumber, precision: u32) -> String {
        self.mark();
        self.translator
            .format_number(&self.locale, value, Some(precision))
    }
}

/// One translation being built: a key and its arguments.
///
/// Its `Display` (what `{{ ... }}` renders) is HTML: an `_html` key's
/// trusted markup with every argument escaped, any other key's text
/// HTML-escaped. Askama does not escape it again (it is `HtmlSafe`), and
/// `|safe` cannot make a plain key raw HTML. [`Translation::text`] is the
/// text as written, for contexts that are not HTML.
pub struct Translation<'a> {
    i18n: &'a I18n,
    key: &'a str,
    args: Args<'a>,
}

impl fmt::Debug for Translation<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Translation")
            .field("key", &self.key)
            .field("args", &self.args)
            .finish_non_exhaustive()
    }
}

impl<'a> Translation<'a> {
    /// Fills `%{name}` with `value`'s text.
    #[must_use]
    pub fn with(mut self, name: &'a str, value: impl fmt::Display) -> Self {
        self.args = self.args.with(name, value);
        self
    }

    /// Fills `%{name}` with trusted markup, not escaped in an `_html`
    /// translation. Only pass markup your own code built.
    #[must_use]
    pub fn with_markup(mut self, name: &'a str, markup: impl fmt::Display) -> Self {
        self.args = self.args.with_markup(name, markup.to_string());
        self
    }

    /// Sets `count`: it selects the plural form and fills `%{count}`.
    #[must_use]
    pub fn count(mut self, count: impl IntoCount) -> Self {
        self.args = self.args.count(count);
        self
    }

    /// The text as written: for a plain key, text the caller escapes for
    /// its context (a flash message is escaped when the layout renders it).
    #[must_use]
    pub fn text(&self) -> String {
        self.i18n.mark();
        self.i18n
            .translator
            .translate(&self.i18n.locale, self.key, &self.args)
    }

    /// The translation as HTML (what `Display` writes).
    #[must_use]
    pub fn html(&self) -> String {
        self.i18n.mark();
        self.i18n
            .translator
            .translate_html(&self.i18n.locale, self.key, &self.args)
    }
}

impl fmt::Display for Translation<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.html())
    }
}

/// Already HTML: plain keys are escaped by [`Translation::html`].
impl askama::filters::HtmlSafe for Translation<'_> {}

/// The request layer that picks each request's locale and attaches an
/// [`I18n`] for handlers and templates, from the first of:
///
/// 1. the first path segment, with [`LocaleNegotiation::with_path_prefix`]
///    (`/pt-BR/posts` is routed as `/posts`; the segment must name an
///    available locale);
/// 2. a query parameter, with [`LocaleNegotiation::with_param`]
///    (`?locale=pt-BR`);
/// 3. the `locale` cookie ([`locale_cookie`]);
/// 4. `Accept-Language`, by quality;
/// 5. the translator's default locale.
///
/// Names that are not available locales are ignored, so no request input
/// ever names a file. After the handler, a response that used the
/// binding gets `Content-Language` and `Vary: Accept-Language, Cookie`.
/// The path prefix is removed in the router's rewrite step
/// ([`Middleware::rewrite`]), before any layer handles the request, so
/// every layer sees `/admin` for `/pt-BR/admin` wherever it is registered:
/// a path guard (an `/admin` guard, an allow-list) cannot be bypassed with
/// a locale prefix, whatever the layer order.
///
/// [`LocaleNegotiation::negotiate`] only answers the question and cannot
/// mark a response. A response that depends on its result (a redirect to
/// the visitor's language, say) must add `Vary: Accept-Language, Cookie`
/// itself with [`set_content_language`].
#[derive(Clone)]
pub struct LocaleNegotiation {
    translator: Arc<Translator>,
    path_prefix: bool,
    param: Option<String>,
    cookie: String,
}

impl fmt::Debug for LocaleNegotiation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LocaleNegotiation")
            .field("path_prefix", &self.path_prefix)
            .field("param", &self.param)
            .field("cookie", &self.cookie)
            .finish_non_exhaustive()
    }
}

impl LocaleNegotiation {
    /// Negotiates from the `locale` cookie, `Accept-Language` and the
    /// default locale.
    #[must_use]
    pub fn new(translator: Arc<Translator>) -> Self {
        Self {
            translator,
            path_prefix: false,
            param: None,
            cookie: LOCALE_COOKIE_NAME.to_owned(),
        }
    }

    /// Lets the first path segment choose the locale and removes it before
    /// any layer runs, so every layer and the route table see the path
    /// without it (the server's request telemetry logs the path as sent).
    /// [`path_prefix`] gives the removed prefix.
    #[must_use]
    pub fn with_path_prefix(mut self) -> Self {
        self.path_prefix = true;
        self
    }

    /// Lets the query parameter `name` (such as `locale`) choose the
    /// locale.
    pub fn with_param(mut self, name: &str) -> Result<Self, I18nError> {
        if !is_parameter_name(name) {
            return Err(I18nError::InvalidParam(bounded(name)));
        }
        self.param = Some(name.to_owned());
        Ok(self)
    }

    /// Reads the visitor's choice from the cookie `name` instead of
    /// `locale`.
    pub fn with_cookie(mut self, name: &str) -> Result<Self, I18nError> {
        if !is_cookie_name(name) {
            return Err(I18nError::InvalidCookie(bounded(name)));
        }
        self.cookie = name.to_owned();
        Ok(self)
    }

    /// The translator.
    #[must_use]
    pub fn translator(&self) -> &Arc<Translator> {
        &self.translator
    }

    /// The locale of `request` (see the order above). It does not mark any
    /// response: when what you send depends on the answer, call
    /// [`set_content_language`] (or add `Vary: Accept-Language, Cookie`)
    /// yourself.
    #[must_use]
    pub fn negotiate(&self, request: &Request) -> String {
        self.choose(request).to_owned()
    }

    /// The available locale the first path segment of `request` names, and
    /// the length of `/` and that segment.
    fn prefix_locale<'s>(&'s self, request: &Request) -> Option<(&'s str, usize)> {
        let path = request.uri().path();
        let rest = path.strip_prefix('/').unwrap_or(path);
        let segment = rest.split('/').next().unwrap_or_default();
        self.translator
            .exact_locale(segment)
            .map(|locale| (locale, 1 + segment.len()))
    }

    /// The locale of `request`.
    fn choose(&self, request: &Request) -> &str {
        let translator = &self.translator;
        if self.path_prefix {
            match request.extensions().get::<PathPrefix>() {
                // Removed by the rewrite step.
                Some(PathPrefix(prefix)) => {
                    let name = prefix.strip_prefix('/').unwrap_or(prefix);
                    if let Some(locale) = translator.exact_locale(name) {
                        return locale;
                    }
                }
                // A request that did not pass the rewrite step (`negotiate`
                // on a request the router has not seen): read the path.
                None if request.extensions().get::<PrefixChecked>().is_none() => {
                    if let Some((locale, _)) = self.prefix_locale(request) {
                        return locale;
                    }
                }
                None => {}
            }
        }
        if let Some(param) = &self.param
            && let Some(value) = query_value(request.uri().query(), param)
            && let Some(locale) = translator.find_locale(&value)
        {
            return locale;
        }
        if let Some(value) = cookie_value(request, &self.cookie)
            && let Some(locale) = translator.find_locale(&value)
        {
            return locale;
        }
        let accept = request
            .headers()
            .get(header::ACCEPT_LANGUAGE)
            .and_then(|value| value.to_str().ok());
        if let Some(header) = accept
            && let Some(locale) = translator.match_accept_language(header)
        {
            return locale;
        }
        translator.default_locale()
    }
}

impl Middleware for LocaleNegotiation {
    /// Removes the locale path prefix, once per request, before any layer
    /// runs (the router calls it; `handle` calls it too for a request that
    /// did not come through a router).
    fn rewrite(&self, request: &mut Request) {
        if !self.path_prefix || request.extensions().get::<PrefixChecked>().is_some() {
            return;
        }
        // Checked once: a second negotiation layer, or `handle` after this
        // step, must not strip `/en` from `/pt-BR/en/...` again.
        request.extensions_mut().insert(PrefixChecked);
        let Some((locale, length)) = self.prefix_locale(request) else {
            return;
        };
        // The locale's canonical name, not the request's spelling of it
        // ("/PT-br" is stored as "/pt-BR").
        let prefix = PathPrefix(format!("/{locale}"));
        // Only a prefix that was really removed is reported: were the
        // rewrite refused, the request would still carry it and a return
        // path built from `path_prefix` would read `/pt-BR/pt-BR/...`.
        if strip_prefix(request, length) {
            request.extensions_mut().insert(prefix);
        }
    }

    fn handle<'a>(&'a self, mut request: Request, next: Next<'a>) -> BoxResponseFuture<'a> {
        self.rewrite(&mut request);
        let locale = self.choose(&request);
        let i18n = I18n::new(Arc::clone(&self.translator), locale);
        request.extensions_mut().insert(i18n.clone());
        Box::pin(async move {
            let mut response = next.run(request).await;
            if i18n.is_translated() {
                set_content_language(&mut response, &i18n.locale);
            }
            response
        })
    }
}

/// The locale path segment [`LocaleNegotiation`] removed from the request.
#[derive(Clone, Debug)]
struct PathPrefix(String);

/// Set once a negotiation layer looked for a locale path prefix, so it is
/// removed at most once per request.
#[derive(Clone, Copy, Debug)]
struct PrefixChecked;

/// The locale path segment [`LocaleNegotiation::with_path_prefix`] removed
/// from `request` (`"/pt-BR"`), or `None` when the request had none (or did
/// not pass through the layer with the prefix on). Code that builds a URL
/// from the stripped request path, such as the sign-in return path, puts it
/// back in front so the visitor returns to the page in the language they
/// used. The value is an available locale's canonical name (letters, digits
/// and hyphens) behind a slash, never request text, even for `/PT-br`; still
/// validate the finished URL (see [`return_path`](crate::return_path))
/// before redirecting to it.
#[must_use]
pub fn path_prefix(request: &Request) -> Option<&str> {
    request
        .extensions()
        .get::<PathPrefix>()
        .map(|prefix| prefix.0.as_str())
}

/// Removes the first `length` bytes (`/` and a locale segment) of the
/// path. The URI holds only the escaped path, which is what the segment
/// was read from and what routing matches, so no decoded form can
/// disagree with it: `/pt-BR%2F..%2Fadmin` has the segment
/// `pt-BR%2F..%2Fadmin`, which is not a locale, and is left alone. Returns
/// whether the request was rewritten.
fn strip_prefix(request: &mut Request, length: usize) -> bool {
    let uri = request.uri();
    let rest = uri.path().get(length..).unwrap_or_default();
    let path = if rest.is_empty() { "/" } else { rest };
    let target = match uri.query() {
        Some(query) => format!("{path}?{query}"),
        None => path.to_owned(),
    };
    let mut parts = uri.clone().into_parts();
    let rebuilt = target.parse().ok().and_then(|path_and_query| {
        parts.path_and_query = Some(path_and_query);
        Uri::from_parts(parts).ok()
    });
    if let Some(stripped) = rebuilt {
        request.set_uri(stripped);
        true
    } else {
        warn!("locale prefix not removed: the remaining path is not a valid URI");
        false
    }
}

/// Marks `response` as translated for `locale`: `Content-Language` names
/// it, and `Vary: Accept-Language, Cookie` keeps shared caches from serving
/// one visitor's language to another. [`LocaleNegotiation`] calls it for
/// every response that used the request's [`I18n`]; call it yourself for a
/// translated response built another way.
pub fn set_content_language(response: &mut Response, locale: &str) {
    let headers = response.headers_mut();
    match HeaderValue::from_str(locale) {
        Ok(value) => {
            headers.insert(header::CONTENT_LANGUAGE, value);
        }
        Err(_) => return,
    }
    for name in ["Accept-Language", "Cookie"] {
        let varies = headers
            .get_all(header::VARY)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|value| value.split(','))
            .map(str::trim)
            .any(|field| field == "*" || field.eq_ignore_ascii_case(name));
        if !varies {
            headers.append(header::VARY, HeaderValue::from_static(name));
        }
    }
}

/// The `Set-Cookie` value that remembers a visitor's locale choice for a
/// year, for a language switcher: `Secure`, `HttpOnly`, `SameSite=Lax`,
/// `Path=/`. `name` is the cookie (`None` for `locale`); `locale` must name
/// an available locale exactly (ignoring case).
pub fn locale_cookie(
    translator: &Translator,
    name: Option<&str>,
    locale: &str,
) -> Result<String, I18nError> {
    let name = name.unwrap_or(LOCALE_COOKIE_NAME);
    if !is_cookie_name(name) {
        return Err(I18nError::InvalidCookie(bounded(name)));
    }
    let locale = translator
        .exact_locale(locale)
        .ok_or(I18nError::UnavailableLocale)?;
    Ok(format!(
        "{name}={locale}; Path=/; Max-Age={LOCALE_COOKIE_MAX_AGE_SECS}; Secure; HttpOnly; \
         SameSite=Lax"
    ))
}

/// The first value of cookie `name`.
fn cookie_value(request: &Request, name: &str) -> Option<String> {
    request
        .headers()
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| Cookie::split_parse(value).flatten())
        .find(|cookie| cookie.name() == name)
        .map(|cookie| cookie.value().to_owned())
}

/// The first value of query parameter `name`, percent-decoded (`+` is a
/// space). `None` when absent or not decodable.
fn query_value(query: Option<&str>, name: &str) -> Option<String> {
    let value = query?
        .split('&')
        .filter_map(|pair| pair.split_once('=').or(Some((pair, ""))))
        .find_map(|(key, value)| (key == name).then_some(value))?;
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' => {
                let hex = value.get(index + 1..index + 3)?;
                decoded.push(u8::from_str_radix(hex, 16).ok()?);
                index += 3;
            }
            b'+' => {
                decoded.push(b' ');
                index += 1;
            }
            byte => {
                decoded.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8(decoded).ok()
}

/// ASCII letters, digits and `_`, not starting with a digit, at most 64
/// bytes.
fn is_parameter_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.bytes().enumerate().all(|(index, byte)| {
            byte == b'_' || byte.is_ascii_alphabetic() || (index > 0 && byte.is_ascii_digit())
        })
}

/// 1-64 ASCII letters, digits, `_` and `-`.
fn is_cookie_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

fn bounded(text: &str) -> String {
    text.chars().take(32).collect()
}

#[cfg(test)]
mod tests {
    use askama::Template;
    use forge_http::{Method, Router, StatusCode};

    use super::*;
    use crate::test_support::{header as header_of, request_with, text};

    const EN: &str = r#"{"en": {
        "greeting": "Hello, %{name}!",
        "users": {"intro_html": "<strong>%{name}</strong> joined"},
        "inbox": {"zero": "No messages", "one": "%{count} message", "other": "%{count} messages"}
    }}"#;
    const PT: &str = r#"{"pt": {"greeting": "Olá, %{name}!", "number": {"format": {"separator": ",", "delimiter": "."}}}}"#;
    const PT_BR: &str = r#"{"pt-BR": {"greeting": "Oi, %{name}!"}}"#;

    fn translator() -> Arc<Translator> {
        let files = [("en.json", EN), ("pt.json", PT), ("pt-BR.json", PT_BR)];
        Arc::new(
            Translator::from_files(&files, forge_i18n::Options::new().show_missing(true))
                .expect("valid catalog"),
        )
    }

    fn request(uri: &str, cookie: &str, accept: &str) -> Request {
        let mut headers = Vec::new();
        if !cookie.is_empty() {
            headers.push(("cookie", cookie));
        }
        if !accept.is_empty() {
            headers.push(("accept-language", accept));
        }
        request_with(Method::GET, uri, &headers)
    }

    #[test]
    fn negotiation_follows_the_forge_go_order() {
        let negotiation = LocaleNegotiation::new(translator())
            .with_path_prefix()
            .with_param("locale")
            .expect("valid parameter");
        for (name, target, cookie, accept, want) in [
            ("default", "/", "", "", "en"),
            ("accept exact", "/", "", "pt-BR,en;q=0.5", "pt-BR"),
            ("accept truncation", "/", "", "pt-PT", "pt"),
            ("accept language variant", "/", "", "en-GB", "en"),
            (
                "accept order by quality",
                "/",
                "",
                "en;q=0.4, pt;q=0.9",
                "pt",
            ),
            (
                "accept skips unknown",
                "/",
                "",
                "fr, de;q=0.9, pt-BR;q=0.1",
                "pt-BR",
            ),
            ("accept wildcard", "/", "", "fr, *;q=0.5", "en"),
            ("accept q=0 excludes", "/", "", "pt;q=0, fr", "en"),
            ("accept malformed", "/", "", ";;;,,,=", "en"),
            ("cookie beats header", "/", "locale=pt", "en", "pt"),
            (
                "invalid cookie ignored",
                "/",
                "locale=../../etc",
                "pt-BR",
                "pt-BR",
            ),
            (
                "param beats cookie",
                "/?locale=pt-br",
                "locale=en",
                "en",
                "pt-BR",
            ),
            ("unknown param ignored", "/?locale=fr", "", "pt", "pt"),
            (
                "traversal param ignored",
                "/?locale=..%2F..%2Fetc",
                "",
                "",
                "en",
            ),
            (
                "path prefix beats param",
                "/pt-BR/posts?locale=en",
                "",
                "",
                "pt-BR",
            ),
            ("path prefix must be available", "/fr/posts", "", "pt", "pt"),
        ] {
            assert_eq!(
                negotiation.negotiate(&request(target, cookie, accept)),
                want,
                "{name}"
            );
        }
        // Without the opt-ins, the prefix and the parameter are ignored.
        let plain = LocaleNegotiation::new(translator());
        assert_eq!(
            plain.negotiate(&request("/pt/posts?locale=pt", "", "")),
            "en"
        );
    }

    fn router(negotiation: LocaleNegotiation) -> Router {
        let mut router = Router::new();
        for path in ["/", "/posts", "/a%2Fb", "/plain"] {
            router
                .route(Method::GET, path, |request: Request| async move {
                    let path = request.uri().path().to_owned();
                    if path == "/plain" {
                        return Response::new(StatusCode::OK, "no translation");
                    }
                    let i18n = I18n::from_request(&request).expect("the layer binds a locale");
                    let greeting = i18n.t("greeting").with("name", "Ana").text();
                    Response::new(
                        StatusCode::OK,
                        format!("{path}|{}|{greeting}", i18n.locale()),
                    )
                })
                .expect("route registers");
        }
        router.layer(negotiation);
        router
    }

    #[tokio::test]
    async fn the_layer_binds_the_locale_and_strips_the_prefix() {
        let router = router(LocaleNegotiation::new(translator()).with_path_prefix());
        for (target, status, want) in [
            ("/pt-BR/posts", StatusCode::OK, "/posts|pt-BR|Oi, Ana!"),
            ("/pt-BR", StatusCode::OK, "/|pt-BR|Oi, Ana!"),
            ("/pt/", StatusCode::OK, "/|pt|Olá, Ana!"),
            ("/PT-br/posts?x=1", StatusCode::OK, "/posts|pt-BR|Oi, Ana!"),
            ("/posts", StatusCode::OK, "/posts|en|Hello, Ana!"),
            // The stripped path still goes through the router's rules: an
            // encoded slash is refused (Forge-Go's invalid_path).
            ("/pt/a%2Fb", StatusCode::BAD_REQUEST, "bad request"),
            // Not a locale segment: nothing is stripped, the escaped path
            // is routed as is (and refused for its encoded slashes).
            ("/pt-BRx/posts", StatusCode::NOT_FOUND, "not found"),
            (
                "/pt-BR%2F..%2Fadmin",
                StatusCode::BAD_REQUEST,
                "bad request",
            ),
        ] {
            let response = router.dispatch(request(target, "", "")).await;
            assert_eq!(response.status(), status, "{target}");
            assert_eq!(text(&response), want, "{target}");
        }
    }

    /// Answers every request with the prefix the negotiation layer kept.
    struct ReportPrefix;

    impl Middleware for ReportPrefix {
        fn handle<'a>(&'a self, request: Request, _: Next<'a>) -> BoxResponseFuture<'a> {
            let prefix = path_prefix(&request).unwrap_or("none").to_owned();
            Box::pin(async move { Response::new(StatusCode::OK, prefix) })
        }
    }

    #[tokio::test]
    async fn the_layer_keeps_the_canonical_prefix_it_removed() {
        let serve = |negotiation: LocaleNegotiation, target: &'static str| async move {
            let mut router = Router::new();
            router.layer(negotiation);
            router.layer(ReportPrefix);
            text(&router.dispatch(request(target, "", "")).await)
        };
        let on = || LocaleNegotiation::new(translator()).with_path_prefix();
        for (target, want) in [
            ("/pt-BR/posts", "/pt-BR"),
            ("/pt/posts", "/pt"),
            // The locale's name, not the request's spelling of it.
            ("/PT-br/posts", "/pt-BR"),
            ("/posts", "none"),
            ("/fr/posts", "none"),
            ("/pt-BRx/posts", "none"),
            ("//evil/posts", "none"),
        ] {
            assert_eq!(serve(on(), target).await, want, "prefix on, {target}");
        }
        assert_eq!(
            serve(LocaleNegotiation::new(translator()), "/pt-BR/posts").await,
            "none",
            "prefix off"
        );
        assert_eq!(path_prefix(&request("/pt-BR/posts", "", "")), None);
    }

    /// A path guard: refuses `/admin` and everything under it.
    struct DenyAdmin;

    impl Middleware for DenyAdmin {
        fn handle<'a>(&'a self, request: Request, next: Next<'a>) -> BoxResponseFuture<'a> {
            let path = request.uri().path();
            if path == "/admin" || path.starts_with("/admin/") {
                return Box::pin(async { Response::new(StatusCode::FORBIDDEN, "denied") });
            }
            next.run(request)
        }
    }

    #[tokio::test]
    async fn a_locale_prefix_cannot_bypass_a_path_guard_registered_before_the_negotiation() {
        for guard_first in [true, false] {
            let mut router = Router::new();
            for path in ["/admin", "/admin/users", "/posts"] {
                router
                    .route(Method::GET, path, |request: Request| async move {
                        let locale = I18n::from_request(&request)
                            .map(|i18n| i18n.locale().to_owned())
                            .unwrap_or_default();
                        Response::new(StatusCode::OK, format!("{}|{locale}", request.uri().path()))
                    })
                    .expect("route registers");
            }
            let negotiation = LocaleNegotiation::new(translator()).with_path_prefix();
            if guard_first {
                router.layer(DenyAdmin).layer(negotiation);
            } else {
                router.layer(negotiation).layer(DenyAdmin);
            }
            for (target, status, want) in [
                ("/admin", StatusCode::FORBIDDEN, "denied"),
                ("/pt-BR/admin", StatusCode::FORBIDDEN, "denied"),
                ("/PT-br/admin/users", StatusCode::FORBIDDEN, "denied"),
                ("/pt/admin?x=1", StatusCode::FORBIDDEN, "denied"),
                ("/en/admin/", StatusCode::FORBIDDEN, "denied"),
                // The prefix is removed once: `/en` after it is a path.
                ("/pt-BR/en/admin", StatusCode::NOT_FOUND, "not found"),
                ("/pt-BR/posts", StatusCode::OK, "/posts|pt-BR"),
                ("/posts", StatusCode::OK, "/posts|en"),
            ] {
                let response = router.dispatch(request(target, "", "")).await;
                assert_eq!(
                    response.status(),
                    status,
                    "guard first: {guard_first}, {target}"
                );
                assert_eq!(
                    text(&response),
                    want,
                    "guard first: {guard_first}, {target}"
                );
            }
        }
    }

    #[tokio::test]
    async fn negotiate_in_a_handler_still_sees_the_removed_prefix() {
        let negotiation = LocaleNegotiation::new(translator()).with_path_prefix();
        let mut router = Router::new();
        router
            .route(Method::GET, "/posts", {
                let negotiation = negotiation.clone();
                move |request: Request| {
                    let locale = negotiation.negotiate(&request);
                    async move { Response::new(StatusCode::OK, locale) }
                }
            })
            .expect("route registers");
        router.layer(negotiation);
        let response = router.dispatch(request("/pt-BR/posts", "", "en")).await;
        assert_eq!(text(&response), "pt-BR");
    }

    #[test]
    fn strip_prefix_reports_whether_it_rewrote_the_request() {
        let mut posts = request("/pt-BR/posts?page=2", "", "");
        assert!(strip_prefix(&mut posts, "/pt-BR".len()));
        assert_eq!(posts.uri().to_string(), "/posts?page=2");
        let mut root = request("/pt-BR", "", "");
        assert!(strip_prefix(&mut root, "/pt-BR".len()));
        assert_eq!(root.uri().path(), "/");
    }

    #[tokio::test]
    async fn negotiate_alone_marks_no_response() {
        // `negotiate` answers a question; it has no response to mark, so a
        // handler whose output depends on it sets the headers itself.
        let negotiation = LocaleNegotiation::new(translator());
        let mut router = Router::new();
        router
            .route(Method::GET, "/where", {
                let negotiation = negotiation.clone();
                move |request: Request| {
                    let negotiation = negotiation.clone();
                    async move {
                        let locale = negotiation.negotiate(&request);
                        let mut response = forge_http::Response::empty(StatusCode::SEE_OTHER);
                        if let Ok(value) = HeaderValue::from_str(&format!("/{locale}/")) {
                            response.headers_mut().insert(header::LOCATION, value);
                        }
                        response
                    }
                }
            })
            .expect("route registers");
        router.layer(negotiation);
        let unmarked = router.dispatch(request("/where", "", "pt-BR")).await;
        assert_eq!(header_of(&unmarked, "location"), Some("/pt-BR/"));
        assert!(unmarked.headers().get(header::VARY).is_none());
        assert_eq!(header_of(&unmarked, "content-language"), None);
    }

    #[tokio::test]
    async fn only_translated_responses_carry_content_language_and_vary() {
        let router = router(LocaleNegotiation::new(translator()));
        let translated = router.dispatch(request("/posts", "locale=pt-BR", "")).await;
        assert_eq!(header_of(&translated, "content-language"), Some("pt-BR"));
        let vary: Vec<_> = translated
            .headers()
            .get_all(header::VARY)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .collect();
        assert_eq!(vary, ["Accept-Language", "Cookie"]);

        let plain = router.dispatch(request("/plain", "locale=pt-BR", "")).await;
        assert_eq!(header_of(&plain, "content-language"), None);
        assert!(plain.headers().get(header::VARY).is_none());
    }

    #[test]
    fn vary_is_merged_not_duplicated() {
        let mut response = Response::new(StatusCode::OK, "x");
        response
            .headers_mut()
            .append(header::VARY, HeaderValue::from_static("accept-language"));
        set_content_language(&mut response, "pt-BR");
        set_content_language(&mut response, "pt-BR");
        let vary: Vec<_> = response
            .headers()
            .get_all(header::VARY)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .collect();
        assert_eq!(vary, ["accept-language", "Cookie"]);
        assert_eq!(header_of(&response, "content-language"), Some("pt-BR"));
    }

    #[test]
    fn the_locale_cookie_is_strict_and_only_names_available_locales() {
        let translator = translator();
        assert_eq!(
            locale_cookie(&translator, None, "pt-br").as_deref(),
            Ok("locale=pt-BR; Path=/; Max-Age=31536000; Secure; HttpOnly; SameSite=Lax")
        );
        for locale in ["fr", "../en", "", "pt-PT"] {
            assert_eq!(
                locale_cookie(&translator, None, locale),
                Err(I18nError::UnavailableLocale),
                "{locale}"
            );
        }
        assert!(locale_cookie(&translator, Some("bad name"), "en").is_err());
        assert!(
            LocaleNegotiation::new(Arc::clone(&translator))
                .with_param("a b")
                .is_err()
        );
        assert!(
            LocaleNegotiation::new(Arc::clone(&translator))
                .with_cookie("a;b")
                .is_err()
        );
        let renamed = LocaleNegotiation::new(translator)
            .with_cookie("lang")
            .expect("valid cookie name");
        assert_eq!(renamed.negotiate(&request("/", "lang=pt", "")), "pt");
    }

    #[derive(Template)]
    #[template(
        source = r#"<p title="{{ i18n.t("greeting").with("name", name) }}">{{ i18n.t("greeting").with("name", name) }}</p><div>{{ i18n.t("users.intro_html").with("name", name) }}</div><i>{{ i18n.t("greeting").with("name", name)|safe }}</i><b>{{ i18n.t("inbox").count(count) }}</b><s>{{ i18n.number(big) }}</s><u lang="{{ i18n.locale() }}">{{ i18n.t("users.<img>_html") }}</u>"#,
        ext = "html"
    )]
    struct Page<'a> {
        i18n: I18n,
        name: &'a str,
        count: i64,
        big: f64,
    }

    #[test]
    fn templates_escape_every_argument_and_plain_keys_are_never_markup() {
        let attack = r#"<script>alert(1)</script>"'&"#;
        let page = Page {
            i18n: I18n::new(translator(), "en"),
            name: attack,
            count: 0,
            big: 1234.5,
        }
        .render()
        .expect("renders");
        assert!(!page.contains("<script>"), "{page}");
        assert!(
            page.contains("<div><strong>&lt;script&gt;alert(1)&lt;/script&gt;&#34;&#39;&amp;</strong> joined</div>"),
            "{page}"
        );
        // `|safe` on a plain key still yields escaped text.
        assert!(page.contains("<i>Hello, &lt;script&gt;"), "{page}");
        // No double escaping in attributes or element content.
        assert!(
            page.contains(r#"<p title="Hello, &lt;script&gt;"#),
            "{page}"
        );
        assert!(page.contains("<b>No messages</b>"), "{page}");
        assert!(page.contains("<s>1,234.50</s>"), "{page}");
        assert!(
            page.contains(r#"<u lang="en">translation missing: en.users.&lt;img&gt;_html</u>"#),
            "{page}"
        );
        let portuguese = I18n::new(translator(), "pt-BR");
        assert_eq!(portuguese.number_with_precision(1234.5, 1), "1.234,5");
        assert_eq!(
            portuguese.l(&DateTime::from_unix(0), "date.formats.default"),
            "1970-01-01"
        );
    }

    #[test]
    fn query_values_are_decoded_strictly() {
        assert_eq!(
            query_value(Some("a=1&locale=pt%2Dbr"), "locale").as_deref(),
            Some("pt-br")
        );
        assert_eq!(query_value(Some("locale"), "locale").as_deref(), Some(""));
        assert_eq!(query_value(Some("locale=%zz"), "locale"), None);
        assert_eq!(query_value(Some("locale=%e9"), "locale"), None);
        assert_eq!(query_value(None, "locale"), None);
    }
}
