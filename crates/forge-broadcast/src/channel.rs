//! Channel names: what a page subscribes to and a publisher sends to.

use std::fmt;

use forge_tenancy::TenantId;

use crate::BroadcastError;

/// The longest channel name, in bytes.
pub const MAX_CHANNEL_NAME_BYTES: usize = 96;

/// A stream of messages, global or owned by one tenant.
///
/// A tenant channel is a different channel from the global one of the same
/// name and from every other tenant's: a message published to it never
/// reaches another tenant's subscribers, and only a viewer authorized for
/// that tenant may subscribe (the stream handler checks it). Global channels
/// are for content every authorized viewer may see; tenant data belongs on
/// [`Channel::for_tenant`] channels.
///
/// Names are 1 to 96 bytes of `[a-z0-9_.:-]` starting with a letter or
/// digit, so a name never contains `/` and the [key](Channel::key) is
/// unambiguous.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct Channel {
    tenant: Option<TenantId>,
    name: String,
}

impl Channel {
    /// A channel not tied to a tenant.
    pub fn global(name: impl Into<String>) -> Result<Self, BroadcastError> {
        let name = valid_name(name.into())?;
        Ok(Self { tenant: None, name })
    }

    /// A channel owned by `tenant`.
    pub fn for_tenant(tenant: TenantId, name: impl Into<String>) -> Result<Self, BroadcastError> {
        let name = valid_name(name.into())?;
        Ok(Self {
            tenant: Some(tenant),
            name,
        })
    }

    /// The owning tenant, `None` for a global channel.
    #[must_use]
    pub fn tenant(&self) -> Option<TenantId> {
        self.tenant
    }

    /// The channel name, such as `products`.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Whether the channel is not tenant-scoped.
    #[must_use]
    pub fn is_global(&self) -> bool {
        self.tenant.is_none()
    }

    /// The canonical text form: `global/NAME` or `TENANT-UUID/NAME` (the same
    /// as Forge-Go's).
    #[must_use]
    pub fn key(&self) -> String {
        match self.tenant {
            Some(tenant) => format!("{tenant}/{}", self.name),
            None => format!("global/{}", self.name),
        }
    }

    /// The inverse of [`Channel::key`]. The tenant must be a UUIDv7.
    pub fn parse_key(key: &str) -> Result<Self, BroadcastError> {
        let (scope, name) = key.split_once('/').ok_or(BroadcastError::InvalidChannel)?;
        if scope == "global" {
            return Self::global(name);
        }
        let tenant = scope
            .parse::<TenantId>()
            .map_err(|_| BroadcastError::InvalidChannel)?;
        Self::for_tenant(tenant, name)
    }
}

impl fmt::Display for Channel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.key())
    }
}

fn valid_name(name: String) -> Result<String, BroadcastError> {
    let mut bytes = name.bytes();
    let valid = name.len() <= MAX_CHANNEL_NAME_BYTES
        && bytes
            .next()
            .is_some_and(|first| first.is_ascii_lowercase() || first.is_ascii_digit())
        && bytes.all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'_' | b'.' | b':' | b'-')
        });
    if valid {
        Ok(name)
    } else {
        Err(BroadcastError::InvalidChannel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_follow_the_documented_alphabet() {
        for valid in [
            "posts",
            "0",
            "a.b:c-d_e",
            &"a".repeat(MAX_CHANNEL_NAME_BYTES),
        ] {
            assert_eq!(Channel::global(valid).expect(valid).name(), valid);
        }
        for invalid in [
            "",
            "Posts",
            "_posts",
            "-posts",
            "posts/1",
            "posts 1",
            "posts\"",
            "posts<",
            "pöst",
            &"a".repeat(MAX_CHANNEL_NAME_BYTES + 1),
        ] {
            assert!(
                matches!(
                    Channel::global(invalid),
                    Err(BroadcastError::InvalidChannel)
                ),
                "{invalid:?}"
            );
            assert!(Channel::for_tenant(TenantId::new(), invalid).is_err());
        }
    }

    #[test]
    fn tenant_and_global_channels_of_one_name_are_distinct() {
        let tenant = TenantId::new();
        let other = TenantId::new();
        let global = Channel::global("posts").expect("valid");
        let mine = Channel::for_tenant(tenant, "posts").expect("valid");
        let theirs = Channel::for_tenant(other, "posts").expect("valid");

        assert_ne!(global, mine);
        assert_ne!(mine, theirs);
        assert_ne!(global.key(), mine.key());
        assert_eq!(global.key(), "global/posts");
        assert_eq!(mine.key(), format!("{tenant}/posts"));
        assert!(global.is_global() && !mine.is_global());
        assert_eq!(mine.tenant(), Some(tenant));
        assert_eq!(mine.to_string(), mine.key());
    }

    #[test]
    fn keys_round_trip_and_reject_anything_else() {
        let tenant = TenantId::new();
        for channel in [
            Channel::global("posts").expect("valid"),
            Channel::for_tenant(tenant, "a.b").expect("valid"),
        ] {
            assert_eq!(Channel::parse_key(&channel.key()).expect("parses"), channel);
        }
        for invalid in [
            "posts",
            "global/",
            "global/Posts",
            "nobody/posts",
            // A UUIDv4 is not a Forge tenant identifier.
            "6f1c3c1e-6c1b-4d0e-9a51-0c1d2b3a4f5e/posts",
            &format!("{tenant}/posts/1"),
        ] {
            assert!(
                matches!(
                    Channel::parse_key(invalid),
                    Err(BroadcastError::InvalidChannel)
                ),
                "{invalid}"
            );
        }
    }
}
