//! Runtime side of the asset pipeline: a digested, embedded, read-only
//! asset table served from the router.
//!
//! The table comes from [`crate::build::embed_assets`] (generated into
//! `OUT_DIR` by the application's `build.rs`). [`AssetSet`] validates it once,
//! maps logical names to digested URLs for templates, and registers one
//! `GET` route per entry. Because every entry is its own exact route, nothing
//! outside the manifest can be reached: there is no directory listing, no
//! wildcard, and `..`, percent-encoded separators, undigested names and
//! absolute paths simply match no route.
//!
//! `HEAD` is served by the `GET` route without the body, as for every
//! route; conditional `GET` with `If-None-Match` answers `304`.

use std::{collections::HashMap, sync::Arc};

use bytes::Bytes;
use forge_http::{HeaderValue, Method, Request, Response, Router, StatusCode, header};
use thiserror::Error;

use crate::common::{content_type, digested_name, extension_of, valid_component, valid_logical};

/// Default URL prefix for [`AssetSet::new`].
pub const DEFAULT_PREFIX: &str = "/assets";

/// `Cache-Control` for digested names: the URL changes when the content does.
const CACHE_CONTROL: &str = "public, max-age=31536000, immutable";
/// Locks down an SVG opened directly (not through `<img>`): no scripts, no
/// network, no framing.
const SVG_CSP: &str = "default-src 'none'; style-src 'unsafe-inline'; sandbox";

/// One embedded file. Constructed by the code that
/// [`crate::build::embed_assets`] generates; applications do not build these
/// by hand.
#[derive(Clone, Copy, Debug)]
pub struct AssetEntry {
    logical: &'static str,
    digested: &'static str,
    digest: &'static str,
    bytes: &'static [u8],
}

impl AssetEntry {
    /// Creates an entry. [`AssetSet::new`] validates it: the digested name
    /// must be derived from the logical name and the digest.
    #[must_use]
    pub const fn new(
        logical: &'static str,
        digested: &'static str,
        digest: &'static str,
        bytes: &'static [u8],
    ) -> Self {
        Self {
            logical,
            digested,
            digest,
            bytes,
        }
    }

    /// The logical path (`css/app.css`).
    #[must_use]
    pub const fn logical(&self) -> &'static str {
        self.logical
    }

    /// The digested path (`css/app-<16 hex>.css`), relative to the prefix.
    #[must_use]
    pub const fn digested(&self) -> &'static str {
        self.digested
    }

    /// The full SHA-256 content digest, lowercase hex.
    #[must_use]
    pub const fn digest(&self) -> &'static str {
        self.digest
    }

    /// The embedded file contents.
    #[must_use]
    pub const fn bytes(&self) -> &'static [u8] {
        self.bytes
    }
}

/// Invalid asset table, prefix or route registration.
#[derive(Debug, Error, Eq, PartialEq)]
pub enum AssetError {
    /// The URL prefix is not an absolute path of safe segments.
    #[error("invalid asset prefix {0:?}: use an absolute path such as \"/assets\"")]
    InvalidPrefix(String),
    /// A logical name is not a relative path of legal segments.
    #[error("invalid logical asset name {0:?}")]
    InvalidLogicalName(String),
    /// The extension is not on the served allowlist.
    #[error("asset {0:?} has an extension that is not served")]
    UnsupportedExtension(String),
    /// The digest is not 64 lowercase hex characters.
    #[error("asset {0:?} has a malformed digest")]
    InvalidDigest(String),
    /// The digested name is not the logical name plus the digest prefix.
    #[error("asset {0:?} has a digested name that does not match its digest")]
    DigestedNameMismatch(String),
    /// Two entries share a logical name.
    #[error("asset {0:?} appears more than once")]
    Duplicate(String),
    /// The router rejected an asset route (for example, registering the same
    /// set twice).
    #[error(transparent)]
    Route(#[from] forge_http::RouteError),
}

/// A validated asset table bound to a URL prefix.
#[derive(Debug)]
pub struct AssetSet {
    prefix: String,
    assets: Vec<Arc<Served>>,
    by_logical: HashMap<&'static str, usize>,
}

/// Everything needed to answer a request for one asset, computed once.
#[derive(Debug)]
struct Served {
    entry: AssetEntry,
    url: String,
    content_type: HeaderValue,
    etag: HeaderValue,
    is_svg: bool,
}

impl AssetSet {
    /// Validates `table` and serves it under [`DEFAULT_PREFIX`].
    pub fn new(table: &'static [AssetEntry]) -> Result<Self, AssetError> {
        Self::with_prefix(DEFAULT_PREFIX, table)
    }

