use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use forge_config::StorageConfig;
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;

use crate::{StorageError, StorageKey};

/// The shortest signing key [`Signer::new`] accepts: an HMAC-SHA256 key should
/// be as long as the digest.
pub const MIN_KEY_BYTES: usize = 32;
/// The longest lifetime a [`Signer`] issues.
pub const MAX_LINK_TTL: Duration = Duration::from_secs(7 * 24 * 3600);
const DEFAULT_LINK_TTL: Duration = Duration::from_secs(15 * 60);
const MAC_CONTEXT: &[u8] = b"forge.storage.url.v1\n";
/// Length of an unpadded base64url SHA-256 signature.
const SIGNATURE_CHARS: usize = 43;

type Clock = Box<dyn Fn() -> SystemTime + Send + Sync>;

/// Issues and verifies time-limited links to stored blobs.
///
/// A link is a capability: whoever holds it can read that one key until it
/// expires, so keep lifetimes short and serve links over TLS. Bind a link to a
/// principal ([`SignOptions::binding`]) when it must not be forwarded.
///
/// The signature is HMAC-SHA256 (RustCrypto) over the key, the expiry, the
/// disposition and the binding, compared in constant time (`subtle`) against
/// **every** configured key. Verification never reveals which part was wrong,
/// and an expired link is reported as expired only when it is authentic.
pub struct Signer {
    /// `keys[0]` signs; all verify, so a key can be rotated.
    keys: Vec<Vec<u8>>,
    clock: Clock,
}

impl std::fmt::Debug for Signer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Signer")
            .field("keys", &self.keys.len())
            .finish_non_exhaustive()
    }
}

/// How to sign one link.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SignOptions {
    ttl: Option<Duration>,
    inline: bool,
    binding: Option<String>,
}

impl SignOptions {
    /// Lifetime, at most [`MAX_LINK_TTL`] (default 15 minutes).
    #[must_use]
    pub const fn ttl(mut self, ttl: Duration) -> Self {
        self.ttl = Some(ttl);
        self
    }

    /// Asks to display the file in the browser. It only takes effect for
    /// content types that are safe to display (raster images); everything else
    /// is always downloaded.
    #[must_use]
    pub const fn inline(mut self, inline: bool) -> Self {
        self.inline = inline;
        self
    }

    /// Ties the link to a value the request must also present, such as the
    /// session or user ID: the handler recomputes it with
    /// [`HandlerOptions::binding`](crate::HandlerOptions::binding) and a link
    /// forwarded to someone else stops working.
    #[must_use]
    pub fn binding(mut self, binding: impl Into<String>) -> Self {
        self.binding = Some(binding.into());
        self
    }
}

/// A signed capability for one key.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Link {
    /// When the link stops working (whole seconds).
    pub expires: SystemTime,
    /// Unpadded base64url HMAC-SHA256.
    pub signature: String,
    /// Whether inline display was requested.
    pub inline: bool,
}

impl Link {
    /// The query string of the link (`expires=<unix>&sig=<sig>[&disposition=inline]`).
    #[must_use]
    pub fn query(&self) -> String {
        let expires = self
            .expires
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs());
        let mut query = format!("expires={expires}&sig={}", self.signature);
        if self.inline {
            query.push_str("&disposition=inline");
        }
        query
    }
}

impl Signer {
    /// Signs with `key` and also accepts links signed with any of `previous`
    /// (key rotation). Every key must be at least [`MIN_KEY_BYTES`] long and
    /// come from a secure random source.
    pub fn new(key: &[u8], previous: &[&[u8]]) -> Result<Self, StorageError> {
        let mut keys = Vec::with_capacity(previous.len() + 1);
        for candidate in std::iter::once(key).chain(previous.iter().copied()) {
            if candidate.len() < MIN_KEY_BYTES {
                return Err(StorageError::Configuration(
                    "storage signing key must be at least 32 bytes",
                ));
            }
            keys.push(candidate.to_vec());
        }
        Ok(Self {
            keys,
            clock: Box::new(SystemTime::now),
        })
    }

