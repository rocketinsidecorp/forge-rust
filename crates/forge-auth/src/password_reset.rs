//! One-time password reset tokens and their delivery port.
//!
//! A reset token is 256 bits from the operating-system CSPRNG (`getrandom`)
//! in the canonical 64-character lowercase hex encoding. Only its SHA-256
//! [`PasswordResetTokenDigest`] is persisted, so a database disclosure does
//! not reveal a usable link. The raw token exists only in the
//! [`PasswordResetDelivery`] handed to a [`PasswordResetNotifier`] and in the
//! link the user follows; its `Debug` output is redacted and Forge never
//! logs it.
//!
//! Applications without a delivery mechanism use
//! [`UndeliverablePasswordResets`], which fails closed: it never reports a
//! delivery it did not make. The page that requests a reset answers the same
//! whether or not an account exists or a message was delivered.

use async_trait::async_trait;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{CredentialError, UnixTimestamp, validate_secret_token};

/// How long a reset link stays valid: 15 minutes.
pub const PASSWORD_RESET_LIFETIME_SECS: u64 = 15 * 60;

/// The path of the page a reset link opens; the token is its `token` query
/// parameter.
pub const PASSWORD_RESET_PATH: &str = "/passwords/edit";

/// Raw one-time password reset token.
#[derive(Clone, Eq, PartialEq)]
pub struct PasswordResetToken(String);

impl PasswordResetToken {
    /// Generates a new token from the operating-system CSPRNG.
    pub fn generate() -> Result<Self, CredentialError> {
        let mut bytes = [0_u8; 32];
        getrandom::fill(&mut bytes).map_err(|_| CredentialError::RandomnessUnavailable)?;
        let mut encoded = String::with_capacity(64);
        for byte in bytes {
            encoded.push(hex_digit(byte >> 4));
            encoded.push(hex_digit(byte & 0x0f));
        }
        Self::new(encoded)
    }

    /// Accepts only the canonical encoding (64 lowercase hex characters).
    pub fn new(value: impl Into<String>) -> Result<Self, CredentialError> {
        validate_secret_token(value.into()).map(Self)
    }

    /// The SHA-256 digest that persistence stores and looks tokens up by.
    #[must_use]
    pub fn digest(&self) -> PasswordResetTokenDigest {
        PasswordResetTokenDigest(Sha256::digest(self.0.as_bytes()).into())
    }

    /// Exposes the token only to the link sent to the user.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for PasswordResetToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("PasswordResetToken([REDACTED])")
    }
}

fn hex_digit(nibble: u8) -> char {
    char::from(b"0123456789abcdef"[usize::from(nibble & 0x0f)])
}

/// SHA-256 digest of a [`PasswordResetToken`].
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct PasswordResetTokenDigest([u8; 32]);

impl PasswordResetTokenDigest {
    /// Wraps persisted digest bytes.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Digest bytes for persistence/query binding.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl std::fmt::Debug for PasswordResetTokenDigest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("PasswordResetTokenDigest([REDACTED])")
    }
}

/// A reset link to deliver to the owner of `login`.
pub struct PasswordResetDelivery {
    login: String,
    token: PasswordResetToken,
    expires_at: UnixTimestamp,
}

impl PasswordResetDelivery {
    /// Creates the delivery of `token` to the account named `login`.
    #[must_use]
    pub fn new(
        login: impl Into<String>,
        token: PasswordResetToken,
        expires_at: UnixTimestamp,
    ) -> Self {
        Self {
            login: login.into(),
            token,
            expires_at,
        }
    }

    /// The login (email address) of the account.
    #[must_use]
    pub fn login(&self) -> &str {
        &self.login
    }

    /// The raw token. Deliver it only to the account's verified address.
    #[must_use]
    pub fn token(&self) -> &PasswordResetToken {
        &self.token
    }