    /// Validates several tables, such as the application's `FORGE_ASSETS`
    /// and [`vendor::HOTWIRE_ASSETS`](crate::vendor::HOTWIRE_ASSETS), and
    /// serves them together under [`DEFAULT_PREFIX`]. A logical name present
    /// in two tables is [`AssetError::Duplicate`].
    pub fn from_tables(tables: &[&'static [AssetEntry]]) -> Result<Self, AssetError> {
        Self::build(DEFAULT_PREFIX, tables)
    }

    /// Validates `table` and serves it under `prefix` (an absolute path such
    /// as `/assets`, without a trailing slash).
    pub fn with_prefix(prefix: &str, table: &'static [AssetEntry]) -> Result<Self, AssetError> {
        Self::build(prefix, &[table])
    }

    fn build(prefix: &str, tables: &[&'static [AssetEntry]]) -> Result<Self, AssetError> {
        validate_prefix(prefix)?;
        let count = tables.iter().map(|table| table.len()).sum();
        let mut assets = Vec::with_capacity(count);
        let mut by_logical = HashMap::with_capacity(count);
        for entry in tables.iter().flat_map(|table| table.iter()) {
            let served = Served::new(prefix, *entry)?;
            if by_logical.insert(entry.logical, assets.len()).is_some() {
                return Err(AssetError::Duplicate(entry.logical.to_owned()));
            }
            assets.push(Arc::new(served));
        }
        Ok(Self {
            prefix: prefix.to_owned(),
            assets,
            by_logical,
        })
    }

    /// The URL prefix assets are served under.
    #[must_use]
    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    /// The digested URL for a logical name, such as
    /// `/assets/app-0123456789abcdef.css` for `app.css`. `None` when the
    /// logical name is not in the manifest, so a typo cannot silently produce
    /// a broken or undigested link.
    #[must_use]
    pub fn path(&self, logical: &str) -> Option<&str> {
        self.served(logical).map(|served| served.url.as_str())
    }

    /// The number of assets in the set.
    #[must_use]
    pub fn len(&self) -> usize {
        self.assets.len()
    }

    /// Whether the set has no assets.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.assets.is_empty()
    }

    fn served(&self, logical: &str) -> Option<&Served> {
        self.by_logical
            .get(logical)
            .and_then(|index| self.assets.get(*index))
            .map(AsRef::as_ref)
    }

    /// The embedded entry for a logical name.
    #[must_use]
    pub fn entry(&self, logical: &str) -> Option<&AssetEntry> {
        self.served(logical).map(|served| &served.entry)
    }

    /// Registers one `GET` route per manifest entry on `router`.
    ///
    /// Only exact digested URLs are routed. Responses carry
    /// `Content-Type` (by extension), `Cache-Control: public,
    /// max-age=31536000, immutable`, `X-Content-Type-Options: nosniff` and
    /// `ETag` (the content digest); a matching `If-None-Match` answers `304`.
    pub fn register(&self, router: &mut Router) -> Result<(), AssetError> {
        for served in &self.assets {
            let served = Arc::clone(served);
            let url = served.url.clone();
            router.route(Method::GET, url, move |request: Request| {
                let served = Arc::clone(&served);
                async move { served.respond(&request) }
            })?;
        }
        Ok(())
    }
}

impl Served {
    fn new(prefix: &str, entry: AssetEntry) -> Result<Self, AssetError> {
        let logical = entry.logical;
        if !valid_logical(logical) {
            return Err(AssetError::InvalidLogicalName(logical.to_owned()));
        }
        let extension = extension_of(logical).map(str::to_ascii_lowercase);
        let Some((extension, content_type)) = extension
            .and_then(|extension| content_type(&extension).map(|media| (extension, media)))
        else {
            return Err(AssetError::UnsupportedExtension(logical.to_owned()));
        };
        let well_formed = entry.digest.len() == 64
            && entry
                .digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'));
        if !well_formed {
            return Err(AssetError::InvalidDigest(logical.to_owned()));
        }
        if digested_name(logical, entry.digest).as_deref() != Some(entry.digested) {
            return Err(AssetError::DigestedNameMismatch(logical.to_owned()));
        }
        let etag = HeaderValue::from_str(&format!("\"{}\"", entry.digest))
            .map_err(|_| AssetError::InvalidDigest(logical.to_owned()))?;
        Ok(Self {
            entry,
            url: format!("{prefix}/{}", entry.digested),
            content_type: HeaderValue::from_static(content_type),
            etag,
            is_svg: extension == "svg",
        })
    }

