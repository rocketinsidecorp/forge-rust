//! `FORGE_DATABASE_URL` and the helpers `forge doctor` and `forge db` share
//! (forge_config::database_url).
//!
//! The URL carries the database password, so the property that matters most
//! is that nothing the configuration says about a refused or accepted URL
//! repeats its credentials. Input: the URL text, with a NUL splitting it from
//! an optional `FORGE_ENV`.
#![no_main]

use forge_config::{AppConfig, database_url};
use libfuzzer_sys::fuzz_target;

const MARKER: &str = "zq7-credential-marker";

fn load(environment: Option<&str>, url: &str) -> Result<AppConfig, String> {
    AppConfig::from_lookup(|key| match key {
        "FORGE_DATABASE_URL" => Some(url.to_owned()),
        "FORGE_ENV" => environment.map(str::to_owned),
        _ => None,
    })
    .map_err(|error| format!("{error} | {error:?}"))
}

fn check(environment: Option<&str>, url: &str, carries_marker: bool) {
    match load(environment, url) {
        Err(rendered) => {
            if carries_marker {
                assert!(!rendered.contains(MARKER), "an error echoed the password");
            }
        }
        Ok(config) => {
            let Some(secret) = config.database.url.as_ref() else {
                return;
            };
            let described = database_url::describe(secret);
            let debug = format!("{config:?}");
            if carries_marker {
                assert!(
                    !described.contains(MARKER),
                    "describe echoed the password: {url:?} -> {described:?}"
                );
                assert!(!debug.contains(MARKER), "Debug echoed the password");
                let json = serde_json::to_string(&config).unwrap_or_default();
                assert!(!json.contains(MARKER), "serialization echoed the password");
            }
            // The decisions about where credentials are sent are consistent:
            // a URL with an endpoint is well formed, and the endpoint is a
            // plain host.
            if let Some((host, port)) = database_url::endpoint(secret) {
                assert!(database_url::is_well_formed(secret));
                // Port 0 names no server (found by this target, fixed in
                // database_url::location; seed `port-zero`).
                assert_ne!(port, 0, "endpoint with port 0");
                assert!(!host.is_empty());
                assert!(
                    !host.chars().any(|character| character.is_whitespace()
                        || character.is_control()
                        || "@/?#\\".contains(character)),
                    "host {host:?} carries delimiters"
                );
            }
        }
    }
}

/// Whether `text`, once the URL parser has removed tab, LF and CR, and
/// lowercased and percent-decoded it, contains the start of the marker.
fn spells_marker(text: &str) -> bool {
    let stripped: Vec<u8> = text
        .bytes()
        .filter(|byte| !matches!(byte, b'\t' | b'\n' | b'\r'))
        .collect();
    let mut decoded = Vec::with_capacity(stripped.len());
    let mut index = 0;
    while index < stripped.len() {
        let hex = |byte: u8| char::from(byte).to_digit(16);
        let escaped = match (stripped.get(index + 1), stripped.get(index + 2)) {
            (Some(&high), Some(&low)) if stripped[index] == b'%' => hex(high)
                .zip(hex(low))
                .map(|(high, low)| (high * 16 + low) as u8),
            _ => None,
        };
        if let Some(byte) = escaped {
            decoded.push(byte);
            index += 3;
        } else {
            decoded.push(stripped[index]);
            index += 1;
        }
    }
    decoded.make_ascii_lowercase();
    decoded.windows(3).any(|window| window == b"zq7")
}

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let (url, environment) = match text.split_once('\0') {
        Some((url, environment)) => (url, Some(environment)),
        None => (text, None),
    };
    check(environment, url, false);
    // Fuzz text that spells the marker would make a host or path look like a
    // leaked password. The URL parser removes ASCII tab, LF and CR anywhere
    // and lowercases hosts, so the text is read the way the parser reads it
    // (seed `marker-split-by-newlines`).
    if spells_marker(text) {
        return;
    }
    // SQLite URLs have no userinfo: what follows `sqlite://` is a file name,
    // which `describe` rightly shows.
    for scheme in ["postgres", "mysql", "mariadb"] {
        let credentialed = format!("{scheme}://app:{MARKER}@{url}");
        check(environment, &credentialed, true);
    }
});
