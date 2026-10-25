# ADR 0037: Internationalization (i18n)

- **Status:** Accepted
- **Date:** 2026-10-20
- **Port of:** Forge-Go ADR 0042 and its review follow-ups (forge-go#21);
  tracked in [forge-rust#22](https://github.com/rocketinsidecorp/forge-rust/issues/22)

The decision is language-neutral: Forge-Rust reads the same translation
files as Forge-Go, with the same keys, plural rules, locale negotiation,
escaping and missing-translation behavior, and ships the same commands. The
guide is [docs/i18n.md](../i18n.md).

## Context

Every user-visible string the generators wrote (the layout, the welcome
page, the sign-in and password-reset pages, flash messages, scaffold pages)
was English in source code; the [Rails parity](../rails-parity.md) matrix
listed I18n as planned. Forge-Go shipped Rails-style i18n first (its ADR
0042) and asked for a port with the same file formats, environment variable,
command names, behavior and security properties.

Constraints on this side: no new package in `Cargo.lock` (`serde_json` is
already admitted, and serde's default keeps the last of two duplicate keys,
which the format forbids); views are compile-time checked Askama templates
in the HTTP adapter ring (ADR 0022); `forge-http` owns its request type and
middleware trait, without tower.

## Decision

1. **Files, keys and locales: as Forge-Go.** JSON files in
   `config/locales`, `LOCALE.json` or `SCOPE.LOCALE.json`, one object whose
   only key is the locale; the files of a locale are merged and a key
   defined twice (in one object or across files) is a load error naming both
   files. Keys are dotted `[A-Za-z0-9_-]+` segments; values are strings,
   objects, plural objects, numbers, booleans and lists of strings or
   `null`. Same limits (1 MiB per file, 512 files, 16 levels, 50,000 keys
   per locale, 256-byte keys). Locales are the validated BCP 47 subset
   `language[-Script][-REGION]`, canonicalized (`pt-br` → `pt-BR`); files
   must be named canonically. The parser is a serde visitor of Forge's own
   that refuses duplicate keys and nesting, on `serde_json`.
2. **Interpolation and escaping: as Forge-Go.** `%{name}` (not recursive),
   `%%`, a placeholder without a value stays visible and is logged; keys
   ending in `_html` (or named `html`) are trusted markup whose arguments are
   HTML-escaped unless passed as markup; a missing `_html` key is never
   markup.
3. **CLDR 46 cardinal plural rules: Forge-Go's table and samples, one to
   one**, including Arabic on n (`0.0`, `1.0`, `2.0` and `103.0` are zero,
   one, two and few, `n % 100` exact past 2^53), Norwegian on `n = 1`, and
   NaN and infinity as `other` in every locale through one guard in front of
   built-in and custom rules (`forge_i18n::plural_rule_for`, custom rules with
   `Options::plural_rule`), including the explicit `zero` form and the
   fallback to `other`.
4. **Negotiation: Forge-Go's order and limits.** Path prefix (opt-in),
   query parameter (opt-in), the `locale` cookie (`Secure`, `HttpOnly`,
   `SameSite=Lax`, a year), `Accept-Language` (4096 bytes, 32 entries,
   strict q-values, truncation of unsupported subtags, `*` is the default),
   then the default locale: `FORGE_I18N_DEFAULT_LOCALE` (validated and
   canonical, in `forge-config`'s `KNOWN_KEYS`), else the application's
   `DEFAULT_LOCALE` (`forge new --locale`), else `en`.
5. **Missing translations: as Forge-Go.** `translation missing:
   LOCALE.KEY` (escaped) in development and test; in production the default
   locale's text, then the key humanized. One warning per locale and key for
   at most 1,024 distinct problems, then a single "missing-translation
   warnings suppressed" line and a counter (`suppressed_warnings`).
6. **Formatting: Forge-Go's Rails keys and strftime subset.** Numbers and
   dates read `number.format.*`, `date.*` and `time.*` through the locale,
   its parents, the default locale and built-in English. Dates are a
   framework-owned `forge_i18n::DateTime` (an instant with an offset and an
   optional zone name), so the core needs no date library; applications
   convert with `DateTime::from_unix`.
7. **Crates and rings.** `forge-i18n` is the core (catalog, translator,
   plural rules, matching, formatting), with no HTTP or template
   dependency, so jobs and mailers use it directly; the facade exposes it as
   `forge::i18n` behind the feature `i18n`, which `views` enables (like
   `mail`). The request side is in the HTTP adapter ring, in `forge-views`:
   `LocaleNegotiation` (a `forge-http` middleware), the request binding
   `I18n` and the template helpers. `forge-http` gains `Request::set_uri`
   so the layer can route `/pt-BR/posts` as `/posts`.
8. **Template helpers.** Askama templates call methods on the binding the
   layout carries: `layout.i18n.t("key")` with `.with("name", value)` and
   `.count(n)`, `layout.i18n.l(&date, "short")`, `layout.i18n.number(n)`,
   `layout.i18n.number_with_precision(n, 2)` and `layout.i18n.locale()`.
   A `Translation` displays as HTML (plain keys escaped, `_html` keys as
   markup with escaped arguments) and is `askama::filters::HtmlSafe`, so
   Askama does not escape it twice and `|safe` cannot turn a plain key into
   markup. `Translation::text()` is the text as written, for flash messages
   and other non-HTML contexts.
9. **Translated responses only.** The binding records that it was used; the
   layer then adds `Content-Language` and `Vary: Accept-Language, Cookie`
   to that response. Responses that never touched it (digested assets,
   JSON, health probes) get neither and stay cacheable.
10. **Generators.** `forge new [--locale en|es|pt-BR]` writes
    `config/locales/en.json` and the chosen locale's file (layout and
    welcome page, number and date formats) and, with the sign-in pages,
    `authentication.LOCALE.json`, the same keys and texts as Forge-Go's
    files (product names say Forge Rust). `build.rs` embeds the directory
    (`forge_views::build::embed_locales`); `views::locales::translator`
    loads it at start-up. The layout, welcome page, sign-in and
    password-reset pages and their messages are translated (throttling
    messages are plural by minutes). `forge generate scaffold` writes
    `config/locales/PLURAL.en.json`, and `PLURAL.LOCALE.json` with the same
    English text when the application's default locale (`DEFAULT_LOCALE` in
    `src/adapters/http/views/locales.rs`) is another one; it refuses the
    plurals `date`, `time` and `number` (formatting scopes). `forge generate
    authentication` writes `authentication.en.json` plus the Spanish and
    Portuguese files when the application has those locales.
11. **`forge i18n check [--json]`.** Read-only, Forge-Go's report and JSON
    schema (`schema_version` 1) and exit status: 1 when a locale lacks a key
    or a plural form. Literal keys are `.t("KEY")` in `*.html` and `*.rs`
    files and `translate(LOCALE, "KEY")` / `translate_html(LOCALE, "KEY")`
    in Rust, outside `tests`, `target`, `vendor`, `node_modules`, `tmp`,
    `bin`, `storage` and dot directories; `date.`, `time.` and `number.`
    data is never unused.

## Deviations from Forge-Go

- **Template syntax** is Askama's: methods on `layout.i18n` instead of the
  `t`/`l`/`number`/`locale` template functions; arguments are builder calls
  (`.with`, `.count`), so the argument-shape errors Forge-Go's `views.Load`
  reports cannot be written. The page title is a `title` block of each page.
- **Headers** are set by the negotiation layer for responses that used the
  binding, instead of by the renderer; `set_content_language` covers a
  translated response built another way.
- **Path prefix and security layers.** Amended 2026-10-25: the prefix is
  removed in the router's rewrite step (`Middleware::rewrite`, run for every
  layer before the first one handles the request), so every layer sees the
  stripped path whatever the registration order, and a path guard
  registered before `LocaleNegotiation` can no longer be bypassed with a
  prefix. (Originally layers registered before it saw the prefixed path and
  path-sensitive layers had to go after it, as in Forge-Go's `App.Use` after
  the i18n middleware; Forge-Go keeps that rule, a parity follow-up.) The layer keeps the removed
  segment as a request extension holding the locale's canonical name, read
  with `forge_views::path_prefix` (Forge-Go: `i18n.PathPrefix`), and
  `SignInRedirect` puts it back in `return_to` only when the result still
  passes `return_path`.