    fn respond(&self, request: &Request) -> Response {
        let not_modified = request
            .headers()
            .get_all(header::IF_NONE_MATCH)
            .iter()
            .any(|value| matches_etag(value, self.entry.digest));
        let mut response = if not_modified {
            Response::empty(StatusCode::NOT_MODIFIED)
        } else {
            let mut full = Response::new(StatusCode::OK, Bytes::from_static(self.entry.bytes));
            full.headers_mut()
                .insert(header::CONTENT_TYPE, self.content_type.clone());
            full
        };
        let headers = response.headers_mut();
        headers.insert(header::ETAG, self.etag.clone());
        headers.insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static(CACHE_CONTROL),
        );
        headers.insert(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        );
        if self.is_svg {
            headers.insert(
                header::CONTENT_SECURITY_POLICY,
                HeaderValue::from_static(SVG_CSP),
            );
        }
        response
    }
}

/// Whether an `If-None-Match` header names this digest. Weak comparison, as
/// RFC 9110 prescribes for `If-None-Match`; `*` matches any current entity.
fn matches_etag(value: &HeaderValue, digest: &str) -> bool {
    let Ok(text) = value.to_str() else {
        return false;
    };
    text.split(',').map(str::trim).any(|candidate| {
        let candidate = candidate.strip_prefix("W/").unwrap_or(candidate);
        candidate == "*"
            || candidate
                .strip_prefix('"')
                .and_then(|rest| rest.strip_suffix('"'))
                == Some(digest)
    })
}

fn validate_prefix(prefix: &str) -> Result<(), AssetError> {
    let valid = prefix
        .strip_prefix('/')
        .is_some_and(|rest| !rest.is_empty() && rest.split('/').all(valid_component));
    if valid {
        Ok(())
    } else {
        Err(AssetError::InvalidPrefix(prefix.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{get, header as header_of, request_with, text};

    const CSS_DIGEST: &str = "7ada5a1234b13c6cafa63c438ae59e1a35173601906230c4b7508722aa1597f6";
    const JS_DIGEST: &str = "a2098bd92b10bf8b816d24b7556b1ce8c49a879d130489065ef1051c17e042f6";
    const SVG_DIGEST: &str = "900fbe934249ad120004bd24adf66aad8817d89586273c0cc50e187bddebb601";
    const CSS_URL: &str = "/assets/app-7ada5a1234b13c6c.css";

    static TABLE: &[AssetEntry] = &[
        AssetEntry::new(
            "app.css",
            "app-7ada5a1234b13c6c.css",
            CSS_DIGEST,
            b"body{color:black}",
        ),
        AssetEntry::new(
            "js/app.js",
            "js/app-a2098bd92b10bf8b.js",
            JS_DIGEST,
            b"export const answer = 42;\n",
        ),
        AssetEntry::new(
            "icon.svg",
            "icon-900fbe934249ad12.svg",
            SVG_DIGEST,
            b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>",
        ),
    ];

    fn router() -> Router {
        let mut router = Router::new();
        AssetSet::new(TABLE)
            .expect("valid table")
            .register(&mut router)
            .expect("routes register");
        router
    }

    fn etag() -> String {
        format!("\"{CSS_DIGEST}\"")
    }

    #[test]
    fn path_maps_logical_names_to_digested_urls() {
        let set = AssetSet::new(TABLE).expect("valid table");

        assert_eq!(set.path("app.css"), Some(CSS_URL));
        assert_eq!(
            set.path("js/app.js"),
            Some("/assets/js/app-a2098bd92b10bf8b.js")
        );
        assert_eq!(set.path("missing.css"), None);
        assert_eq!(set.path("/app.css"), None);
        assert_eq!(set.len(), 3);
        assert!(!set.is_empty());
        assert_eq!(set.prefix(), "/assets");
    }

    #[test]
    fn custom_prefix_changes_urls() {
        let set = AssetSet::with_prefix("/static/v1", TABLE).expect("valid table");

        assert_eq!(
            set.path("app.css"),
            Some("/static/v1/app-7ada5a1234b13c6c.css")
        );
    }

    #[test]
    fn invalid_prefixes_are_rejected() {
        for prefix in [
            "", "/", "assets", "/assets/", "/a//b", "/a/../b", "/a b", "/a{b}",
        ] {
            assert_eq!(
                AssetSet::with_prefix(prefix, TABLE).expect_err(prefix),
                AssetError::InvalidPrefix(prefix.to_owned())
            );
        }
    }

    #[tokio::test]
    async fn serves_a_digested_asset_with_security_and_cache_headers() {
        let response = router().dispatch(get(CSS_URL)).await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(text(&response), "body{color:black}");
        assert_eq!(
            header_of(&response, "content-type"),
            Some("text/css; charset=utf-8")
        );
        assert_eq!(
            header_of(&response, "cache-control"),
            Some("public, max-age=31536000, immutable")
        );
        assert_eq!(
            header_of(&response, "x-content-type-options"),
            Some("nosniff")
        );
        assert_eq!(header_of(&response, "etag"), Some(etag().as_str()));
        assert_eq!(header_of(&response, "content-security-policy"), None);
    }

    #[tokio::test]
    async fn nested_assets_and_media_types() {
        let router = router();
        let js = router
            .dispatch(get("/assets/js/app-a2098bd92b10bf8b.js"))
            .await;
        let svg = router
            .dispatch(get("/assets/icon-900fbe934249ad12.svg"))
            .await;

        assert_eq!(js.status(), StatusCode::OK);
        assert_eq!(
            header_of(&js, "content-type"),
            Some("text/javascript; charset=utf-8")
        );
        assert_eq!(header_of(&svg, "content-type"), Some("image/svg+xml"));
        assert_eq!(
            header_of(&svg, "content-security-policy"),
            Some("default-src 'none'; style-src 'unsafe-inline'; sandbox")
        );
    }

    #[tokio::test]
    async fn matching_etag_answers_not_modified_without_a_body() {
        let router = router();
        for value in [
            etag(),
            format!("W/{}", etag()),
            format!("\"other\", {}", etag()),
            "*".to_owned(),
        ] {
            let response = router
                .dispatch(request_with(
                    Method::GET,
                    CSS_URL,
                    &[("if-none-match", &value)],
                ))
                .await;

            assert_eq!(response.status(), StatusCode::NOT_MODIFIED, "{value}");
            assert!(response.body().is_empty(), "{value}");
            assert_eq!(header_of(&response, "etag"), Some(etag().as_str()));
            assert_eq!(
                header_of(&response, "cache-control"),
                Some("public, max-age=31536000, immutable")
            );
        }
    }

    #[tokio::test]
    async fn non_matching_etag_serves_the_full_body() {
        let response = router()
            .dispatch(request_with(
                Method::GET,
                CSS_URL,
                &[("if-none-match", "\"stale\"")],
            ))
            .await;

        assert_eq!(response.status(), StatusCode::OK);
        assert!(!response.body().is_empty());
    }

    #[tokio::test]
    async fn logical_undigested_and_wrong_digest_names_are_not_found() {
        let router = router();
        for path in [
            "/assets/app.css",
            "/assets/js/app.js",
            "/assets/app-0000000000000000.css",
            "/assets/app-7ada5a1234b13c6c.js",
            "/assets/",
            "/assets",
            "/assets/js",
            "/assets/js/",
            "/app-7ada5a1234b13c6c.css",
        ] {
            let response = router.dispatch(get(path)).await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
        }
    }

    #[tokio::test]
    async fn traversal_and_encoded_separator_attempts_are_never_served() {
        let router = router();
        for path in [
            "/assets/../assets/app-7ada5a1234b13c6c.css",
            "/assets/js/../app-7ada5a1234b13c6c.css",
            "/assets/%2e%2e/app-7ada5a1234b13c6c.css",
            "/assets/..%2fapp-7ada5a1234b13c6c.css",
            "/assets/js%2Fapp-a2098bd92b10bf8b.js",
            "/assets/%61pp-7ada5a1234b13c6c.css",
            "//assets/app-7ada5a1234b13c6c.css",
            "/assets//app-7ada5a1234b13c6c.css",
            "/assets/app-7ada5a1234b13c6c.css/",
            "/assets/app-7ada5a1234b13c6c.css/extra",
            "/etc/passwd",
        ] {
            let response = router.dispatch(get(path)).await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
        }
    }

    #[tokio::test]
    async fn only_get_is_routed() {
        let response = router()
            .dispatch(request_with(Method::POST, CSS_URL, &[]))
            .await;

        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(header_of(&response, "allow"), Some("GET, HEAD"));
    }

    #[test]
    fn registering_a_set_twice_is_a_route_error() {
        let mut router = Router::new();
        let set = AssetSet::new(TABLE).expect("valid table");
        set.register(&mut router).expect("first registration");

        assert!(matches!(
            set.register(&mut router).expect_err("duplicate"),
            AssetError::Route(_)
        ));
    }

    #[test]
    fn tampered_tables_are_rejected() {
        static UNKNOWN_EXTENSION: &[AssetEntry] = &[AssetEntry::new(
            "page.html",
            "page-7ada5a1234b13c6c.html",
            CSS_DIGEST,
            b"<p>",
        )];
        static NO_EXTENSION: &[AssetEntry] =
            &[AssetEntry::new("LICENSE", "LICENSE", CSS_DIGEST, b"x")];
        static BAD_DIGEST: &[AssetEntry] = &[AssetEntry::new(
            "app.css",
            "app-7ada5a1234b13c6c.css",
            "XYZ",
            b"x",
        )];
        static WRONG_NAME: &[AssetEntry] = &[AssetEntry::new(
            "app.css",
            "app-ffffffffffffffff.css",
            CSS_DIGEST,
            b"x",
        )];
        static TRAVERSAL: &[AssetEntry] = &[AssetEntry::new(
            "../secret.css",
            "../secret-7ada5a1234b13c6c.css",
            CSS_DIGEST,
            b"x",
        )];
        static DUPLICATE: &[AssetEntry] = &[
            AssetEntry::new("app.css", "app-7ada5a1234b13c6c.css", CSS_DIGEST, b"x"),
            AssetEntry::new("app.css", "app-7ada5a1234b13c6c.css", CSS_DIGEST, b"x"),
        ];

        assert_eq!(
            AssetSet::new(UNKNOWN_EXTENSION).expect_err("html"),
            AssetError::UnsupportedExtension("page.html".to_owned())
        );
        assert!(matches!(
            AssetSet::new(NO_EXTENSION).expect_err("no extension"),
            AssetError::InvalidLogicalName(_) | AssetError::UnsupportedExtension(_)
        ));
        assert!(matches!(
            AssetSet::new(BAD_DIGEST).expect_err("digest"),
            AssetError::InvalidDigest(_)
        ));
        assert!(matches!(
            AssetSet::new(WRONG_NAME).expect_err("name"),
            AssetError::DigestedNameMismatch(_)
        ));
        assert!(matches!(
            AssetSet::new(TRAVERSAL).expect_err("traversal"),
            AssetError::InvalidLogicalName(_)
        ));
        assert!(matches!(
            AssetSet::new(DUPLICATE).expect_err("duplicate"),
            AssetError::Duplicate(_)
        ));
    }

    #[test]
    fn from_tables_serves_several_tables_and_rejects_cross_table_duplicates() {
        let merged = AssetSet::from_tables(&[TABLE, crate::vendor::HOTWIRE_ASSETS])
            .expect("disjoint tables");
        assert_eq!(
            merged.len(),
            TABLE.len() + crate::vendor::HOTWIRE_ASSETS.len()
        );
        assert_eq!(merged.path("app.css"), Some(CSS_URL));
        assert!(merged.path(crate::vendor::TURBO_ASSET).is_some());

        assert!(matches!(
            AssetSet::from_tables(&[TABLE, TABLE]).expect_err("same names twice"),
            AssetError::Duplicate(_)
        ));
        assert!(AssetSet::from_tables(&[]).expect("empty").is_empty());
    }

    #[test]
    fn matches_etag_ignores_unparsable_values() {
        let value = HeaderValue::from_bytes(&[0xff, b'"']).expect("opaque bytes");

        assert!(!matches_etag(&value, CSS_DIGEST));
    }
}
