//! The oracle the `credentials_envelope` fuzz target uses for key errors
//! (`fuzz/oracles/credentials_key_errors.rs`): the real wordings pass it and
//! a message that repeats a key does not. A `no_main` fuzz binary cannot run
//! unit tests, so they run here.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use forge_credentials::{MAX_KEYS, parse_keys};

#[path = "../../../fuzz/oracles/credentials_key_errors.rs"]
mod oracle;
use oracle::{INVALID_KEY, is_fixed_message};

#[test]
fn every_failure_of_parse_keys_is_a_fixed_message() {
    let too_many = ["x"; MAX_KEYS + 1].join("\n");
    let long = "A".repeat(44);
    let valid = "QkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkI";
    let non_canonical = format!("{}Z", "A".repeat(42));
    let after_a_valid_key = format!("{valid}\n{long}\n");
    for text in [
        "",
        "  \n# comment\n",
        "!",
        "a,b",
        "ke",
        "credentials: ke",
        long.as_str(),
        non_canonical.as_str(),
        too_many.as_str(),
        after_a_valid_key.as_str(),
    ] {
        let error = parse_keys(text).expect_err("not a usable key text");
        let shown = error.to_string();
        assert!(is_fixed_message(&shown), "{text:?} -> {shown:?}");
    }
    // Eight valid keys are fine, nine are refused with the fixed count.
    let nine = [valid; MAX_KEYS + 1].join(",");
    assert!(is_fixed_message(
        &parse_keys(&nine).expect_err("too many").to_string()
    ));
}

#[test]
fn a_message_that_repeats_a_key_is_flagged() {
    // `"A"*42 + "Z"` is itself valid base64url: a leak of it passes any
    // oracle that tests the key text after transforming it.
    let key = format!("{}Z", "A".repeat(42));
    for leaky in [
        format!("credentials: key 1: encryption: invalid key: {key}"),
        format!("credentials: key 1: {INVALID_KEY} ({key})"),
        format!("credentials: key 1: {INVALID_KEY}: {key}"),
        format!("credentials: key {key}"),
        format!("credentials: invalid key {key}"),
        format!("credentials: at most {MAX_KEYS} keys are accepted: {key}"),
        format!("credentials: no key found: {key}"),
        format!("credentials: key : {INVALID_KEY}"),
        format!("credentials: key x1: {INVALID_KEY}"),
        "credentials: key 1: ".to_owned(),
        key,
    ] {
        assert!(!is_fixed_message(&leaky), "{leaky:?}");
    }
}
