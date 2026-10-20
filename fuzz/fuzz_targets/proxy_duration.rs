//! Go duration text in flags, `config/deploy.yml` and forge-proxy requests
//! (forge_proxy::GoDuration, ADR 0042).
//!
//! Properties: no panic or arithmetic overflow; a parsed duration prints in
//! Go's syntax and parses back to the same number of nanoseconds.
#![no_main]

use forge_proxy::GoDuration;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    if let Ok(duration) = GoDuration::parse(text) {
        let printed = duration.to_string();
        assert_eq!(
            GoDuration::parse(&printed),
            Ok(duration),
            "{text:?} -> {printed:?} changed the value"
        );
        let _ = duration.to_std();
        let _ = duration.whole_seconds();
    }
});
