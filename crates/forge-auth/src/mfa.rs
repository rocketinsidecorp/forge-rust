//! TOTP multi-factor primitives (RFC 6238), the encryption of TOTP seeds at
//! rest, one-time backup (recovery) codes and the opaque challenge token of
//! a sign-in that still needs its second factor.
//!
//! Every primitive comes from an audited crate; Forge only composes them:
//!
//! - HMAC-SHA-1 (`hmac` + `sha1`, RustCrypto) computes the codes. SHA-1 is
//!   the HMAC hash RFC 6238 authenticator apps interoperate with; it is not
//!   used for anything else.
//! - AES-256-GCM (`ring`) seals seeds with a fresh random 96-bit nonce; the
//!   stored form is `nonce || ciphertext || tag` with no associated data,
//!   byte-compatible with Forge-Go's `NewAESGCMSecretCipher`.
//! - Seeds, backup codes and challenge tokens come from the operating-system
//!   CSPRNG (`getrandom`); persistence keeps SHA-256 digests (`sha2`) and
//!   compares them in constant time (`subtle`).
//!
//! The profile matches Forge-Go: 160-bit seeds, 6 digits, 30-second
//! periods, the current period plus one before and one after, and a code is
//! accepted only for a period later than the last accepted one, so a code
//! can never be replayed. Seeds, codes, provisioning URIs and tokens are
//! redacted in `Debug` output and never part of an error.

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, KeyInit, Mac};
use ring::aead::{AES_256_GCM, Aad, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use sha1::Sha1;
use sha2::{Digest as _, Sha256};
use subtle::ConstantTimeEq;
use thiserror::Error;

use crate::UnixTimestamp;

/// Bytes of a TOTP seed: 160 bits, as RFC 4226 recommends.
pub const TOTP_SECRET_BYTES: usize = 20;
/// Digits of a TOTP code.
pub const TOTP_DIGITS: usize = 6;
/// Seconds of one TOTP period.
pub const TOTP_PERIOD_SECS: u64 = 30;
/// Backup codes created when a factor is confirmed or its codes regenerated.
pub const MFA_BACKUP_CODE_COUNT: usize = 10;
/// Bytes of the AES-256-GCM key of [`Aes256GcmSecretCipher`]
/// (`FORGE_AUTH_MFA_KEY` holds them as canonical unpadded base64url).
pub const MFA_KEY_BYTES: usize = 32;

const BACKUP_CODE_BYTES: usize = 15;
const CHALLENGE_TOKEN_BYTES: usize = 32;
const TAG_LEN: usize = 16;
const BACKUP_DOMAIN: &[u8] = b"mfa-backup\0";

/// Failure of an MFA primitive. It never carries secret material.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum MfaPrimitiveError {
    /// The operating-system CSPRNG could not provide the bytes.
    #[error("secure randomness unavailable")]
    RandomnessUnavailable,
    /// A seed had the wrong length or encoding.
    #[error("invalid TOTP secret")]
    InvalidSecret,
    /// An issuer or account label was empty, too long or had control
    /// characters.
    #[error("TOTP issuer and account labels are required and must not contain control characters")]
    InvalidLabel,
    /// Sealing or opening a seed failed: a wrong key, a tampered ciphertext
    /// or a digest that does not match.
    #[error("invalid encrypted MFA secret")]
    Cipher,
}

fn random<const N: usize>() -> Result<[u8; N], MfaPrimitiveError> {
    let mut bytes = [0_u8; N];
    getrandom::fill(&mut bytes).map_err(|_| MfaPrimitiveError::RandomnessUnavailable)?;
    Ok(bytes)
}

fn sha256(parts: &[&[u8]]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part);
    }
    hasher.finalize().into()
}

macro_rules! digest_type {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Clone, Copy, Eq)]
        pub struct $name([u8; 32]);

        impl $name {
            /// Wraps persisted digest bytes.
            #[must_use]
            pub const fn from_bytes(bytes: [u8; 32]) -> Self {
                Self(bytes)
            }

            /// Digest bytes for persistence and query binding.
            #[must_use]
            pub const fn as_bytes(&self) -> &[u8; 32] {
                &self.0
            }
        }

        /// Constant-time comparison.
        impl PartialEq for $name {
            fn eq(&self, other: &Self) -> bool {
                self.0.ct_eq(&other.0).into()
            }
        }

        impl std::hash::Hash for $name {
            fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
                self.0.hash(state);
            }
        }

        impl std::fmt::Debug for $name {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str(concat!(stringify!($name), "([REDACTED])"))
            }
        }
    };
}

