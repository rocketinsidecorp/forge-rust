//! Signed, expiring channel names: the capability a page renders and the
//! stream handler verifies.

use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use forge_config::{AppConfig, Environment};
use forge_tenancy::TenantId;
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use subtle::ConstantTimeEq;
use tracing::warn;

use crate::{BroadcastError, Channel};

/// The shortest accepted signing key, in bytes.
pub const MIN_KEY_BYTES: usize = 32;
/// How long a signed channel name stays valid by default.
pub const DEFAULT_TTL: Duration = Duration::from_secs(24 * 3600);
/// The longest allowed validity.
pub const MAX_TTL: Duration = Duration::from_secs(7 * 24 * 3600);
/// The query parameter carrying the signed channel name.
pub const STREAM_PARAMETER: &str = "stream";
/// Where applications mount [`EventStreams`](crate::EventStreams).
pub const DEFAULT_STREAMS_PATH: &str = "/streams";

const TOKEN_VERSION: &str = "v1";
const MAX_TOKEN_BYTES: usize = 512;
/// Domain separation: these MACs can never be confused with another Forge
/// signature made with the same key. Identical to Forge-Go's label, so both
/// frameworks sign and verify the same tokens.
const MAC_LABEL: &[u8] = b"forge-broadcast-stream-v1.";

type Clock = Box<dyn Fn() -> SystemTime + Send + Sync>;

/// Signs channel names with HMAC-SHA256 so that a browser can only subscribe
/// to channels the server rendered for it, like Rails' signed stream names.
///
/// A token is `v1.<claims>.<mac>`: base64url (unpadded) JSON claims
/// `{"t": tenant, "n": name, "e": expiry}` and the HMAC-SHA256 (RustCrypto)
/// of the label `forge-broadcast-stream-v1.` and `v1.<claims>`, the same
/// format as Forge-Go. Verification compares the MAC in constant time
/// (`subtle`) against every configured key, so a key can be rotated, and
/// reports an expired token as expired only when it is authentic.
///
/// A token is a bearer capability for one channel until it expires: it does
/// not prove who holds it, so the stream handler also authenticates the
/// caller and requires a tenant channel's tenant to be the caller's.
pub struct Signer {
    /// `keys[0]` signs; all verify.
    keys: Vec<Vec<u8>>,
    ttl: Duration,
    clock: Clock,
}

impl fmt::Debug for Signer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Signer")
            .field("keys", &self.keys.len())
            .field("ttl", &self.ttl)
            .finish_non_exhaustive()
    }
}

#[derive(Serialize, Deserialize)]
struct Claims {
    #[serde(rename = "t", default, skip_serializing_if = "Option::is_none")]
    tenant: Option<String>,
    #[serde(rename = "n")]
    name: String,
    #[serde(rename = "e")]
    expires: i64,
}

impl Signer {
    /// Signs with `key` and also accepts tokens signed with any of
    /// `previous` (key rotation). Every key must be at least
    /// [`MIN_KEY_BYTES`] of random data, shared by every replica.
    pub fn new(key: &[u8], previous: &[&[u8]]) -> Result<Self, BroadcastError> {
        let mut keys = Vec::with_capacity(previous.len() + 1);
        for candidate in std::iter::once(key).chain(previous.iter().copied()) {
            if candidate.len() < MIN_KEY_BYTES {
                return Err(BroadcastError::Configuration(
                    "stream signing keys must be at least 32 bytes",
                ));
            }
            keys.push(candidate.to_vec());
        }
        Ok(Self {
            keys,
            ttl: DEFAULT_TTL,
            clock: Box::new(SystemTime::now),
        })
    }

    /// A signer with a random key for this process only: streams work, but
    /// tokens stop verifying after a restart and on other replicas.
    pub fn random() -> Result<Self, BroadcastError> {
        let mut key = [0_u8; MIN_KEY_BYTES];
        getrandom::fill(&mut key).map_err(|_| BroadcastError::Random)?;
        Self::new(&key, &[])
    }

