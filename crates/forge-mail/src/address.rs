//! Mailboxes: an optional display name and an ASCII addr-spec.

use std::{fmt, str::FromStr};

use forge_config::{MailboxError, parse_mailbox, validate_mailbox};
use serde::{Deserialize, Serialize};

use crate::{MailError, encoding};

/// One mailbox: an optional display name and an ASCII addr-spec.
///
/// Forge does not implement SMTPUTF8 (RFC 6531) or IDNA: the address must be
/// ASCII dot-atom `local@domain`, while the display name may be any text and
/// is RFC 2047 encoded. Quoted local parts, comments, route addresses and
/// domain literals are rejected, which removes the ambiguity attackers use to
/// smuggle extra recipients or SMTP commands. The rules are defined once, in
/// [`forge_config::validate_mailbox`], and shared with `FORGE_MAIL_FROM`
/// validation.
///
/// ```
/// use forge_mail::Address;
///
/// let address: Address = "Ada Lovelace <ada@example.com>".parse()?;
/// assert_eq!(address.email, "ada@example.com");
/// assert_eq!(address.to_string(), "\"Ada Lovelace\" <ada@example.com>");
/// assert!(Address::new("", "ada@example.com\r\nBcc: eve@example.com").is_err());
/// # Ok::<(), forge_mail::MailError>(())
/// ```
#[derive(Clone, Debug, Default, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct Address {
    /// Display name; empty for none.
    pub name: String,
    /// The addr-spec, `local@domain`.
    pub email: String,
}

impl Address {
    /// Validates and returns an address.
    pub fn new(name: impl Into<String>, email: impl Into<String>) -> Result<Self, MailError> {
        let address = Self {
            name: name.into(),
            email: email.into(),
        };
        address.validate()?;
        Ok(address)
    }

    /// Parses `user@example.com` or `Display Name <user@example.com>`.
    pub fn parse(value: &str) -> Result<Self, MailError> {
        let (name, email) =
            parse_mailbox(value).map_err(|error| mailbox_error("address", error))?;
        Ok(Self { name, email })
    }

    /// Why the address cannot be used in a message, if it cannot.
    pub fn validate(&self) -> Result<(), MailError> {
        self.validate_field("address")
    }

    pub(crate) fn validate_field(&self, field: &str) -> Result<(), MailError> {
        validate_mailbox(&self.name, &self.email).map_err(|error| mailbox_error(field, error))
    }

    /// The part after the `@`.
    #[must_use]
    pub fn domain(&self) -> &str {
        self.email.split_once('@').map_or("", |(_, domain)| domain)
    }
}

fn mailbox_error(field: &str, error: MailboxError) -> MailError {
    if error.unsupported {
        MailError::AddressUnsupported {
            field: field.to_owned(),
            reason: error.reason,
        }
    } else {
        MailError::invalid(field, error.reason)
    }
}

impl FromStr for Address {
    type Err = MailError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

/// The header form: the bare addr-spec without a name, otherwise the name
/// (quoted or RFC 2047 encoded) followed by `<email>`. Meaningful only for a
/// valid address.
impl fmt::Display for Address {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.name.is_empty() {
            return formatter.write_str(&self.email);
        }
        let name = if self.name.contains("=?") {
            // Plain text that looks like an encoded word could be decoded by
            // a client into something other than what was written.
            encoding::encode_text(&self.name)
        } else {
            encoding::display_name(&self.name)
        };
        write!(formatter, "{name} <{}>", self.email)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_name_the_field_never_the_value() {
        let error = Address::new("", "victim@example.com\r\nRCPT TO:<eve@example.com>")
            .expect_err("injection");
        assert_eq!(error.code(), "mail_invalid");
        assert!(!error.to_string().contains("victim"), "{error}");
        let error = Address::new("", "joão@example.com").expect_err("unsupported");
        assert_eq!(error.code(), "mail_address_unsupported");
        assert!(!error.to_string().contains("jo"), "{error}");
    }

    #[test]
    fn header_forms() {
        let plain = Address::new("", "ada@example.com").expect("valid");
        assert_eq!(plain.to_string(), "ada@example.com");
        assert_eq!(plain.domain(), "example.com");
        let tricky = Address::new("=?utf-8?b?QWRtaW4=?=", "ada@example.com").expect("valid");
        assert!(tricky.to_string().starts_with("=?utf-8?b?PT91"), "{tricky}");
        for name in ["Ada\u{202E}gpj.exe", "Ada\u{2028}", "Ada\u{0}", "Ada\u{85}"] {
            assert!(Address::new(name, "ada@example.com").is_err(), "{name:?}");
        }
    }
}