    /// The site-relative link, `/passwords/edit?token=<token>`; prefix it
    /// with the application's public origin.
    #[must_use]
    pub fn reset_path(&self) -> String {
        format!("{PASSWORD_RESET_PATH}?token={}", self.token.expose())
    }

    /// When the link stops working.
    #[must_use]
    pub const fn expires_at(&self) -> UnixTimestamp {
        self.expires_at
    }
}

impl std::fmt::Debug for PasswordResetDelivery {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PasswordResetDelivery")
            .field("login", &"[REDACTED]")
            .field("token", &self.token)
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// Why a reset link was not delivered. Never shown to the visitor.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum PasswordResetDeliveryError {
    /// The application has no delivery mechanism configured.
    #[error("password reset delivery is not configured")]
    NotConfigured,
    /// The delivery mechanism failed.
    #[error("password reset delivery unavailable")]
    Unavailable,
}

/// Delivers password reset links (for example by email).
///
/// Implementations must deliver only to the account's own verified address,
/// must not log the token, and must return an error rather than `Ok` when
/// nothing was delivered.
#[async_trait]
pub trait PasswordResetNotifier: Send + Sync {
    /// Delivers one reset link.
    async fn deliver(
        &self,
        delivery: &PasswordResetDelivery,
    ) -> Result<(), PasswordResetDeliveryError>;
}

/// Fail-closed [`PasswordResetNotifier`] for applications without a mailer:
/// it delivers nothing and always reports
/// [`PasswordResetDeliveryError::NotConfigured`].
#[derive(Clone, Copy, Debug, Default)]
pub struct UndeliverablePasswordResets;

#[async_trait]
impl PasswordResetNotifier for UndeliverablePasswordResets {
    async fn deliver(
        &self,
        _delivery: &PasswordResetDelivery,
    ) -> Result<(), PasswordResetDeliveryError> {
        Err(PasswordResetDeliveryError::NotConfigured)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_tokens_are_canonical_unique_and_digested() {
        let first = PasswordResetToken::generate().expect("randomness");
        let second = PasswordResetToken::generate().expect("randomness");
        assert_ne!(first, second);
        assert_eq!(first.expose().len(), 64);
        assert_eq!(
            PasswordResetToken::new(first.expose()).expect("canonical"),
            first
        );
        assert_eq!(first.digest(), first.clone().digest());
        assert_ne!(first.digest(), second.digest());
        assert_eq!(
            PasswordResetToken::new("ab".repeat(32))
                .expect("canonical")
                .digest()
                .as_bytes()[..4],
            [0x27, 0x1a, 0x41, 0x3b]
        );
    }

    #[test]
    fn malformed_tokens_are_refused() {
        for invalid in [
            "",
            "abc",
            &"AB".repeat(32),
            &"zz".repeat(32),
            &"a".repeat(65),
        ] {
            assert!(PasswordResetToken::new(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn tokens_never_appear_in_debug_output() {
        let token = PasswordResetToken::new("ab".repeat(32)).expect("canonical");
        let delivery = PasswordResetDelivery::new(
            "ada@example.com",
            token.clone(),
            UnixTimestamp::from_secs(900),
        );
        for debug in [
            format!("{token:?}"),
            format!("{:?}", token.digest()),
            format!("{delivery:?}"),
        ] {
            assert!(!debug.contains("abab"), "{debug}");
            assert!(!debug.contains("ada@"), "{debug}");
        }
        assert_eq!(
            delivery.reset_path(),
            format!("/passwords/edit?token={}", "ab".repeat(32))
        );
        assert_eq!(delivery.login(), "ada@example.com");
    }

    #[tokio::test]
    async fn the_default_notifier_never_claims_delivery() {
        let delivery = PasswordResetDelivery::new(
            "ada@example.com",
            PasswordResetToken::generate().expect("randomness"),
            UnixTimestamp::from_secs(900),
        );
        assert_eq!(
            UndeliverablePasswordResets.deliver(&delivery).await,
            Err(PasswordResetDeliveryError::NotConfigured)
        );
    }
}
