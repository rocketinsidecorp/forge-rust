//! The closed set of messages `forge_credentials::parse_keys` can fail with,
//! shared by the `credentials_envelope` fuzz target and by
//! `crates/forge-credentials/tests/key_error_oracle.rs`, which runs the checks
//! a `no_main` fuzz binary cannot.

use forge_credentials::MAX_KEYS;

/// The wording of `parse_keys` failures (`envelope.rs`) and, for a refused
/// key, of `forge_encryption::parse_key`, which names `KEY_SIZE` (32) bytes.
pub const INVALID_KEY: &str =
    "encryption: invalid key: must be canonical unpadded base64url for exactly 32 bytes";

/// Whether `shown` is one of the three wordings `parse_keys` can fail with.
/// They are fixed and carry no key text, so anything else in an error, a
/// repeated key included, is a leak. Looking for the key text inside the
/// message instead flagged key text that is a piece of the fixed wording
/// ("credentials: ke"), and a transformed probe missed verbatim echoes of keys
/// that are themselves valid base64url; a closed set has neither problem.
pub fn is_fixed_message(shown: &str) -> bool {
    if shown == "credentials: no key found"
        || shown == format!("credentials: at most {MAX_KEYS} keys are accepted")
    {
        return true;
    }
    let Some(rest) = shown.strip_prefix("credentials: key ") else {
        return false;
    };
    let Some((index, reason)) = rest.split_once(": ") else {
        return false;
    };
    !index.is_empty() && index.bytes().all(|byte| byte.is_ascii_digit()) && reason == INVALID_KEY
}