    /// A signer for `FORGE_STORAGE_SIGNING_KEY`. It fails when the key is not
    /// configured: Forge never signs with a default key.
    pub fn from_config(config: &StorageConfig) -> Result<Self, StorageError> {
        let key = config.signing_key_bytes().map_err(|_| {
            StorageError::Configuration("FORGE_STORAGE_SIGNING_KEY is not a valid signing key")
        })?;
        match key {
            Some(key) => Self::new(&key, &[]),
            None => Err(StorageError::Configuration(
                "FORGE_STORAGE_SIGNING_KEY is required to sign storage links",
            )),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    pub(crate) fn now(&self) -> SystemTime {
        (self.clock)()
    }

    /// Signs a link to `key`.
    pub fn sign(&self, key: &StorageKey, options: &SignOptions) -> Result<Link, StorageError> {
        let ttl = options.ttl.unwrap_or(DEFAULT_LINK_TTL);
        if ttl < Duration::from_secs(1) || ttl > MAX_LINK_TTL {
            return Err(StorageError::Configuration(
                "storage link ttl must be between 1s and 7 days",
            ));
        }
        // Round up to a whole second: a link never lives shorter than its ttl.
        let now = self.now().duration_since(UNIX_EPOCH).unwrap_or_default();
        let expires = now.as_secs() + ttl.as_secs() + 1;
        let binding = options.binding.as_deref().unwrap_or("");
        let mac = self.mac(
            &self.keys[0],
            key.as_str(),
            expires,
            options.inline,
            binding,
        )?;
        Ok(Link {
            expires: UNIX_EPOCH + Duration::from_secs(expires),
            signature: URL_SAFE_NO_PAD.encode(mac),
            inline: options.inline,
        })
    }

    /// A signed URL for `key` below `prefix`: a local path such as `/files`
    /// (the mount of the [`StorageHandler`](crate::StorageHandler), ending
    /// before the key) or an absolute `http(s)` URL.
    pub fn url(
        &self,
        prefix: &str,
        key: &StorageKey,
        options: &SignOptions,
    ) -> Result<String, StorageError> {
        let local = prefix.starts_with('/') && !prefix.starts_with("//");
        let absolute = prefix.starts_with("https://") || prefix.starts_with("http://");
        if (!local && !absolute) || prefix.contains(['?', '#', '\\', ' ']) {
            return Err(StorageError::Configuration(
                "storage link prefix must be a local path or an http(s) URL without query",
            ));
        }
        let link = self.sign(key, options)?;
        Ok(format!(
            "{}/{}?{}",
            prefix.trim_end_matches('/'),
            key,
            link.query()
        ))
    }

    /// Checks the raw query string of a request for `key` and `binding`.
    ///
    /// Returns whether the signed disposition asks for inline display. Fails
    /// with [`StorageError::InvalidLink`] for a missing, malformed, duplicated
    /// or forged parameter, a different key or a different binding, and with
    /// [`StorageError::ExpiredLink`] only for an *authentic* link whose time ran
    /// out. Parameters must be exactly as [`Link::query`] writes them
    /// (no percent-encoding): there is no second spelling of a valid link.
    pub fn verify(
        &self,
        key: &StorageKey,
        query: Option<&str>,
        binding: &str,
    ) -> Result<bool, StorageError> {
        let parsed = parse_link(query).ok_or(StorageError::InvalidLink)?;
        let mut valid = subtle::Choice::from(0);
        for candidate in &self.keys {
            let expected = self
                .mac(
                    candidate,
                    key.as_str(),
                    parsed.expires,
                    parsed.inline,
                    binding,
                )
                .map_err(|_| StorageError::InvalidLink)?;
            // Every key is checked, whatever the earlier ones said.
            valid |= expected.as_slice().ct_eq(&parsed.signature);
        }
        if !bool::from(valid) {
            return Err(StorageError::InvalidLink);
        }
        let now = self.now().duration_since(UNIX_EPOCH).unwrap_or_default();
        if now.as_secs() >= parsed.expires {
            return Err(StorageError::ExpiredLink);
        }
        Ok(parsed.inline)
    }

    /// Seconds until the link in `query` expires (zero when it is unparsable or
    /// past); used for cache headers after verification succeeded.
    pub(crate) fn remaining(&self, query: Option<&str>) -> Duration {
        let Some(parsed) = parse_link(query) else {
            return Duration::ZERO;
        };
        let now = self.now().duration_since(UNIX_EPOCH).unwrap_or_default();
        Duration::from_secs(parsed.expires.saturating_sub(now.as_secs()))
    }

    fn mac(
        &self,
        secret: &[u8],
        blob_key: &str,
        expires: u64,
        inline: bool,
        binding: &str,
    ) -> Result<Vec<u8>, StorageError> {
        let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(secret)
            .map_err(|_| StorageError::Configuration("storage signing key is unusable"))?;
        let disposition = if inline { "inline" } else { "attachment" };
        // Every field before the binding is free of newlines (a validated key, a
        // decimal, a fixed word), so the encoding is unambiguous.
        mac.update(MAC_CONTEXT);
        mac.update(blob_key.as_bytes());
        mac.update(b"\n");
        mac.update(expires.to_string().as_bytes());
        mac.update(b"\n");
        mac.update(disposition.as_bytes());
        mac.update(b"\n");
        mac.update(binding.as_bytes());
        Ok(mac.finalize().into_bytes().to_vec())
    }
}

struct ParsedLink {
    expires: u64,
    signature: Vec<u8>,
    inline: bool,
}

/// Strict parsing: exactly one `expires` (decimal, at most 12 digits), exactly
/// one `sig` (canonical 43-character unpadded base64url), at most one
/// `disposition` (`inline` or `attachment`). Other parameters are ignored.
fn parse_link(query: Option<&str>) -> Option<ParsedLink> {
    let (mut expires, mut signature, mut disposition) = (None, None, None);
    let (mut expires_seen, mut signature_seen, mut disposition_seen) = (0, 0, 0);
    for pair in query?.split('&') {
        let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
        match name {
            "expires" => {
                expires_seen += 1;
                expires = Some(value);
            }
            "sig" => {
                signature_seen += 1;
                signature = Some(value);
            }
            "disposition" => {
                disposition_seen += 1;
                disposition = Some(value);
            }
            _ => {}
        }
    }
    if expires_seen != 1 || signature_seen != 1 || disposition_seen > 1 {
        return None;
    }
    let expires = expires?;
    if expires.is_empty()
        || expires.len() > 12
        || !expires.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let signature = signature?;
    if signature.len() != SIGNATURE_CHARS {
        return None;
    }
    let inline = match disposition {
        None | Some("attachment") => false,
        Some("inline") => true,
        Some(_) => return None,
    };
    Some(ParsedLink {
        expires: expires.parse().ok()?,
        signature: URL_SAFE_NO_PAD.decode(signature).ok()?,
        inline,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };

    use super::*;

    pub(crate) const KEY_A: [u8; 32] = [7; 32];
    pub(crate) const KEY_B: [u8; 32] = [9; 32];
    pub(crate) const T0: u64 = 1_800_000_000;

    #[derive(Clone)]
    pub(crate) struct TestClock(Arc<AtomicU64>);

    impl TestClock {
        pub(crate) fn at(seconds: u64) -> Self {
            Self(Arc::new(AtomicU64::new(seconds)))
        }

        pub(crate) fn advance(&self, seconds: u64) {
            self.0.fetch_add(seconds, Ordering::SeqCst);
        }

        pub(crate) fn boxed(&self) -> Clock {
            let seconds = Arc::clone(&self.0);
            Box::new(move || UNIX_EPOCH + Duration::from_secs(seconds.load(Ordering::SeqCst)))
        }
    }

    pub(crate) fn signer(clock: &TestClock) -> Signer {
        Signer::new(&KEY_A, &[])
            .expect("signer")
            .with_clock(clock.boxed())
    }

    fn key(text: &str) -> StorageKey {
        StorageKey::parse(text).expect("key")
    }

    fn signed_query(signer: &Signer, key: &StorageKey, options: &SignOptions) -> String {
        signer.sign(key, options).expect("sign").query()
    }

    #[test]
    fn a_link_round_trips_and_carries_its_disposition() {
        let clock = TestClock::at(T0);
        let signer = signer(&clock);
        let k = key("avatars/a.png");
        let attachment = signed_query(&signer, &k, &SignOptions::default());
        assert_eq!(
            signer.verify(&k, Some(&attachment), "").expect("valid"),
            false
        );
        let inline = signed_query(&signer, &k, &SignOptions::default().inline(true));
        assert!(inline.ends_with("&disposition=inline"));
        assert_eq!(signer.verify(&k, Some(&inline), "").expect("valid"), true);
        // An explicit `disposition=attachment` is the same as none.
        let explicit = format!("{attachment}&disposition=attachment");
        assert_eq!(
            signer.verify(&k, Some(&explicit), "").expect("valid"),
            false
        );
    }

    #[test]
    fn every_tampering_is_rejected_as_invalid() {
        let clock = TestClock::at(T0);
        let signer = signer(&clock);
        let k = key("secret/report.pdf");
        let good = signed_query(&signer, &k, &SignOptions::default());
        let (expires_part, rest) = good.split_once('&').expect("parts");
        let signature = rest.strip_prefix("sig=").expect("sig");
        let expires: u64 = expires_part
            .strip_prefix("expires=")
            .expect("expires")
            .parse()
            .expect("number");

        let flipped = {
            let mut bytes = signature.as_bytes().to_vec();
            bytes[0] = if bytes[0] == b'A' { b'B' } else { b'A' };
            String::from_utf8(bytes).expect("ascii")
        };
        let other_key = key("secret/other.pdf");
        let cases: Vec<(&str, StorageKey, Option<String>)> = vec![
            ("other key", other_key, Some(good.clone())),
            (
                "extended expiry",
                k.clone(),
                Some(format!("expires={}&sig={signature}", expires + 3600)),
            ),
            (
                "flipped signature",
                k.clone(),
                Some(format!("{expires_part}&sig={flipped}")),
            ),
            (
                "added inline",
                k.clone(),
                Some(format!("{good}&disposition=inline")),
            ),
            (
                "duplicate expires",
                k.clone(),
                Some(format!("{good}&expires={expires}")),
            ),
            (
                "duplicate sig",
                k.clone(),
                Some(format!("{good}&sig={signature}")),
            ),
            (
                "duplicate disposition",
                k.clone(),
                Some(format!(
                    "{good}&disposition=attachment&disposition=attachment"
                )),
            ),
            ("missing sig", k.clone(), Some(expires_part.to_owned())),
            (
                "missing expires",
                k.clone(),
                Some(format!("sig={signature}")),
            ),
            ("no query", k.clone(), None),
            ("empty query", k.clone(), Some(String::new())),
            (
                "signed for expiry zero",
                k.clone(),
                Some(format!("expires=0&sig={signature}")),
            ),
            (
                "non-decimal expiry",
                k.clone(),
                Some(format!("expires=+{expires}&sig={signature}")),
            ),
            (
                "hex expiry",
                k.clone(),
                Some(format!("expires=0x{expires:x}&sig={signature}")),
            ),
            (
                "padded signature",
                k.clone(),
                Some(format!("{expires_part}&sig={signature}=")),
            ),
            (
                "short signature",
                k.clone(),
                Some(format!("{expires_part}&sig={}", &signature[..42])),
            ),
            (
                "percent-encoded signature",
                k.clone(),
                Some(format!("{expires_part}&sig={}%41", &signature[..42])),
            ),
            (
                "unknown disposition",
                k.clone(),
                Some(format!("{good}&disposition=weird")),
            ),
            (
                "empty signature",
                k.clone(),
                Some(format!("{expires_part}&sig=")),
            ),
        ];
        for (name, key, query) in cases {
            let result = signer.verify(&key, query.as_deref(), "");
            assert!(
                matches!(result, Err(StorageError::InvalidLink)),
                "{name}: {result:?}"
            );
        }
    }

    #[test]
    fn expiry_is_only_reported_for_authentic_links() {
        let clock = TestClock::at(T0);
        let signer = signer(&clock);
        let k = key("a/b");
        let good = signed_query(
            &signer,
            &k,
            &SignOptions::default().ttl(Duration::from_secs(60)),
        );
        assert!(signer.verify(&k, Some(&good), "").is_ok());
        clock.advance(59);
        assert!(
            signer.verify(&k, Some(&good), "").is_ok(),
            "a link never lives shorter than its ttl"
        );
        clock.advance(2);
        assert!(matches!(
            signer.verify(&k, Some(&good), ""),
            Err(StorageError::ExpiredLink)
        ));
        // A forged link that merely claims a past expiry learns nothing.
        let forged = format!("expires={T0}&sig={}", "A".repeat(43));
        assert!(matches!(
            signer.verify(&k, Some(&forged), ""),
            Err(StorageError::InvalidLink)
        ));
    }

    #[test]
    fn binding_ties_a_link_to_a_principal() {
        let clock = TestClock::at(T0);
        let signer = signer(&clock);
        let k = key("docs/x.pdf");
        let query = signed_query(&signer, &k, &SignOptions::default().binding("user-1"));
        assert!(signer.verify(&k, Some(&query), "user-1").is_ok());
        for other in ["user-2", "", "user-1 ", "USER-1"] {
            assert!(
                matches!(
                    signer.verify(&k, Some(&query), other),
                    Err(StorageError::InvalidLink)
                ),
                "binding {other:?}"
            );
        }
        // A link signed without a binding does not verify under one either.
        let unbound = signed_query(&signer, &k, &SignOptions::default());
        assert!(matches!(
            signer.verify(&k, Some(&unbound), "user-1"),
            Err(StorageError::InvalidLink)
        ));
    }

    #[test]
    fn rotation_signs_with_the_first_key_and_accepts_all() {
        let clock = TestClock::at(T0);
        let old = Signer::new(&KEY_A, &[])
            .expect("old")
            .with_clock(clock.boxed());
        let rotated = Signer::new(&KEY_B, &[&KEY_A])
            .expect("rotated")
            .with_clock(clock.boxed());
        let only_new = Signer::new(&KEY_B, &[])
            .expect("new")
            .with_clock(clock.boxed());
        let k = key("a");
        let signed_with_old = signed_query(&old, &k, &SignOptions::default());
        let signed_with_new = signed_query(&rotated, &k, &SignOptions::default());
        assert!(
            rotated.verify(&k, Some(&signed_with_old), "").is_ok(),
            "the previous key is still accepted"
        );
        assert!(rotated.verify(&k, Some(&signed_with_new), "").is_ok());
        assert!(
            only_new.verify(&k, Some(&signed_with_new), "").is_ok(),
            "signing used the first key"
        );
        assert!(
            matches!(
                only_new.verify(&k, Some(&signed_with_old), ""),
                Err(StorageError::InvalidLink)
            ),
            "dropping the old key revokes its links"
        );
    }

    #[test]
    fn a_different_signing_key_never_verifies() {
        let clock = TestClock::at(T0);
        let a = signer(&clock);
        let b = Signer::new(&KEY_B, &[])
            .expect("b")
            .with_clock(clock.boxed());
        let k = key("a");
        let query = signed_query(&a, &k, &SignOptions::default());
        assert!(matches!(
            b.verify(&k, Some(&query), ""),
            Err(StorageError::InvalidLink)
        ));
    }

    #[test]
    fn keys_and_ttls_are_bounded() {
        for short in [vec![], vec![1; 31]] {
            assert!(matches!(
                Signer::new(&short, &[]),
                Err(StorageError::Configuration(_))
            ));
        }
        assert!(
            Signer::new(&KEY_A, &[&[1; 8]]).is_err(),
            "a short previous key is refused too"
        );
        assert!(Signer::new(&[0; 1024], &[]).is_ok());
        let clock = TestClock::at(T0);
        let signer = signer(&clock);
        let k = key("a");
        for ttl in [
            Duration::ZERO,
            Duration::from_millis(999),
            MAX_LINK_TTL + Duration::from_secs(1),
        ] {
            assert!(
                signer.sign(&k, &SignOptions::default().ttl(ttl)).is_err(),
                "{ttl:?}"
            );
        }
        assert!(
            signer
                .sign(&k, &SignOptions::default().ttl(MAX_LINK_TTL))
                .is_ok()
        );
    }

    #[test]
    fn signer_from_config_requires_a_key_and_never_echoes_it() {
        let mut config = StorageConfig::default();
        let missing = Signer::from_config(&config).expect_err("no default key");
        assert!(missing.to_string().contains("FORGE_STORAGE_SIGNING_KEY"));
        config.signing_key = Some(forge_config::SecretString::new("AAAA"));
        let invalid = Signer::from_config(&config).expect_err("short key");
        assert!(!invalid.to_string().contains("AAAA"));
        let key = "dGhpcy1pcy1hLTMyLWJ5dGUtc2lnbmluZy1rZXktISE";
        config.signing_key = Some(forge_config::SecretString::new(key));
        assert!(Signer::from_config(&config).is_ok());
        assert!(!format!("{:?}", Signer::from_config(&config).expect("signer")).contains(key));
    }

    #[test]
    fn urls_have_a_validated_prefix() {
        let clock = TestClock::at(T0);
        let signer = signer(&clock);
        let k = key("avatars/a.png");
        let url = signer
            .url("/files/", &k, &SignOptions::default())
            .expect("url");
        assert!(url.starts_with("/files/avatars/a.png?expires="), "{url}");
        assert!(
            signer
                .url("https://cdn.example.com/files", &k, &SignOptions::default())
                .is_ok()
        );
        for bad in [
            "files",
            "//evil.com/files",
            "javascript:alert(1)",
            "/files?x=1",
            "/files#x",
            "/fi les",
            "/f\\x",
            "",
        ] {
            assert!(
                signer.url(bad, &k, &SignOptions::default()).is_err(),
                "{bad:?}"
            );
        }
        // The URL's query verifies.
        let query = url.split_once('?').expect("query").1;
        assert!(signer.verify(&k, Some(query), "").is_ok());
    }
}
