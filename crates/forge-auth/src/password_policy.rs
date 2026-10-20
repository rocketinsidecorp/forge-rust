//! The rule for passwords people choose: at `setup` and on the reset page.
//!
//! Sign-in itself accepts any non-empty password the hasher accepts, so
//! credentials created before a policy change keep working.

use thiserror::Error;

/// Fewest characters (Unicode scalar values) a new password may have.
pub const MIN_PASSWORD_CHARS: usize = 12;

/// Most bytes a password may have; the Argon2id adapter's input bound.
pub const MAX_PASSWORD_BYTES: usize = 1024;

/// A password that satisfies the policy. It is never trimmed or normalized,
/// and its `Debug` output is redacted.
#[derive(Clone, Eq, PartialEq)]
pub struct NewPassword(String);

impl NewPassword {
    /// Checks `value` against the policy: at least [`MIN_PASSWORD_CHARS`]
    /// characters, at most [`MAX_PASSWORD_BYTES`] bytes, no control
    /// characters.
    pub fn new(value: impl Into<String>) -> Result<Self, PasswordPolicyError> {
        let value = value.into();
        if value.len() > MAX_PASSWORD_BYTES {
            return Err(PasswordPolicyError::TooLong);
        }
        if value.chars().any(char::is_control) {
            return Err(PasswordPolicyError::ControlCharacter);
        }
        if value.chars().count() < MIN_PASSWORD_CHARS {
            return Err(PasswordPolicyError::TooShort);
        }
        Ok(Self(value))
    }

    /// The password bytes, for the hasher only.
    #[must_use]
    pub fn expose(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

impl std::fmt::Debug for NewPassword {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("NewPassword([REDACTED])")
    }
}

/// Why a new password was refused. The messages are safe to show.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum PasswordPolicyError {
    /// Fewer than [`MIN_PASSWORD_CHARS`] characters.
    #[error("must be at least 12 characters")]
    TooShort,
    /// More than [`MAX_PASSWORD_BYTES`] bytes.
    #[error("must be at most 1024 bytes")]
    TooLong,
    /// Contains a control character (such as a newline).
    #[error("must not contain control characters")]
    ControlCharacter,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_policy_counts_characters_and_bounds_bytes() {
        assert_eq!(
            NewPassword::new("short pass1"),
            Err(PasswordPolicyError::TooShort)
        );
        assert!(NewPassword::new("twelve chars").is_ok());
        // Twelve characters, more bytes: accepted.
        assert!(NewPassword::new("ççççççççççç1").is_ok());
        assert_eq!(
            NewPassword::new("a".repeat(MAX_PASSWORD_BYTES + 1)),
            Err(PasswordPolicyError::TooLong)
        );
        assert_eq!(
            NewPassword::new("line one is\nlong"),
            Err(PasswordPolicyError::ControlCharacter)
        );
        let password = NewPassword::new("  spaces kept  ").expect("valid");
        assert_eq!(password.expose(), b"  spaces kept  ");
        assert_eq!(format!("{password:?}"), "NewPassword([REDACTED])");
    }

    #[test]
    fn messages_match_the_constants() {
        assert!(
            PasswordPolicyError::TooShort
                .to_string()
                .contains(&MIN_PASSWORD_CHARS.to_string())
        );
        assert!(
            PasswordPolicyError::TooLong
                .to_string()
                .contains(&MAX_PASSWORD_BYTES.to_string())
        );
    }
}