digest_type!(
    /// SHA-256 digest of a TOTP seed, stored beside its ciphertext and
    /// checked after decryption, so a wrong key or a swapped ciphertext is
    /// never silently accepted.
    TotpSecretDigest
);
digest_type!(
    /// Domain-separated SHA-256 digest of a backup code: what persistence
    /// stores and looks codes up by.
    MfaBackupCodeDigest
);
digest_type!(
    /// SHA-256 digest of an [`MfaChallengeToken`]: what persistence stores.
    MfaChallengeDigest
);
digest_type!(
    /// What a sign-in challenge is bound to besides its token: the SHA-256
    /// digest of the browser's CSRF token issued with it, so a challenge
    /// cookie carried to another browser session is refused.
    MfaChallengeBinding
);

/// A TOTP seed (160 random bits).
#[derive(Clone)]
pub struct TotpSecret([u8; TOTP_SECRET_BYTES]);

impl TotpSecret {
    /// Generates a seed from the operating-system CSPRNG.
    pub fn generate() -> Result<Self, MfaPrimitiveError> {
        random().map(Self)
    }

    /// Restores a seed from exactly [`TOTP_SECRET_BYTES`] bytes.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, MfaPrimitiveError> {
        <[u8; TOTP_SECRET_BYTES]>::try_from(bytes)
            .map(Self)
            .map_err(|_| MfaPrimitiveError::InvalidSecret)
    }

    /// Restores a seed from its unpadded RFC 4648 Base32 text (what an
    /// authenticator app is given), ignoring case, spaces and hyphens.
    pub fn from_base32(text: &str) -> Result<Self, MfaPrimitiveError> {
        let compact = compact_base32(text);
        let bytes = base32_decode(&compact).ok_or(MfaPrimitiveError::InvalidSecret)?;
        Self::from_bytes(&bytes)
    }

    /// The seed as unpadded RFC 4648 Base32 (32 characters), to type into an
    /// authenticator app.
    #[must_use]
    pub fn base32(&self) -> String {
        base32_encode(&self.0)
    }

    /// The digest persistence stores beside the ciphertext.
    #[must_use]
    pub fn digest(&self) -> TotpSecretDigest {
        TotpSecretDigest(sha256(&[&self.0]))
    }

    /// The TOTP period containing `now`.
    #[must_use]
    pub const fn counter_at(now: UnixTimestamp) -> u64 {
        now.as_secs() / TOTP_PERIOD_SECS
    }

    /// The code of period `counter` (RFC 4226 dynamic truncation).
    #[must_use]
    pub fn code(&self, counter: u64) -> String {
        // HMAC takes keys of any length, so this never fails; if it did, the
        // empty code matches nothing and verification fails closed.
        let Ok(mut mac) = <Hmac<Sha1> as KeyInit>::new_from_slice(&self.0) else {
            return String::new();
        };
        mac.update(&counter.to_be_bytes());
        let sum = mac.finalize().into_bytes();
        let offset = usize::from(sum[sum.len() - 1] & 0x0f);
        let binary = (u32::from(sum[offset]) & 0x7f) << 24
            | u32::from(sum[offset + 1]) << 16
            | u32::from(sum[offset + 2]) << 8
            | u32::from(sum[offset + 3]);
        format!("{:06}", binary % 1_000_000)
    }

    /// The code an authenticator app shows at `now`.
    #[must_use]
    pub fn code_at(&self, now: UnixTimestamp) -> String {
        self.code(Self::counter_at(now))
    }

    /// The period a code belongs to, when `code` (six ASCII digits, outer
    /// whitespace ignored) is the code of the current period or of one
    /// before or after it and that period is later than `last_counter`, the
    /// last accepted one. Persist the returned counter before trusting it.
    #[must_use]
    pub fn verify(&self, code: &str, now: UnixTimestamp, last_counter: Option<u64>) -> Option<u64> {
        let code = code.trim();
        if code.len() != TOTP_DIGITS || !code.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        let current = Self::counter_at(now);
        let mut matched = None;
        for counter in [
            Some(current),
            current.checked_sub(1),
            current.checked_add(1),
        ]
        .into_iter()
        .flatten()
        {
            if last_counter.is_some_and(|last| counter <= last) {
                continue;
            }
            let expected = self.code(counter);
            if bool::from(expected.as_bytes().ct_eq(code.as_bytes())) && matched.is_none() {
                matched = Some(counter);
            }
        }
        matched
    }

    /// The `otpauth://totp/` URI authenticator apps import (usually from a
    /// QR code). `issuer` (up to 128 bytes) and `account` (up to 320 bytes)
    /// must be non-empty and free of control characters.
    pub fn provisioning_uri(
        &self,
        issuer: &str,
        account: &str,
    ) -> Result<TotpProvisioningUri, MfaPrimitiveError> {
        let issuer = issuer.trim();
        let account = account.trim();
        if !valid_label(issuer, 128) || !valid_label(account, 320) {
            return Err(MfaPrimitiveError::InvalidLabel);
        }
        let label = percent_encode(&format!("{issuer}:{account}"));
        Ok(TotpProvisioningUri(format!(
            "otpauth://totp/{label}?algorithm=SHA1&digits={TOTP_DIGITS}&issuer={}&period={TOTP_PERIOD_SECS}&secret={}",
            percent_encode(issuer),
            self.base32()
        )))
    }

    /// Encrypts the seed for persistence.
    pub fn seal(&self, cipher: &dyn SecretCipher) -> Result<SealedTotpSecret, MfaPrimitiveError> {
        let ciphertext = cipher
            .seal(&self.0)
            .map_err(|_| MfaPrimitiveError::Cipher)?;
        Ok(SealedTotpSecret {
            ciphertext,
            digest: self.digest(),
        })
    }

    /// Decrypts a persisted seed and checks its length and digest; any
    /// mismatch (wrong key, tampered ciphertext, swapped digest) fails.
    pub fn open(
        cipher: &dyn SecretCipher,
        sealed: &SealedTotpSecret,
    ) -> Result<Self, MfaPrimitiveError> {
        let plaintext = cipher
            .open(&sealed.ciphertext)
            .map_err(|_| MfaPrimitiveError::Cipher)?;
        let secret = Self::from_bytes(&plaintext).map_err(|_| MfaPrimitiveError::Cipher)?;
        if secret.digest() != sealed.digest {
            return Err(MfaPrimitiveError::Cipher);
        }
        Ok(secret)
    }
}