    /// The signer of a generated application, from `FORGE_STREAMS_KEY` and
    /// `FORGE_STREAMS_PREVIOUS_KEYS`.
    ///
    /// The current key signs; the previous keys (at most 3) only verify, so
    /// the key can be rotated without breaking pages that are open. Previous
    /// keys without a current key are refused. Errors never contain a key.
    ///
    /// Production refuses to start without the key, because every replica
    /// must sign stream names alike. Development and test use a random
    /// per-process key and log a warning, so live updates work without setup
    /// until the process restarts.
    pub fn from_config(config: &AppConfig) -> Result<Self, BroadcastError> {
        let key = config.streams.key_bytes().map_err(|_| {
            BroadcastError::Configuration("FORGE_STREAMS_KEY is not a valid signing key")
        })?;
        let previous = config.streams.previous_key_bytes().map_err(|_| {
            BroadcastError::Configuration(
                "FORGE_STREAMS_PREVIOUS_KEYS is not valid: at most 3 base64 keys of at least 32 \
                 bytes, comma-separated",
            )
        })?;
        let previous: Vec<&[u8]> = previous.iter().map(Vec::as_slice).collect();
        match key {
            Some(key) => Self::new(&key, &previous),
            None if !previous.is_empty() => Err(BroadcastError::Configuration(
                "FORGE_STREAMS_PREVIOUS_KEYS needs FORGE_STREAMS_KEY: the previous keys only \
                 verify, the current key signs",
            )),
            None if config.environment == Environment::Production => {
                Err(BroadcastError::Configuration(
                    "FORGE_STREAMS_KEY is required in production: set it to the output of \
                     `openssl rand -base64 32` so every replica signs stream names alike",
                ))
            }
            None => {
                warn!(
                    "FORGE_STREAMS_KEY is not set: using a random key for this process; live \
                     updates work only until a restart"
                );
                Self::random()
            }
        }
    }

    /// Sets how long tokens are valid: positive and at most [`MAX_TTL`]
    /// (default [`DEFAULT_TTL`]).
    pub fn with_ttl(mut self, ttl: Duration) -> Result<Self, BroadcastError> {
        if ttl.is_zero() || ttl > MAX_TTL {
            return Err(BroadcastError::Configuration(
                "the stream token TTL must be positive and at most 7 days",
            ));
        }
        self.ttl = ttl;
        Ok(self)
    }

    /// Replaces the clock, for tests.
    #[must_use]
    pub fn with_clock(mut self, clock: impl Fn() -> SystemTime + Send + Sync + 'static) -> Self {
        self.clock = Box::new(clock);
        self
    }

    /// A token for `channel` that expires after the signer's TTL.
    pub fn sign(&self, channel: &Channel) -> Result<String, BroadcastError> {
        let expires = unix_seconds((self.clock)() + self.ttl);
        let claims = Claims {
            tenant: channel.tenant().map(|tenant| tenant.to_string()),
            name: channel.name().to_owned(),
            expires,
        };
        let encoded = serde_json::to_vec(&claims)
            .map_err(|_| BroadcastError::Configuration("stream claims could not be encoded"))?;
        let body = format!("{TOKEN_VERSION}.{}", URL_SAFE_NO_PAD.encode(encoded));
        let signature = URL_SAFE_NO_PAD.encode(self.mac(&self.keys[0], &body)?);
        Ok(format!("{body}.{signature}"))
    }