- **`LocaleNegotiation::negotiate` and `Vary`.** Decision: documented, not
  marked. `negotiate` takes a request and returns a locale; it has no
  response, so it cannot mark one, and a hidden side effect on a shared flag
  would mark responses that never used the locale. A handler that branches on
  it calls `set_content_language` (or adds the `Vary`) itself. Forge-Go's
  `Translator.Negotiate` has the same gap and the same answer for now
  (`SetHeaders` marks a response); the two stay in step.
- **Path prefix and the raw path.** `http::Uri` keeps only the escaped
  path, which is what routing matches, so the locale segment is read from
  the escaped path: `/pt-BR%2F..%2Fadmin` has no locale segment and is left
  untouched (Forge-Go strips the decoded segment and clears `RawPath`).
  Either way the routed path and the escaped path cannot disagree.
- **Keys only Forge-Rust's pages need:** `sessions.new.already_signed_in`
  (the sign-in page offers sign-out to a signed-in browser). Forge-Go's
  keys of features Forge-Rust does not have yet (`sessions.mfa.*`,
  `sessions.invalid_code`, `throttled.code`, `sessions.sign_in_to_continue`,
  `sessions.expired`) are not generated, so `forge i18n check` reports
  nothing unused in a new application; they return with those features.
  Scaffold keys follow Forge-Rust's pages (`form.create`/`form.update`,
  `show.confirm_delete`, `form.utc`, `flash.no_version`).
- **`forge destroy`** does not exist in Forge-Rust yet, so the
  remove-unless-translated rule for `PLURAL.LOCALE.json` comes with it.
- **Older applications** are not patched: `forge generate scaffold` and
  `authentication` refuse an application without
  `src/adapters/http/views/locales.rs` and point to the upgrade steps in
  the guide, since generators never edit existing files.
- **Usage errors** (`forge i18n` without `check`) are clap's (exit 2).

## Consequences

- New applications are translatable from the first commit; the Spanish and
  Brazilian Portuguese pages ship complete, and generated tests fail on any
  `translation missing` in the pages they render.
- Translation files are code: embedded at build time, reviewed like
  templates, never loaded from user input.
- No new package in `Cargo.lock`; `forge-i18n` depends on `serde`,
  `serde_json`, `thiserror` and `tracing`.

## Follow-ups

- Framework validation messages (`FieldErrors` texts, domain errors such as
  "must not be blank") and the scaffold's Yes/No and RFC 3339 values are
  English, as in Forge-Go.
- Mailer templates with the recipient's locale; a `locale_path` helper for
  path-prefix applications (the sign-in return path already keeps the
  prefix); `forge destroy` (with the generators,
  [forge-rust#15](https://github.com/rocketinsidecorp/forge-rust/issues/15)).
- Resolved 2026-10-25 for Forge-Rust by the rewrite step above; Forge-Go
  still relies on the registration order of path-sensitive middleware.
- Broadcast fragments render in the default locale for every subscriber, as
  in Forge-Go and Rails.