impl std::fmt::Debug for TotpSecret {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("TotpSecret([REDACTED])")
    }
}

fn valid_label(value: &str, max_bytes: usize) -> bool {
    !value.is_empty() && value.len() <= max_bytes && !value.chars().any(char::is_control)
}

/// Percent-encodes everything but RFC 3986 unreserved characters.
fn percent_encode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len() * 3);
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(char::from(byte));
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

/// An `otpauth://` provisioning URI. It contains the seed: never log it.
#[derive(Clone)]
pub struct TotpProvisioningUri(String);

impl TotpProvisioningUri {
    /// Exposes the URI only to the page that shows it once.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for TotpProvisioningUri {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("TotpProvisioningUri([REDACTED])")
    }
}

/// A seed as persistence stores it: authenticated ciphertext and digest.
#[derive(Clone, Eq, PartialEq)]
pub struct SealedTotpSecret {
    ciphertext: Vec<u8>,
    digest: TotpSecretDigest,
}

impl SealedTotpSecret {
    /// Restores a persisted seed. [`TotpSecret::open`] authenticates it.
    #[must_use]
    pub fn new(ciphertext: Vec<u8>, digest: TotpSecretDigest) -> Self {
        Self { ciphertext, digest }
    }

    /// The ciphertext to persist (`nonce || ciphertext || tag`).
    #[must_use]
    pub fn ciphertext(&self) -> &[u8] {
        &self.ciphertext
    }

    /// The seed's digest to persist and bind challenges to.
    #[must_use]
    pub const fn digest(&self) -> &TotpSecretDigest {
        &self.digest
    }
}

impl std::fmt::Debug for SealedTotpSecret {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SealedTotpSecret([REDACTED])")
    }
}

/// Authenticated encryption of MFA seeds before they reach persistence.
///
/// Implementations must authenticate (an AEAD); a failure must say nothing
/// about why. Applications that need key identifiers, a KMS or online key
/// rotation implement this port; [`Aes256GcmSecretCipher`] is the built-in
/// single-key cipher.
pub trait SecretCipher: Send + Sync {
    /// Encrypts `plaintext` with a fresh nonce.
    fn seal(&self, plaintext: &[u8]) -> Result<Vec<u8>, SecretCipherError>;