    /// Checks the signature and expiry of `token` and returns its channel.
    ///
    /// [`BroadcastError::InvalidToken`] covers everything that is not an
    /// authentic token; [`BroadcastError::ExpiredToken`] an authentic one
    /// past its expiry. The stream handler answers both alike.
    pub fn verify(&self, token: &str) -> Result<Channel, BroadcastError> {
        if token.len() > MAX_TOKEN_BYTES {
            return Err(BroadcastError::InvalidToken);
        }
        let (body, signature) = token.rsplit_once('.').ok_or(BroadcastError::InvalidToken)?;
        let encoded_claims = body
            .strip_prefix(TOKEN_VERSION)
            .and_then(|rest| rest.strip_prefix('.'))
            .ok_or(BroadcastError::InvalidToken)?;
        let given = URL_SAFE_NO_PAD
            .decode(signature)
            .map_err(|_| BroadcastError::InvalidToken)?;
        // Every key is tried, so the time taken does not tell which one
        // (if any) matched.
        let mut valid = subtle::Choice::from(0);
        for key in &self.keys {
            valid |= self.mac(key, body)?.as_slice().ct_eq(&given);
        }
        if !bool::from(valid) {
            return Err(BroadcastError::InvalidToken);
        }
        let raw = URL_SAFE_NO_PAD
            .decode(encoded_claims)
            .map_err(|_| BroadcastError::InvalidToken)?;
        let claims: Claims =
            serde_json::from_slice(&raw).map_err(|_| BroadcastError::InvalidToken)?;
        if unix_seconds((self.clock)()) >= claims.expires {
            return Err(BroadcastError::ExpiredToken);
        }
        let channel = match claims.tenant {
            None => Channel::global(claims.name),
            Some(tenant) => {
                let tenant = tenant
                    .parse::<TenantId>()
                    .map_err(|_| BroadcastError::InvalidToken)?;
                Channel::for_tenant(tenant, claims.name)
            }
        };
        channel.map_err(|_| BroadcastError::InvalidToken)
    }

    /// The URL a `<turbo-stream-source>` connects to: `path` (where
    /// [`EventStreams`](crate::EventStreams) is mounted) with the signed
    /// channel in the `stream` query parameter.
    pub fn source_url(&self, path: &str, channel: &Channel) -> Result<String, BroadcastError> {
        let valid_path = path.starts_with('/')
            && !path.starts_with("//")
            && path.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'.' | b'_' | b'~')
            });
        if !valid_path {
            return Err(BroadcastError::Configuration(
                "the streams path must be a local path of letters, digits and / - . _ ~",
            ));
        }
        // The token is base64url and '.', which need no URL encoding.
        Ok(format!("{path}?{STREAM_PARAMETER}={}", self.sign(channel)?))
    }

    /// `<turbo-stream-source src="/streams?stream=...">` for `channel`: what
    /// a page renders (unescaped, `|safe`) to receive the channel's updates,
    /// like Rails' `turbo_stream_from`. The markup holds only the escaped
    /// URL; render it for a channel the viewer is authorized to read.
    pub fn turbo_stream_from(&self, channel: &Channel) -> Result<String, BroadcastError> {
        self.turbo_stream_from_path(DEFAULT_STREAMS_PATH, channel)
    }

    /// [`Signer::turbo_stream_from`] for streams mounted at `path`.
    pub fn turbo_stream_from_path(
        &self,
        path: &str,
        channel: &Channel,
    ) -> Result<String, BroadcastError> {
        let source = self.source_url(path, channel)?;
        Ok(format!(
            "<turbo-stream-source src=\"{}\"></turbo-stream-source>",
            escape_attribute(&source)
        ))
    }

    fn mac(&self, key: &[u8], body: &str) -> Result<Vec<u8>, BroadcastError> {
        let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(key)
            .map_err(|_| BroadcastError::Configuration("invalid stream signing key"))?;
        mac.update(MAC_LABEL);
        mac.update(body.as_bytes());
        Ok(mac.finalize().into_bytes().to_vec())
    }
}

fn unix_seconds(time: SystemTime) -> i64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(elapsed) => i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX),
        Err(before) => i64::try_from(before.duration().as_secs()).map_or(i64::MIN, |secs| -secs),
    }
}

