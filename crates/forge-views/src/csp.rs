//! Content-Security-Policy and companion headers for HTML responses.

use forge_http::{BoxResponseFuture, HeaderValue, Middleware, Next, Request, Response, header};
use thiserror::Error;

use crate::ImportMap;

/// `Referrer-Policy` applied with the policy.
const REFERRER_POLICY: &str = "strict-origin-when-cross-origin";

/// Maximum number of extra script hashes.
const MAX_SCRIPT_HASHES: usize = 16;

/// Invalid CSP input.
#[derive(Debug, Error, Eq, PartialEq)]
pub enum CspError {
    /// A script hash is not a base64 SHA-256 digest (44 characters).
    #[error("script hash must be the 44-character base64 SHA-256 digest")]
    InvalidScriptHash,
    /// More hashes than the policy allows.
    #[error("a policy allows at most {MAX_SCRIPT_HASHES} script hashes")]
    TooManyScriptHashes,
}

/// A strict Content-Security-Policy.
///
/// The default policy allows only same-origin resources, no plugins, no
/// framing and no inline styles or scripts. It has no `unsafe-inline` and no
/// `unsafe-eval`; the only inline script a page needs, the import map, is
/// allowed by hash through [`with_import_map`](Self::with_import_map).
///
/// ```
/// let policy = forge_views::ContentSecurityPolicy::strict();
/// assert!(policy.header_value().starts_with("default-src 'self'; script-src 'self'; "));
/// ```
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ContentSecurityPolicy {
    script_hashes: Vec<String>,
}

impl ContentSecurityPolicy {
    /// The strict default policy.
    #[must_use]
    pub fn strict() -> Self {
        Self::default()
    }

    /// Additionally allows the import map's inline script by its SHA-256.
    #[must_use]
    pub fn with_import_map(mut self, map: &ImportMap) -> Self {
        let hash = map.sha256_base64().to_owned();
        if !self.script_hashes.contains(&hash) {
            self.script_hashes.push(hash);
        }
        self
    }

    /// Additionally allows an inline script by the base64 SHA-256 of its
    /// exact body. The value is validated, so it cannot inject policy syntax.
    pub fn with_script_hash(mut self, sha256_base64: &str) -> Result<Self, CspError> {
        let valid = sha256_base64.len() == 44
            && sha256_base64.ends_with('=')
            && sha256_base64
                .bytes()
                .take(43)
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/'));
        if !valid {
            return Err(CspError::InvalidScriptHash);
        }
        if self.script_hashes.len() >= MAX_SCRIPT_HASHES {
            return Err(CspError::TooManyScriptHashes);
        }
        if !self.script_hashes.iter().any(|hash| hash == sha256_base64) {
            self.script_hashes.push(sha256_base64.to_owned());
        }
        Ok(self)
    }

    /// The `Content-Security-Policy` header value.
    #[must_use]
    pub fn header_value(&self) -> String {
        let mut script_src = String::from("script-src 'self'");
        for hash in &self.script_hashes {
            script_src.push_str(" 'sha256-");
            script_src.push_str(hash);
            script_src.push('\'');
        }
        format!(
            "default-src 'self'; {script_src}; style-src 'self'; img-src 'self' data:; \
             object-src 'none'; base-uri 'self'; form-action 'self'; frame-ancestors 'none'"
        )
    }

    /// Sets `Content-Security-Policy` and
    /// `Referrer-Policy: strict-origin-when-cross-origin` on an HTML
    /// response, replacing any existing values.
    ///
    /// Header values are built from validated hashes and constants; if a
    /// value could not be encoded (which validation prevents), the response
    /// is left without the policy rather than carrying a partial one, and
    /// the failure is logged.
    pub fn apply(&self, response: &mut Response) {
        match HeaderValue::from_str(&self.header_value()) {
            Ok(value) => {
                let headers = response.headers_mut();
                headers.insert(header::CONTENT_SECURITY_POLICY, value);
                headers.insert(
                    header::REFERRER_POLICY,
                    HeaderValue::from_static(REFERRER_POLICY),
                );
            }
            Err(failure) => {
                tracing::error!(error = %failure, "content security policy is not a valid header value");
            }
        }
    }
}