    /// Decrypts and authenticates what [`SecretCipher::seal`] returned.
    fn open(&self, sealed: &[u8]) -> Result<Vec<u8>, SecretCipherError>;
}

/// A cipher failure that never echoes keys, plaintext or ciphertext.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum SecretCipherError {
    /// The key is not canonical unpadded base64url of exactly 32 bytes.
    #[error("MFA encryption key must be canonical unpadded base64url for exactly 32 bytes")]
    InvalidKey,
    /// Encryption failed (no randomness for the nonce).
    #[error("MFA secret encryption failed")]
    Seal,
    /// Decryption or authentication failed: a wrong key or a modified
    /// ciphertext, deliberately not told apart.
    #[error("invalid encrypted MFA secret")]
    Open,
}

/// AES-256-GCM (`ring`) with one 32-byte key, as Forge-Go's
/// `NewAESGCMSecretCipher`: `nonce (12) || ciphertext || tag (16)`, random
/// nonces, no associated data. Load the key from the environment or a
/// secret manager (`FORGE_AUTH_MFA_KEY`), never from the identity database.
pub struct Aes256GcmSecretCipher {
    key: LessSafeKey,
}

impl Aes256GcmSecretCipher {
    /// Builds the cipher from exactly [`MFA_KEY_BYTES`] bytes.
    pub fn new(key: &[u8]) -> Result<Self, SecretCipherError> {
        if key.len() != MFA_KEY_BYTES {
            return Err(SecretCipherError::InvalidKey);
        }
        let key = UnboundKey::new(&AES_256_GCM, key).map_err(|_| SecretCipherError::InvalidKey)?;
        Ok(Self {
            key: LessSafeKey::new(key),
        })
    }

    /// Builds the cipher from the canonical unpadded base64url encoding of
    /// 32 bytes, the format of `FORGE_AUTH_MFA_KEY`. The error never echoes
    /// the value.
    pub fn from_base64url(encoded: &str) -> Result<Self, SecretCipherError> {
        Self::new(&parse_mfa_key(encoded)?)
    }
}

impl std::fmt::Debug for Aes256GcmSecretCipher {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Aes256GcmSecretCipher([REDACTED])")
    }
}

/// Decodes `FORGE_AUTH_MFA_KEY`: canonical unpadded base64url (no padding,
/// no trailing bits, the URL-safe alphabet) of exactly 32 bytes.
pub fn parse_mfa_key(encoded: &str) -> Result<[u8; MFA_KEY_BYTES], SecretCipherError> {
    let decoded = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| SecretCipherError::InvalidKey)?;
    if URL_SAFE_NO_PAD.encode(&decoded) != encoded {
        return Err(SecretCipherError::InvalidKey);
    }
    <[u8; MFA_KEY_BYTES]>::try_from(decoded.as_slice()).map_err(|_| SecretCipherError::InvalidKey)
}

impl SecretCipher for Aes256GcmSecretCipher {
    fn seal(&self, plaintext: &[u8]) -> Result<Vec<u8>, SecretCipherError> {
        let nonce: [u8; NONCE_LEN] = random().map_err(|_| SecretCipherError::Seal)?;
        let mut in_out = plaintext.to_vec();
        self.key
            .seal_in_place_append_tag(
                Nonce::assume_unique_for_key(nonce),
                Aad::empty(),
                &mut in_out,
            )
            .map_err(|_| SecretCipherError::Seal)?;
        let mut sealed = Vec::with_capacity(NONCE_LEN + in_out.len());
        sealed.extend_from_slice(&nonce);
        sealed.extend_from_slice(&in_out);
        Ok(sealed)
    }

    fn open(&self, sealed: &[u8]) -> Result<Vec<u8>, SecretCipherError> {
        if sealed.len() < NONCE_LEN + TAG_LEN {
            return Err(SecretCipherError::Open);
        }
        let (nonce, ciphertext) = sealed.split_at(NONCE_LEN);
        let nonce = Nonce::try_assume_unique_for_key(nonce).map_err(|_| SecretCipherError::Open)?;
        let mut in_out = ciphertext.to_vec();
        let plaintext = self
            .key
            .open_in_place(nonce, Aad::empty(), &mut in_out)
            .map_err(|_| SecretCipherError::Open)?;
        Ok(plaintext.to_vec())
    }
}

