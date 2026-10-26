//! Bearer credentials of API clients: the `Authorization: Bearer <token>`
//! header (Forge-Go's `auth.Middleware` rules) and the issuance and digest
//! of the opaque token it carries.
//!
//! A bearer token is a [`SessionBearerToken`]: 32 bytes from the operating
//! system's CSPRNG (`getrandom`), as 64 lowercase hex digits. Servers store
//! only its SHA-256 digest ([`SessionBearerToken::digest`], `sha2`) and look
//! sessions up by that digest, so neither a database disclosure nor a timing
//! difference in the lookup reveals a usable token.

use sha2::{Digest as _, Sha256};

use crate::{CredentialError, SessionBearerToken, SessionCredentialDigest};

/// The authentication scheme of [`bearer_credential`], as Forge-Go spells it.
pub const BEARER_SCHEME: &str = "Bearer";

/// The `WWW-Authenticate` value that goes with a `401` for a bearer route.
pub const BEARER_CHALLENGE: &str = "Bearer realm=\"forge\"";

/// Why a request carries no usable bearer credential.
#[derive(Clone, Copy, Debug, thiserror::Error, Eq, PartialEq)]
pub enum BearerCredentialError {
    /// No `Authorization` header: `401 credentials_required`.
    #[error("authentication is required")]
    Missing,
    /// A repeated, combined or malformed header, another scheme or a token
    /// that is not canonical: `401 credentials_invalid`.
    #[error("credentials are invalid or expired")]
    Invalid,
}

/// The bearer token of a request's `Authorization` header values.
///
/// Exactly one header is accepted, holding `Bearer`, one space and a
/// canonical token, and nothing else: a repeated header, a comma (two
/// credentials folded into one), another scheme, extra whitespace or a
/// token in another encoding is [`BearerCredentialError::Invalid`]. Cookies,
/// query parameters and other headers are never read, so they cannot stand
/// in for the header.
///
/// ```
/// use forge_auth::{BearerCredentialError, bearer_credential};
///
/// let token = "ab".repeat(32);
/// let header = format!("Bearer {token}");
/// assert_eq!(bearer_credential([header.as_bytes()])?.expose(), token);
/// assert_eq!(bearer_credential([]), Err(BearerCredentialError::Missing));
/// assert_eq!(
///     bearer_credential([b"Basic dXNlcjpwYXNz".as_slice()]),
///     Err(BearerCredentialError::Invalid)
/// );
/// # Ok::<(), BearerCredentialError>(())
/// ```
pub fn bearer_credential<'a>(
    values: impl IntoIterator<Item = &'a [u8]>,
) -> Result<SessionBearerToken, BearerCredentialError> {
    let mut values = values.into_iter();
    let value = values.next().ok_or(BearerCredentialError::Missing)?;
    if values.next().is_some() {
        return Err(BearerCredentialError::Invalid);
    }
    let value = std::str::from_utf8(value).map_err(|_| BearerCredentialError::Invalid)?;
    let token = value
        .strip_prefix(BEARER_SCHEME)
        .and_then(|rest| rest.strip_prefix(' '))
        .ok_or(BearerCredentialError::Invalid)?;
    SessionBearerToken::new(token).map_err(|_| BearerCredentialError::Invalid)
}

impl SessionBearerToken {
    /// A new token: 32 bytes from the operating system's CSPRNG, in the
    /// canonical encoding. Deliver it to the client once and store only
    /// [`digest`](Self::digest).
    pub fn generate() -> Result<Self, CredentialError> {
        let mut bytes = [0_u8; 32];
        getrandom::fill(&mut bytes).map_err(|_| CredentialError::RandomnessUnavailable)?;
        Self::new(hex_lower(&bytes))
    }

    /// The SHA-256 digest a server stores and looks the session up by: the
    /// digest of the canonical text, exactly what the generated
    /// `infrastructure::security::bearer_digest` computes for cookies.
    #[must_use]
    pub fn digest(&self) -> SessionCredentialDigest {
        let mut bytes = [0_u8; 32];
        bytes.copy_from_slice(&Sha256::digest(self.expose().as_bytes()));
        SessionCredentialDigest::from_bytes(bytes)
    }
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(value: &str) -> Result<SessionBearerToken, BearerCredentialError> {
        bearer_credential([value.as_bytes()])
    }

    #[test]
    fn exactly_one_canonical_bearer_header_is_accepted() {
        let token = "0f".repeat(32);
        assert_eq!(
            header(&format!("Bearer {token}")).map(|parsed| parsed.expose().to_owned()),
            Ok(token)
        );
    }

    #[test]
    fn a_missing_header_is_missing_and_everything_else_invalid() {
        assert_eq!(bearer_credential([]), Err(BearerCredentialError::Missing));
        let token = "0f".repeat(32);
        let valid = format!("Bearer {token}");
        assert_eq!(
            bearer_credential([valid.as_bytes(), valid.as_bytes()]),
            Err(BearerCredentialError::Invalid),
            "a repeated header"
        );
        for hostile in [
            String::new(),
            "Bearer".to_owned(),
            "Bearer ".to_owned(),
            format!("bearer {token}"),
            format!("Bearer  {token}"),
            format!("Bearer {token} "),
            format!(" Bearer {token}"),
            format!("Bearer\t{token}"),
            format!("Bearer {token}, Bearer {token}"),
            format!("Basic {token}"),
            format!("Bearer {}", "0F".repeat(32)),
            format!("Bearer {}", "0f".repeat(31)),
            "Bearer not-a-token".to_owned(),
        ] {
            assert_eq!(
                header(&hostile).map(|_| ()),
                Err(BearerCredentialError::Invalid),
                "{hostile:?}"
            );
        }
        assert_eq!(
            bearer_credential([b"Bearer \xff".as_slice()]).map(|_| ()),
            Err(BearerCredentialError::Invalid),
            "not UTF-8"
        );
    }

    #[test]
    fn generated_tokens_are_canonical_independent_and_digested() {
        let first = SessionBearerToken::generate().expect("OS randomness");
        let second = SessionBearerToken::generate().expect("OS randomness");
        assert_eq!(first.expose().len(), 64);
        assert_ne!(first, second);
        assert_eq!(SessionBearerToken::new(first.expose()), Ok(first.clone()));
        assert_eq!(first.digest(), first.digest());
        assert_ne!(first.digest(), second.digest());
        // SHA-256 of the canonical text, as the generated cookie mechanism.
        let known = SessionBearerToken::new("00".repeat(32)).expect("canonical");
        let expected = Sha256::digest("00".repeat(32).as_bytes());
        assert_eq!(known.digest().as_bytes().as_slice(), expected.as_slice());
    }
}
