//! The strict YAML subset `forge deploy` reads from config/deploy.yml
//! (crates/forge-cli/src/deploy/yaml.rs, ADR 0042).
//!
//! The reader is crate-private to forge-cli, so the target compiles the very
//! same source file (it uses only `std`) instead of widening the CLI's public
//! API for tests. Properties: no panic or stack overflow on any text (depth
//! and size are bounded), parsing is deterministic, and the scalar helpers
//! never panic.
#![no_main]
#![allow(dead_code)]

#[path = "../../crates/forge-cli/src/deploy/yaml.rs"]
mod yaml;

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let first = yaml::parse(text);
    let second = yaml::parse(text);
    assert_eq!(
        format!("{first:?}"),
        format!("{second:?}"),
        "parsing is not deterministic"
    );
    yaml::resolve(text);
    if let Some(number) = yaml::parse_go_int(text) {
        // Go's ParseInt takes underscores only between digits (base prefix);
        // whatever it accepts, resolving the same text must not panic.
        let _ = number;
    }
    for line in text.lines().take(64) {
        yaml::resolve(line);
        let _ = yaml::parse_go_int(line);
    }
});