/// A one-time backup (recovery) code: 120 random bits as 24 Base32
/// characters in groups of four (`ABCD-EFGH-...`). Only its digest is
/// persisted; the code is shown once.
#[derive(Clone)]
pub struct MfaBackupCode(String);

impl MfaBackupCode {
    /// Generates `count` distinct codes (1 to 100) and their digests.
    pub fn generate_set(
        count: usize,
    ) -> Result<(Vec<Self>, Vec<MfaBackupCodeDigest>), MfaPrimitiveError> {
        if !(1..=100).contains(&count) {
            return Err(MfaPrimitiveError::InvalidSecret);
        }
        let mut codes = Vec::with_capacity(count);
        let mut digests: Vec<MfaBackupCodeDigest> = Vec::with_capacity(count);
        while codes.len() < count {
            let raw: [u8; BACKUP_CODE_BYTES] = random()?;
            let digest = backup_digest(&raw);
            if digests.contains(&digest) {
                continue;
            }
            digests.push(digest);
            codes.push(Self(group(&base32_encode(&raw))));
        }
        Ok((codes, digests))
    }

    /// The digest of `input` when it has the shape of a backup code
    /// (ignoring case, spaces and hyphens); `None` for anything else, such
    /// as a six-digit TOTP code.
    #[must_use]
    pub fn parse(input: &str) -> Option<MfaBackupCodeDigest> {
        let compact = compact_base32(input);
        if compact.len() != BACKUP_CODE_BYTES * 8 / 5 {
            return None;
        }
        let raw = base32_decode(&compact)?;
        Some(backup_digest(&raw))
    }

    /// Exposes the code only to the page that shows it once.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for MfaBackupCode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("MfaBackupCode([REDACTED])")
    }
}

fn backup_digest(raw: &[u8]) -> MfaBackupCodeDigest {
    MfaBackupCodeDigest(sha256(&[BACKUP_DOMAIN, raw]))
}

fn group(encoded: &str) -> String {
    let mut grouped = String::with_capacity(encoded.len() + encoded.len() / 4);
    for (index, character) in encoded.chars().enumerate() {
        if index > 0 && index % 4 == 0 {
            grouped.push('-');
        }
        grouped.push(character);
    }
    grouped
}

/// The opaque token of a pending second-factor sign-in: 256 random bits as
/// unpadded base64url (43 characters), held in a `HttpOnly` cookie. Only
/// its SHA-256 digest is persisted.
#[derive(Clone, Eq, PartialEq)]
pub struct MfaChallengeToken {
    encoded: String,
    raw: [u8; CHALLENGE_TOKEN_BYTES],
}

impl MfaChallengeToken {
    /// Generates a token from the operating-system CSPRNG.
    pub fn generate() -> Result<Self, MfaPrimitiveError> {
        let raw: [u8; CHALLENGE_TOKEN_BYTES] = random()?;
        Ok(Self {
            encoded: URL_SAFE_NO_PAD.encode(raw),
            raw,
        })
    }

    /// Accepts only the canonical encoding of 32 bytes.
    pub fn new(value: &str) -> Result<Self, MfaPrimitiveError> {
        let decoded = URL_SAFE_NO_PAD
            .decode(value)
            .map_err(|_| MfaPrimitiveError::InvalidSecret)?;
        let raw = <[u8; CHALLENGE_TOKEN_BYTES]>::try_from(decoded.as_slice())
            .map_err(|_| MfaPrimitiveError::InvalidSecret)?;
        if URL_SAFE_NO_PAD.encode(raw) != value {
            return Err(MfaPrimitiveError::InvalidSecret);
        }
        Ok(Self {
            encoded: value.to_owned(),
            raw,
        })
    }

    /// The digest persistence stores (SHA-256 of the 32 raw bytes, as
    /// Forge-Go).
    #[must_use]
    pub fn digest(&self) -> MfaChallengeDigest {
        MfaChallengeDigest(sha256(&[&self.raw]))
    }

    /// Exposes the token only to the challenge cookie.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.encoded
    }
}

impl std::fmt::Debug for MfaChallengeToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("MfaChallengeToken([REDACTED])")
    }
}

const BASE32_ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

