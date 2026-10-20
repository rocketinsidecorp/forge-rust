//! The timestamps `forge deploy` and forge-proxy read back from lock records,
//! logs and `docker ps` (forge_proxy::clock::Timestamp, ADR 0042).
//!
//! Properties: no panic; a parsed RFC 3339 instant prints and parses back to
//! the same instant; a parsed Docker timestamp denotes an instant the RFC 3339
//! printer can express.
#![no_main]

use forge_proxy::clock::Timestamp;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    if let Some(stamp) = Timestamp::parse_rfc3339(text) {
        let printed = stamp.rfc3339_nano();
        let again = Timestamp::parse_rfc3339(&printed)
            .unwrap_or_else(|| panic!("{printed:?} (from {text:?}) does not parse"));
        assert_eq!(
            (again.unix_seconds, again.nanos),
            (stamp.unix_seconds, stamp.nanos),
            "{text:?} -> {printed:?} changed the instant"
        );
        let _ = stamp.rfc3339();
        let _ = stamp.rfc3339_millis();
        let _ = stamp.human();
        let _ = stamp.docker();
    }
    if let Some(stamp) = Timestamp::parse_docker(text) {
        let printed = stamp.rfc3339_nano();
        assert!(Timestamp::parse_rfc3339(&printed).is_some(), "{printed:?}");
        let _ = stamp.millis_since(Timestamp::default());
    }
});
