//! `FORGE_MAIL_URL` (forge_config mail settings): the SMTP server the
//! application sends credentials to.
//!
//! Errors never repeat the password; an accepted `smtp://` or `smtps://` URL
//! names exactly one host, a port in 1-65535 and nothing that a client could
//! read differently (no path, parameter, fragment or second host).
#![no_main]

use forge_config::AppConfig;
use libfuzzer_sys::fuzz_target;

const MARKER: &str = "zq7-credential-marker";

fn check(url: &str, carries_marker: bool) {
    let loaded = AppConfig::from_lookup(|key| match key {
        "FORGE_MAIL_URL" => Some(url.to_owned()),
        "FORGE_MAIL_FROM" => Some("app@example.com".to_owned()),
        _ => None,
    });
    let config = match loaded {
        Ok(config) => config,
        Err(error) => {
            if carries_marker {
                let rendered = format!("{error} | {error:?}");
                assert!(!rendered.contains(MARKER), "an error echoed the password");
            }
            return;
        }
    };
    if carries_marker {
        let debug = format!("{config:?}");
        assert!(!debug.contains(MARKER), "Debug echoed the password");
        let json = serde_json::to_string(&config).unwrap_or_default();
        assert!(!json.contains(MARKER), "serialization echoed the password");
    }
    if let Some(endpoint) = config.mail.smtp() {
        assert!(!endpoint.host.is_empty());
        assert!(endpoint.port != 0, "port zero");
        assert!(
            !endpoint
                .host
                .chars()
                .any(|character| character.is_whitespace()
                    || character.is_control()
                    || "@/?#,\\".contains(character)),
            "host {:?} carries delimiters",
            endpoint.host
        );
        if let Some(username) = &endpoint.username {
            assert!(
                !username.chars().any(char::is_control),
                "a control character in the user name"
            );
        }
    }
}

fuzz_target!(|data: &[u8]| {
    let Ok(url) = std::str::from_utf8(data) else {
        return;
    };
    check(url, false);
    if url.contains("zq7") {
        return; // the fuzz text spells the marker: not a leaked password
    }
    for scheme in ["smtp", "smtps", "file"] {
        check(&format!("{scheme}://app:{MARKER}@{url}"), true);
    }
});