/// As a request layer, the policy is applied to every response that does not
/// already carry a `Content-Security-Policy` (an SVG asset keeps its own
/// sandboxing policy), so no HTML page can be served without it:
///
/// ```
/// let mut router = forge_http::Router::new();
/// router.layer(forge_views::ContentSecurityPolicy::strict());
/// ```
impl Middleware for ContentSecurityPolicy {
    fn handle<'a>(&'a self, request: Request, next: Next<'a>) -> BoxResponseFuture<'a> {
        Box::pin(async move {
            let mut response = next.run(request).await;
            if !response
                .headers()
                .contains_key(header::CONTENT_SECURITY_POLICY)
            {
                self.apply(&mut response);
            }
            response
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AssetEntry, AssetSet, test_support::header as header_of};
    use forge_http::StatusCode;

    #[test]
    fn strict_policy_is_exact_and_has_no_unsafe_directives() {
        let value = ContentSecurityPolicy::strict().header_value();

        assert_eq!(
            value,
            "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; \
             object-src 'none'; base-uri 'self'; form-action 'self'; frame-ancestors 'none'"
        );
        assert!(!value.contains("unsafe-"));
    }

    #[test]
    fn import_map_hash_is_added_to_script_src_only() {
        static TABLE: &[AssetEntry] = &[AssetEntry::new(
            "turbo.js",
            "turbo-c3449cd99d19554d.js",
            "c3449cd99d19554da590a1d8a779f7c583577738f0bc8af1956d5f74bd51194a",
            b"turbo",
        )];
        let assets = AssetSet::new(TABLE).expect("valid table");
        let map = ImportMap::builder()
            .pin("@hotwired/turbo", "turbo.js")
            .build(&assets)
            .expect("valid map");

        let value = ContentSecurityPolicy::strict()
            .with_import_map(&map)
            .with_import_map(&map)
            .header_value();

        assert_eq!(
            value,
            "default-src 'self'; script-src 'self' 'sha256-mH1ogHvYcN6oG992YDBq8JYlIqdRLaqN80Fj3frcADY='; \
             style-src 'self'; img-src 'self' data:; object-src 'none'; base-uri 'self'; \
             form-action 'self'; frame-ancestors 'none'"
        );
    }

    #[test]
    fn script_hashes_are_validated() {
        let valid = "47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=";
        assert!(
            ContentSecurityPolicy::strict()
                .with_script_hash(valid)
                .is_ok()
        );
        for invalid in [
            "",
            "short",
            "47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU",
            "47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuF'=",
            "'; script-src * ;'47DEQpj8HBSa+/TImW+5JCeuQeRk=",
        ] {
            assert_eq!(
                ContentSecurityPolicy::strict()
                    .with_script_hash(invalid)
                    .expect_err(invalid),
                CspError::InvalidScriptHash,
                "{invalid}"
            );
        }
    }

    #[test]
    fn script_hash_count_is_bounded() {
        let mut policy = ContentSecurityPolicy::strict();
        for index in 0..MAX_SCRIPT_HASHES {
            let letter = char::from(b'a' + u8::try_from(index).expect("small index"));
            let hash = format!("{}{letter}=", "A".repeat(42));
            policy = policy.with_script_hash(&hash).expect("within bound");
        }

        assert_eq!(
            policy
                .with_script_hash(&format!("{}z=", "A".repeat(42)))
                .expect_err("over bound"),
            CspError::TooManyScriptHashes
        );
    }

    #[tokio::test]
    async fn as_a_layer_it_covers_every_response_but_keeps_existing_policies() {
        use forge_http::{Method, Router};

        use crate::test_support::get;

        let mut router = Router::new();
        router.layer(ContentSecurityPolicy::strict());
        router
            .route(Method::GET, "/page", |_| async {
                Response::empty(StatusCode::OK)
            })
            .expect("route");
        router
            .route(Method::GET, "/image.svg", |_| async {
                let mut response = Response::empty(StatusCode::OK);
                response.headers_mut().insert(
                    header::CONTENT_SECURITY_POLICY,
                    HeaderValue::from_static("default-src 'none'; sandbox"),
                );
                response
            })
            .expect("route");

        let page = router.dispatch(get("/page")).await;
        let missing = router.dispatch(get("/missing")).await;
        let svg = router.dispatch(get("/image.svg")).await;

        let strict = ContentSecurityPolicy::strict().header_value();
        assert_eq!(
            header_of(&page, "content-security-policy"),
            Some(strict.as_str())
        );
        assert_eq!(
            header_of(&missing, "content-security-policy"),
            Some(strict.as_str())
        );
        assert_eq!(
            header_of(&svg, "content-security-policy"),
            Some("default-src 'none'; sandbox")
        );
    }

    #[test]
    fn apply_sets_csp_and_referrer_policy_replacing_existing_values() {
        let mut response = Response::empty(StatusCode::OK);
        response.headers_mut().insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static("default-src *"),
        );
        let policy = ContentSecurityPolicy::strict();

        policy.apply(&mut response);

        assert_eq!(
            header_of(&response, "content-security-policy"),
            Some(policy.header_value().as_str())
        );
        assert_eq!(
            header_of(&response, "referrer-policy"),
            Some("strict-origin-when-cross-origin")
        );
        assert_eq!(
            response
                .headers()
                .get_all("content-security-policy")
                .iter()
                .count(),
            1
        );
    }
}