/// Uppercase without spaces or hyphens.
fn compact_base32(text: &str) -> String {
    text.chars()
        .filter(|character| !character.is_whitespace() && *character != '-')
        .map(|character| character.to_ascii_uppercase())
        .collect()
}

/// RFC 4648 Base32 of whole 5-byte groups, so there is never padding.
fn base32_encode(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(bytes.len() * 8 / 5);
    for chunk in bytes.chunks(5) {
        let mut buffer = [0_u8; 5];
        buffer[..chunk.len()].copy_from_slice(chunk);
        let bits = buffer
            .iter()
            .fold(0_u64, |acc, byte| (acc << 8) | u64::from(*byte));
        let characters = (chunk.len() * 8).div_ceil(5);
        for index in 0..characters {
            let shift = 35 - index * 5;
            encoded.push(char::from(
                BASE32_ALPHABET[usize::from((bits >> shift) as u8 & 0x1f)],
            ));
        }
    }
    encoded
}

/// Decodes unpadded uppercase Base32 of whole 8-character groups.
fn base32_decode(text: &str) -> Option<Vec<u8>> {
    if text.is_empty() || !text.len().is_multiple_of(8) {
        return None;
    }
    let mut bytes = Vec::with_capacity(text.len() * 5 / 8);
    for chunk in text.as_bytes().chunks(8) {
        let mut bits = 0_u64;
        for character in chunk {
            let value = BASE32_ALPHABET
                .iter()
                .position(|candidate| candidate == character)?;
            bits = (bits << 5) | value as u64;
        }
        bytes.extend_from_slice(&bits.to_be_bytes()[3..]);
    }
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(seconds: u64) -> UnixTimestamp {
        UnixTimestamp::from_secs(seconds)
    }

    fn rfc_secret() -> TotpSecret {
        TotpSecret::from_bytes(b"12345678901234567890").expect("20 bytes")
    }

    fn key() -> String {
        URL_SAFE_NO_PAD.encode([7_u8; 32])
    }

    #[test]
    fn codes_match_the_rfc_6238_sha1_vectors() {
        // RFC 6238 Appendix B (8 digits); Forge keeps the last six.
        let secret = rfc_secret();
        for (time, code) in [
            (59, "287082"),
            (1_111_111_109, "081804"),
            (1_111_111_111, "050471"),
            (1_234_567_890, "005924"),
            (2_000_000_000, "279037"),
            (20_000_000_000, "353130"),
        ] {
            assert_eq!(secret.code_at(at(time)), code, "T={time}");
        }
    }

    #[test]
    fn verification_accepts_one_period_of_skew_and_never_a_replay() {
        let secret = rfc_secret();
        let now = at(1_111_111_111);
        let current = TotpSecret::counter_at(now);
        for counter in [current - 1, current, current + 1] {
            assert_eq!(
                secret.verify(&secret.code(counter), now, None),
                Some(counter)
            );
        }
        for counter in [current - 2, current + 2] {
            assert_eq!(secret.verify(&secret.code(counter), now, None), None);
        }
        // A code at or before the last accepted counter is a replay.
        let code = secret.code(current);
        assert_eq!(secret.verify(&code, now, Some(current)), None);
        assert_eq!(secret.verify(&code, now, Some(current + 1)), None);
        assert_eq!(secret.verify(&code, now, Some(current - 1)), Some(current));
        // Shape: six ASCII digits only.
        for bad in ["", "12345", "1234567", "12345a", "１２３４５６"] {
            assert_eq!(secret.verify(bad, now, None), None, "{bad}");
        }
        assert_eq!(
            secret.verify(&format!(" {code}\n"), now, None),
            Some(current)
        );
        // A wrong seed's codes are wrong.
        let other = TotpSecret::generate().expect("random");
        assert_eq!(
            other.verify(&code, now, None).is_some(),
            other.code(current) == code
        );
    }

    #[test]
    fn base32_round_trips_and_provisioning_uri_is_encoded() {
        let secret = rfc_secret();
        assert_eq!(secret.base32(), "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ");
        let restored =
            TotpSecret::from_base32("gezd gnbv-gy3t qojq gezd gnbv gy3t qojq").expect("base32");
        assert_eq!(restored.digest(), secret.digest());
        for bad in [
            "",
            "GEZDGNBV",
            "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJ1",
            "========",
        ] {
            assert!(TotpSecret::from_base32(bad).is_err(), "{bad}");
        }
        let uri = secret
            .provisioning_uri("Acme Inc", "ada@example.com")
            .expect("labels");
        assert_eq!(
            uri.expose(),
            "otpauth://totp/Acme%20Inc%3Aada%40example.com?algorithm=SHA1&digits=6\
             &issuer=Acme%20Inc&period=30&secret=GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ"
        );
        assert!(!format!("{uri:?}").contains("GEZD"));
        assert!(!format!("{secret:?}").contains("GEZD"));
        for (issuer, account) in [("", "a"), ("a", ""), ("a\nb", "c"), ("a", "b\u{7f}")] {
            assert_eq!(
                secret.provisioning_uri(issuer, account).err(),
                Some(MfaPrimitiveError::InvalidLabel)
            );
        }
        assert!(secret.provisioning_uri(&"i".repeat(129), "a").is_err());
    }

    #[test]
    fn sealed_seeds_round_trip_and_fail_closed_on_tamper_or_wrong_key() {
        let cipher = Aes256GcmSecretCipher::from_base64url(&key()).expect("key");
        let secret = TotpSecret::generate().expect("random");
        let sealed = secret.seal(&cipher).expect("seal");
        assert_eq!(
            sealed.ciphertext().len(),
            NONCE_LEN + TOTP_SECRET_BYTES + TAG_LEN
        );
        assert_ne!(
            &sealed.ciphertext()[NONCE_LEN..][..TOTP_SECRET_BYTES],
            &secret.0
        );
        let opened = TotpSecret::open(&cipher, &sealed).expect("open");
        assert_eq!(opened.digest(), secret.digest());
        // Two seals of one seed differ (fresh nonce).
        assert_ne!(
            secret.seal(&cipher).expect("seal").ciphertext(),
            sealed.ciphertext()
        );

        // Every flipped bit is detected.
        for index in 0..sealed.ciphertext().len() {
            let mut tampered = sealed.ciphertext().to_vec();
            tampered[index] ^= 0x01;
            let tampered = SealedTotpSecret::new(tampered, *sealed.digest());
            assert_eq!(
                TotpSecret::open(&cipher, &tampered).err(),
                Some(MfaPrimitiveError::Cipher),
                "byte {index}"
            );
        }
        // Truncated.
        let short = SealedTotpSecret::new(sealed.ciphertext()[..20].to_vec(), *sealed.digest());
        assert!(TotpSecret::open(&cipher, &short).is_err());
        // Wrong key.
        let other = Aes256GcmSecretCipher::new(&[8_u8; 32]).expect("key");
        assert_eq!(
            TotpSecret::open(&other, &sealed).err(),
            Some(MfaPrimitiveError::Cipher)
        );
        // A valid ciphertext of another seed under the recorded digest.
        let swapped = SealedTotpSecret::new(
            TotpSecret::generate()
                .expect("random")
                .seal(&cipher)
                .expect("seal")
                .ciphertext()
                .to_vec(),
            *sealed.digest(),
        );
        assert_eq!(
            TotpSecret::open(&cipher, &swapped).err(),
            Some(MfaPrimitiveError::Cipher)
        );
        assert_eq!(format!("{cipher:?}"), "Aes256GcmSecretCipher([REDACTED])");
        assert!(!format!("{sealed:?}").contains(&format!("{:?}", sealed.ciphertext())));
    }

    fn from_hex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|index| u8::from_str_radix(&text[index..index + 2], 16).expect("hex"))
            .collect()
    }

    #[test]
    fn the_stored_form_is_forge_go_compatible() {
        // Produced by Go's crypto/cipher AES-256-GCM as Forge-Go's
        // NewAESGCMSecretCipher stores it (nonce||ciphertext||tag, no
        // associated data): key 0x07 * 32, nonce 0x01 * 12, plaintext
        // "12345678901234567890".
        let stored = from_hex(
            "01010101010101010101010147d3ba83a589db36e8e4ed13430345079fc32dc4\
             f6e56db92900529dcce011bd83a7ae2f",
        );
        let cipher = Aes256GcmSecretCipher::new(&[7_u8; 32]).expect("key");
        assert_eq!(cipher.open(&stored).expect("open"), b"12345678901234567890");
    }

    #[test]
    fn keys_must_be_canonical_unpadded_base64url_of_32_bytes() {
        assert!(parse_mfa_key(&key()).is_ok());
        let padded = format!("{}=", key());
        let standard = "+/+/+/+/+/+/+/+/+/+/+/+/+/+/+/+/+/+/+/+/+/8";
        let short = URL_SAFE_NO_PAD.encode([1_u8; 31]);
        let long = URL_SAFE_NO_PAD.encode([1_u8; 33]);
        // Non-zero trailing bits: not the canonical spelling of its bytes.
        let mut non_canonical = key();
        non_canonical.pop();
        non_canonical.push('B');
        for bad in [
            "",
            padded.as_str(),
            standard,
            short.as_str(),
            long.as_str(),
            non_canonical.as_str(),
            " AAAA",
        ] {
            assert_eq!(
                Aes256GcmSecretCipher::from_base64url(bad).err(),
                Some(SecretCipherError::InvalidKey),
                "{bad}"
            );
        }
        assert!(Aes256GcmSecretCipher::new(&[0_u8; 16]).is_err());
        assert!(!SecretCipherError::InvalidKey.to_string().contains("AAAA"));
    }

    #[test]
    fn backup_codes_are_distinct_formatted_and_parsed_by_digest() {
        let (codes, digests) = MfaBackupCode::generate_set(MFA_BACKUP_CODE_COUNT).expect("codes");
        assert_eq!(codes.len(), MFA_BACKUP_CODE_COUNT);
        for (code, digest) in codes.iter().zip(&digests) {
            assert_eq!(code.expose().len(), 29, "{}", code.expose());
            assert_eq!(code.expose().matches('-').count(), 5);
            assert_eq!(MfaBackupCode::parse(code.expose()), Some(*digest));
            let typed = code.expose().to_lowercase().replace('-', " ");
            assert_eq!(MfaBackupCode::parse(&typed), Some(*digest));
            assert!(!format!("{code:?}").contains(code.expose()));
        }
        let unique: std::collections::HashSet<_> = digests.iter().collect();
        assert_eq!(unique.len(), digests.len());
        for bad in ["123456", "", "ABCD-EFGH", "ABCD-EFGH-IJKL-MNOP-QRST-UVW1"] {
            assert_eq!(MfaBackupCode::parse(bad), None, "{bad}");
        }
        assert!(MfaBackupCode::generate_set(0).is_err());
        assert!(MfaBackupCode::generate_set(101).is_err());
    }

    #[test]
    fn backup_codes_are_forge_go_compatible() {
        // Forge-Go: Base32 (no padding) of bytes 0..15 and
        // sha256("mfa-backup\0" || raw), computed with Go's standard library.
        let raw: Vec<u8> = (0..15).collect();
        let code = group(&base32_encode(&raw));
        assert_eq!(code, "AAAQ-EAYE-AUDA-OCAJ-BIFQ-YDIO");
        assert_eq!(
            MfaBackupCode::parse(&code).map(|digest| digest.as_bytes().to_vec()),
            Some(from_hex(
                "091d9a1002b79f5af04c5589ebfac567b05d9bd621faa2478475742962c918b9"
            ))
        );
    }

    #[test]
    fn challenge_tokens_are_canonical_and_digested_from_raw_bytes() {
        let token = MfaChallengeToken::generate().expect("random");
        assert_eq!(token.expose().len(), 43);
        let parsed = MfaChallengeToken::new(token.expose()).expect("canonical");
        assert_eq!(parsed.digest(), token.digest());
        let raw = URL_SAFE_NO_PAD.decode(token.expose()).expect("base64url");
        let expected: [u8; 32] = Sha256::digest(&raw).into();
        assert_eq!(token.digest().as_bytes(), &expected);
        for bad in ["", "abc", &format!("{}=", token.expose()), &"A".repeat(44)] {
            assert!(MfaChallengeToken::new(bad).is_err(), "{bad}");
        }
        assert!(!format!("{token:?}").contains(token.expose()));
        assert_ne!(
            MfaChallengeToken::generate().expect("random"),
            MfaChallengeToken::generate().expect("random")
        );
    }

    #[test]
    fn base32_matches_rfc_4648_vectors() {
        assert_eq!(base32_encode(b"fooba"), "MZXW6YTB");
        assert_eq!(base32_decode("MZXW6YTB").as_deref(), Some(&b"fooba"[..]));
        assert_eq!(base32_decode("MZXW6YT1"), None);
        assert_eq!(base32_decode("MZXW6YT"), None);
    }
}
