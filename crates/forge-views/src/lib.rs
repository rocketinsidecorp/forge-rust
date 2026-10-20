//! Server-rendered views for Forge applications (ADR 0022).
//!
//! The crate gives HTTP adapters the primitives behind Rails-style HTML
//! applications, with secure defaults:
//!
//! - [`html`] / [`html_with_status`]: render an Askama view as an HTML
//!   response (auto-escaped; failures become an opaque `500`).
//! - [`see_other`], [`turbo_stream`], [`turbo_stream_with_status`],
//!   [`turbo_stream_requested`], [`TurboStreamAction`], [`DomId`],
//!   [`dom_id`]: Turbo-compatible redirects and stream responses (live
//!   updates over Server-Sent Events are in `forge-broadcast`).
//! - [`AssetSet`] with [`build::embed_assets`]: content-digested assets
//!   embedded in the binary and served from exact manifest entries only.
//! - [`ImportMap`]: bare module specifiers mapped to digested URLs, with the
//!   hash a strict CSP needs.
//! - [`ContentSecurityPolicy`]: a strict default policy without
//!   `unsafe-inline`, also usable as a request layer.
//! - [`CsrfProtection`]: the request layer that enforces CSRF tokens on
//!   every unsafe request (hidden `csrf_token` field or `x-csrf-token`
//!   header) and applies the CSRF-verified `_method` override before
//!   routing; [`csrf_token`] gives handlers the token to render.
//! - [`csrf_meta`], [`csrf_field`], [`method_override`], [`FieldErrors`]:
//!   form helpers.
//! - [`Flash`] and [`FlashLayer`]: one-shot `notice`/`alert` messages.
//! - [`SignInRedirect`], [`return_path`], [`prefers_html`]: send signed-out
//!   browsers to the sign-in page with a validated return path (ADR 0026).
//! - [`vendor`]: the pinned Turbo and Stimulus modules as embedded assets.
//! - [`LocaleNegotiation`], [`I18n`], [`Translation`]: each request's
//!   locale (path prefix, query, `locale` cookie, `Accept-Language`) and the
//!   template helpers `t`, `l`, `number` and `locale` over `forge-i18n`;
//!   [`set_content_language`], [`path_prefix`] and [`locale_cookie`] (ADR 0037).
//!
//! Views belong in the HTTP adapter ring; domain and application code never
//! see this crate. See `docs/views.md` for the developer guide.
//!
//! # Features
//!
//! - `runtime` (default): everything except the build helper.
//! - `build` (default): [`build::embed_assets`]. A `build.rs` should depend
//!   on this crate with `default-features = false, features = ["build"]` to
//!   avoid the HTTP and template dependencies.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

#[cfg(feature = "build")]
pub mod build;
#[cfg(any(feature = "build", feature = "runtime"))]
mod common;

#[cfg(feature = "runtime")]
mod assets;
#[cfg(feature = "runtime")]
mod csp;
#[cfg(feature = "runtime")]
mod csrf;
#[cfg(feature = "runtime")]
mod encoding;
#[cfg(feature = "runtime")]
mod flash;
#[cfg(feature = "runtime")]
mod forms;
#[cfg(feature = "runtime")]
mod i18n;
#[cfg(feature = "runtime")]
mod import_map;
#[cfg(feature = "runtime")]
mod render;
#[cfg(feature = "runtime")]
mod sign_in;
#[cfg(all(test, feature = "runtime"))]
mod test_support;
#[cfg(feature = "runtime")]
mod turbo;
#[cfg(feature = "runtime")]
pub mod vendor;

#[cfg(feature = "runtime")]
pub use assets::{AssetEntry, AssetError, AssetSet, DEFAULT_PREFIX};
#[cfg(feature = "runtime")]
pub use csp::{ContentSecurityPolicy, CspError};
#[cfg(feature = "runtime")]
pub use csrf::{
    CSRF_COOKIE_NAME, CsrfError, CsrfProtection, csrf_cookie, csrf_token, csrf_verified,
    presented_csrf_token,
};
#[cfg(feature = "runtime")]
pub use flash::{FLASH_COOKIE_NAME, Flash, FlashError, FlashLayer, MAX_FLASH_MESSAGE_BYTES, flash};
#[cfg(feature = "runtime")]
pub use forms::{
    BASE_FIELD, CSRF_FIELD_NAME, CSRF_HEADER_NAME, CSRF_META_NAME, CsrfVerified, FieldErrors,
    METHOD_FIELD_NAME, csrf_field, csrf_meta, method_override,
};
#[cfg(feature = "runtime")]
pub use i18n::{
    I18n, I18nError, LOCALE_COOKIE_NAME, LocaleNegotiation, Translation, locale_cookie,
    path_prefix, set_content_language,
};
#[cfg(feature = "runtime")]
pub use import_map::{ImportMap, ImportMapBuilder, ImportMapError};
#[cfg(feature = "runtime")]
pub use render::{html, html_with_status};
#[cfg(feature = "runtime")]
pub use sign_in::{
    MAX_RETURN_PATH_BYTES, RETURN_TO_PARAM, SignInRedirect, prefers_html, return_path,
    sign_in_location,
};
#[cfg(feature = "runtime")]
pub use turbo::{
    DomId, DomIdError, RedirectError, StreamAction, TURBO_STREAM_MEDIA_TYPE, TurboStreamAction,
    TurboStreamError, dom_id, see_other, see_other_to_origin, turbo_stream, turbo_stream_html,
    turbo_stream_requested, turbo_stream_with_status,
};