fn escape_attribute(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    const KEY: &[u8; 32] = b"0123456789abcdef0123456789abcdef";
    const OTHER: &[u8; 32] = b"fedcba9876543210fedcba9876543210";

    fn signer() -> Signer {
        Signer::new(KEY, &[]).expect("valid key")
    }

    /// A signer whose clock is `now` seconds after the epoch.
    fn at(key: &[u8], now: &Arc<AtomicU64>) -> Signer {
        let now = Arc::clone(now);
        Signer::new(key, &[])
            .expect("valid key")
            .with_clock(move || UNIX_EPOCH + Duration::from_secs(now.load(Ordering::SeqCst)))
    }

    #[test]
    fn tokens_round_trip_for_global_and_tenant_channels() {
        let signer = signer();
        for channel in [
            Channel::global("posts").expect("valid"),
            Channel::for_tenant(TenantId::new(), "posts").expect("valid"),
        ] {
            let token = signer.sign(&channel).expect("signs");
            assert!(token.starts_with("v1."), "{token}");
            assert!(token.len() <= MAX_TOKEN_BYTES);
            assert_eq!(signer.verify(&token).expect("verifies"), channel);
        }
    }

    #[test]
    fn short_keys_and_bad_ttls_are_refused() {
        assert!(Signer::new(&[0; 31], &[]).is_err());
        assert!(Signer::new(KEY, &[&[0; 31]]).is_err());
        assert!(signer().with_ttl(Duration::ZERO).is_err());
        assert!(signer().with_ttl(MAX_TTL + Duration::from_secs(1)).is_err());
        assert!(signer().with_ttl(MAX_TTL).is_ok());
        assert!(!format!("{:?}", signer()).contains("0123456789"));
    }

    #[test]
    fn forged_and_tampered_tokens_are_invalid() {
        let signer = signer();
        let channel = Channel::for_tenant(TenantId::new(), "posts").expect("valid");
        let token = signer.sign(&channel).expect("signs");
        let foreign = Signer::new(OTHER, &[])
            .expect("valid key")
            .sign(&channel)
            .expect("signs");
        // The same claims for another tenant, re-encoded without a MAC.
        let (body, mac) = token.rsplit_once('.').expect("has a mac");
        let other_tenant = Claims {
            tenant: Some(TenantId::new().to_string()),
            name: "posts".to_owned(),
            expires: i64::MAX,
        };
        let swapped = format!(
            "v1.{}.{mac}",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&other_tenant).expect("encodes"))
        );
        let mut flipped = token.clone().into_bytes();
        let last = flipped.len() - 1;
        flipped[last] = if flipped[last] == b'A' { b'B' } else { b'A' };
        let flipped = String::from_utf8(flipped).expect("ascii");

        for forged in [
            "",
            "forged",
            "v1.",
            "v1..",
            &format!("{body}."),
            &format!("v2{}", &token[2..]),
            &format!("{token}="),
            &format!("{token}x"),
            &format!("{body}.{}", "A".repeat(43)),
            &foreign,
            &swapped,
            &flipped,
            &format!("{token}{}", "a".repeat(MAX_TOKEN_BYTES)),
        ] {
            assert!(
                matches!(signer.verify(forged), Err(BroadcastError::InvalidToken)),
                "{forged:?}"
            );
        }
    }

    #[test]
    fn tokens_expire_after_the_ttl() {
        let now = Arc::new(AtomicU64::new(1_000_000));
        let signer = at(KEY, &now)
            .with_ttl(Duration::from_secs(60))
            .expect("valid ttl");
        let channel = Channel::global("posts").expect("valid");
        let token = signer.sign(&channel).expect("signs");

        now.store(1_000_059, Ordering::SeqCst);
        assert_eq!(signer.verify(&token).expect("still valid"), channel);
        now.store(1_000_060, Ordering::SeqCst);
        assert!(matches!(
            signer.verify(&token),
            Err(BroadcastError::ExpiredToken)
        ));
        // An expired forgery is still reported as invalid, not expired.
        let forged = format!(
            "{}.{}",
            token.rsplit_once('.').expect("mac").0,
            "A".repeat(43)
        );
        assert!(matches!(
            signer.verify(&forged),
            Err(BroadcastError::InvalidToken)
        ));
    }

    #[test]
    fn previous_keys_verify_but_never_sign() {
        let channel = Channel::global("posts").expect("valid");
        let old = Signer::new(OTHER, &[]).expect("valid key");
        let rotated = Signer::new(KEY, &[OTHER]).expect("valid keys");
        let token = old.sign(&channel).expect("signs");

        assert_eq!(rotated.verify(&token).expect("old key verifies"), channel);
        let fresh = rotated.sign(&channel).expect("signs");
        assert!(matches!(
            old.verify(&fresh),
            Err(BroadcastError::InvalidToken)
        ));
        assert!(
            signer().verify(&fresh).is_ok(),
            "new tokens use the new key"
        );
    }

    /// Tokens printed by Forge-Go's `broadcast.Signer` for the same key and
    /// clock: both frameworks produce, and accept, identical tokens.
    #[test]
    fn tokens_match_forge_go_byte_for_byte() {
        const GO_GLOBAL: &str = "v1.eyJuIjoicG9zdHMiLCJlIjoxODAwMDg2NDAwfQ.\
                                 gRA7075gFuL1Z14xAQQBTroNHkruRHr52J_aBchB5-k";
        const GO_TENANT: &str = "v1.eyJ0IjoiMDE5MDAwMDAtMDAwMC03MDAwLTgwMDAtMDAwMDAwMDAwMDAxIiwibiI6\
                                 InBvc3RzIiwiZSI6MTgwMDA4NjQwMH0.\
                                 aJhTH3w2KzL4uir7YlFi7SDm1vBWBmzwt5WOyNnraVc";
        let now = Arc::new(AtomicU64::new(1_800_000_000));
        let signer = at(KEY, &now);
        let global = Channel::global("posts").expect("valid");
        let tenant: TenantId = "01900000-0000-7000-8000-000000000001"
            .parse()
            .expect("a UUIDv7");
        let scoped = Channel::for_tenant(tenant, "posts").expect("valid");

        assert_eq!(signer.sign(&global).expect("signs"), GO_GLOBAL);
        assert_eq!(signer.sign(&scoped).expect("signs"), GO_TENANT);
        assert_eq!(signer.verify(GO_GLOBAL).expect("verifies"), global);
        assert_eq!(signer.verify(GO_TENANT).expect("verifies"), scoped);
    }

    #[test]
    fn stream_sources_escape_their_url_and_validate_the_path() {
        let signer = signer();
        let channel = Channel::global("posts").expect("valid");

        let html = signer.turbo_stream_from(&channel).expect("renders");
        assert!(
            html.starts_with("<turbo-stream-source src=\"/streams?stream=v1."),
            "{html}"
        );
        assert!(html.ends_with("\"></turbo-stream-source>"), "{html}");
        let url = signer.source_url("/live", &channel).expect("url");
        assert!(url.starts_with("/live?stream=v1."), "{url}");
        let token = url.split_once("stream=").expect("param").1;
        assert_eq!(signer.verify(token).expect("verifies"), channel);
        for path in [
            "",
            "streams",
            "//evil.example",
            "/a?b",
            "/a#b",
            "/a b",
            "/\"><x",
        ] {
            assert!(signer.source_url(path, &channel).is_err(), "{path:?}");
        }
        assert_eq!(escape_attribute("a&\"'<>"), "a&amp;&quot;&#39;&lt;&gt;");
    }

    const NEW_B64: &str = "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY=";
    const OLD_B64: &str = "ZmVkY2JhOTg3NjU0MzIxMGZlZGNiYTk4NzY1NDMyMTA=";

    fn production_config(vars: &[(&'static str, &str)]) -> AppConfig {
        AppConfig::from_lookup(|name| {
            if name == "FORGE_ENV" {
                return Some("production".to_owned());
            }
            vars.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_owned())
        })
        .expect("valid configuration")
    }

    /// The rotation of the generated application: a name signed with the old
    /// key still verifies once the old key moves to the previous keys, and
    /// every new name is signed with the new key.
    #[test]
    fn from_config_rotates_the_key_without_breaking_open_pages() {
        let channel = Channel::for_tenant(TenantId::new(), "posts").expect("valid");
        let before = Signer::from_config(&production_config(&[("FORGE_STREAMS_KEY", OLD_B64)]))
            .expect("old key");
        let token = before.sign(&channel).expect("signs");

        let new_only = Signer::from_config(&production_config(&[("FORGE_STREAMS_KEY", NEW_B64)]))
            .expect("new key");
        assert!(matches!(
            new_only.verify(&token),
            Err(BroadcastError::InvalidToken)
        ));

        let rotated = Signer::from_config(&production_config(&[
            ("FORGE_STREAMS_KEY", NEW_B64),
            ("FORGE_STREAMS_PREVIOUS_KEYS", OLD_B64),
        ]))
        .expect("rotation");
        assert_eq!(rotated.verify(&token).expect("open page"), channel);
        let fresh = rotated.sign(&channel).expect("signs");
        assert_eq!(new_only.verify(&fresh).expect("new key signed"), channel);
        assert!(
            matches!(before.verify(&fresh), Err(BroadcastError::InvalidToken)),
            "the previous key never signs"
        );
    }

    #[test]
    fn previous_keys_without_a_current_key_are_refused_without_echoing_a_key() {
        for environment in ["production", "development"] {
            let config = AppConfig::from_lookup(|name| match name {
                "FORGE_ENV" => Some(environment.to_owned()),
                "FORGE_STREAMS_PREVIOUS_KEYS" => Some(OLD_B64.to_owned()),
                _ => None,
            })
            .expect("valid configuration");
            let error = Signer::from_config(&config).expect_err("needs the current key");
            let text = error.to_string();
            assert!(text.contains("FORGE_STREAMS_PREVIOUS_KEYS needs"), "{text}");
            assert!(!text.contains(OLD_B64), "{text}");
        }
        // A hand-built configuration with an invalid list is refused as well.
        let mut config = production_config(&[("FORGE_STREAMS_KEY", NEW_B64)]);
        config.streams.previous_keys = vec![forge_config::SecretString::new("not-a-key!"); 4];
        let error = Signer::from_config(&config).expect_err("invalid list");
        assert!(!error.to_string().contains("not-a-key"), "{error}");
    }

    #[test]
    fn from_config_requires_a_key_in_production_only() {
        let mut config = AppConfig {
            environment: Environment::Production,
            ..AppConfig::default()
        };
        assert!(matches!(
            Signer::from_config(&config),
            Err(BroadcastError::Configuration(message)) if message.contains("FORGE_STREAMS_KEY")
        ));
        config.environment = Environment::Development;
        assert!(Signer::from_config(&config).is_ok());
        config.environment = Environment::Test;
        assert!(Signer::from_config(&config).is_ok());

        let key = "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY=";
        let configured = AppConfig::from_lookup(|name| match name {
            "FORGE_ENV" => Some("production".to_owned()),
            "FORGE_STREAMS_KEY" => Some(key.to_owned()),
            _ => None,
        })
        .expect("valid configuration");
        let production = Signer::from_config(&configured).expect("configured key");
        // The configured key is KEY: tokens of either verify on the other.
        let channel = Channel::global("posts").expect("valid");
        let token = production.sign(&channel).expect("signs");
        assert_eq!(signer().verify(&token).expect("same key"), channel);
    }
}
