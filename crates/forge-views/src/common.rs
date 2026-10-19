//! Naming and media-type rules shared by the build-time embedder and the
//! runtime asset server, so what the build accepts is exactly what the server
//! can serve.

/// Number of hex characters of the content digest embedded in a digested
/// file name (64 bits: collision-resistant enough for cache busting, short
/// enough for readable URLs). The full digest is the `ETag`.
pub(crate) const DIGEST_URL_HEX: usize = 16;

/// The media type served for a lowercase file extension, or `None` when the
/// extension is not on the closed allowlist.
pub(crate) fn content_type(extension: &str) -> Option<&'static str> {
    Some(match extension {
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "map" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "txt" => "text/plain; charset=utf-8",
        _ => return None,
    })
}

/// The extension (text after the last `.`) of the final path segment.
pub(crate) fn extension_of(path: &str) -> Option<&str> {
    let file = path.rsplit('/').next()?;
    let (stem, extension) = file.rsplit_once('.')?;
    (!stem.is_empty() && !extension.is_empty()).then_some(extension)
}

/// Whether `component` is a legal logical path segment: ASCII letters,
/// digits, `.`, `_` and `-`, not starting with a dot. These characters need
/// no URL escaping and cannot be confused with route pattern syntax.
pub(crate) fn valid_component(component: &str) -> bool {
    !component.is_empty()
        && !component.starts_with('.')
        && component
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

/// Whether `logical` is a relative, `/`-separated path of legal segments.
#[cfg(feature = "runtime")]
pub(crate) fn valid_logical(logical: &str) -> bool {
    logical.split('/').all(valid_component)
}

/// `css/app.css` and a digest become `css/app-<16 hex>.css`.
pub(crate) fn digested_name(logical: &str, digest_hex: &str) -> Option<String> {
    let extension = extension_of(logical)?;
    let stem = logical.strip_suffix(extension)?.strip_suffix('.')?;
    let short = digest_hex.get(..DIGEST_URL_HEX)?;
    Some(format!("{stem}-{short}.{extension}"))
}

/// Lowercase hexadecimal encoding.
pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        // Writing to a `String` cannot fail.
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digested_names_keep_directory_and_extension() {
        let digest = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        assert_eq!(
            digested_name("css/app.css", digest).as_deref(),
            Some("css/app-0123456789abcdef.css")
        );
        assert_eq!(
            digested_name("a.min.js", digest).as_deref(),
            Some("a.min-0123456789abcdef.js")
        );
        assert_eq!(digested_name("README", digest), None);
        assert_eq!(digested_name("app.css", "abc"), None);
    }

    #[cfg(feature = "runtime")]
    #[test]
    fn components_reject_dotfiles_and_unsafe_characters() {
        for good in ["app.css", "a-b_c.js", "css/app.css"] {
            assert!(valid_logical(good), "{good}");
        }
        for bad in [
            "",
            ".env",
            "a/.hidden",
            "..",
            "a/../b",
            "a b.css",
            "a{b}.css",
            "a?b.css",
            "a%2Fb.css",
            "/abs.css",
            "a//b.css",
            "é.css",
            "a\\b.css",
        ] {
            assert!(!valid_logical(bad), "{bad:?}");
        }
    }

    #[test]
    fn only_allowlisted_extensions_have_media_types() {
        assert_eq!(content_type("css"), Some("text/css; charset=utf-8"));
        assert_eq!(content_type("woff2"), Some("font/woff2"));
        for other in ["html", "htm", "php", "exe", "json", "wasm", "xml", ""] {
            assert_eq!(content_type(other), None, "{other}");
        }
    }

    #[cfg(feature = "build")]
    #[test]
    fn hex_is_lowercase_and_padded() {
        assert_eq!(hex(&[0x00, 0x0f, 0xff]), "000fff");
    }
}
