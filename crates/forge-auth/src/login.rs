//! The one canonical form of a login (an email address) used for storage,
//! lookup, throttling and delivery.
//!
//! Unicode logins are refused at the boundary: case mapping differs between
//! Rust, PostgreSQL collations and mail systems (PostgreSQL may fold
//! `user@İcloud.com` to `user@icloud.com`), so a look-alike spelling could
//! match another account, get its own throttle counter, or receive another
//! account's reset link. A canonical login is trimmed, ASCII only, without
//! whitespace or control characters, at most [`MAX_LOGIN_BYTES`] bytes, and
//! ASCII-lowercased.

use thiserror::Error;

/// Longest login accepted, in bytes.
pub const MAX_LOGIN_BYTES: usize = 320;

/// A login in its canonical form.
#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CanonicalLogin(String);

impl CanonicalLogin {
    /// Canonicalizes what a person typed: surrounding whitespace removed,
    /// ASCII letters lowercased. Anything non-ASCII, any whitespace or
    /// control character inside, an empty or a too long value is refused.
    pub fn parse(input: &str) -> Result<Self, LoginFormatError> {
        let trimmed = input.trim();
        if trimmed.is_empty() || trimmed.len() > MAX_LOGIN_BYTES {
            return Err(LoginFormatError);
        }
        if !trimmed.bytes().all(|byte| byte.is_ascii_graphic()) {
            return Err(LoginFormatError);
        }
        Ok(Self(trimmed.to_ascii_lowercase()))
    }

    /// The canonical text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for CanonicalLogin {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("CanonicalLogin([REDACTED])")
    }
}

impl std::fmt::Display for CanonicalLogin {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// The login is not a canonical ASCII login.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[error("a login is 1 to 320 ASCII characters without spaces or control characters")]
pub struct LoginFormatError;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logins_are_trimmed_and_ascii_lowercased() {
        let login = CanonicalLogin::parse("  Ada.Lovelace+x@Example.COM \n").expect("valid");
        assert_eq!(login.as_str(), "ada.lovelace+x@example.com");
        assert_eq!(format!("{login:?}"), "CanonicalLogin([REDACTED])");
    }

    #[test]
    fn unicode_whitespace_control_and_oversized_logins_are_refused() {
        for invalid in [
            "",
            "   ",
            "user@\u{130}cloud.com",
            "user@icloud.com\u{0}",
            "us er@example.com",
            "user@ex\tample.com",
            "\u{212a}elvin@example.com",
            "caf\u{e9}@example.com",
            &format!("{}@x", "a".repeat(MAX_LOGIN_BYTES)),
        ] {
            assert_eq!(
                CanonicalLogin::parse(invalid),
                Err(LoginFormatError),
                "{invalid:?}"
            );
        }
    }
}
