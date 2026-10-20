//! The `notAfter` forge-proxy reads from the PEM/DER certificates it caches
//! (forge_proxy::der::not_after, ADR 0042): a hand-written DER reader over
//! bytes from disk and from the ACME server.
//!
//! Properties: no panic on any bytes, no out-of-bounds read, and an accepted
//! time lies in the range the two ASN.1 time types can express (a
//! GeneralizedTime has a four-digit year, 0000-9999).
#![no_main]

use forge_proxy::der::not_after;
use libfuzzer_sys::fuzz_target;

// 0000-01-01T00:00:00Z and 9999-12-31T23:59:59Z.
const EARLIEST: i64 = -62_167_219_200;
const LATEST: i64 = 253_402_300_799;

fuzz_target!(|data: &[u8]| {
    if let Some(seconds) = not_after(data) {
        assert!(
            (EARLIEST..=LATEST).contains(&seconds),
            "notAfter {seconds} is outside the ASN.1 time range"
        );
    }
});
