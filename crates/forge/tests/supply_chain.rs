//! The supply-chain gate (ADR 0043, docs/supply-chain.md): what CI and a
//! release execute is pinned, least-privileged and reachable only the way the
//! documentation says, and every fuzz target is wired in. The Rust twin of
//! Forge-Go's `supply_chain_test.go`; it reads the workflow, script and
//! Dockerfile text, so it needs no YAML parser and no network.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read(relative: &str) -> String {
    let path = root().join(relative);
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

/// The workflows of the framework repository, as (file name, relative path).
fn workflows() -> Vec<(String, String)> {
    let directory = root().join(".github/workflows");
    let mut found: Vec<(String, String)> = fs::read_dir(&directory)
        .unwrap_or_else(|error| panic!("{}: {error}", directory.display()))
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        // `.yaml` workflows run too: none may escape the gate.
        .filter(|name| name.ends_with(".yml") || name.ends_with(".yaml"))
        .map(|name| {
            let relative = format!(".github/workflows/{name}");
            (name, relative)
        })
        .collect();
    found.sort();
    for expected in [
        "ci.yml",
        "fuzz.yml",
        "release.yml",
        "scorecard.yml",
        "supply-chain.yml",
    ] {
        assert!(
            found.iter().any(|(name, _)| name == expected),
            "{expected} is missing from the workflows: {found:?}"
        );
    }
    found
}

fn is_hex(text: &str, length: usize) -> bool {
    text.len() == length
        && text
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// `owner/repo[/path]@<40 hex>`.
fn is_pinned_action(reference: &str) -> bool {
    let Some((name, commit)) = reference.split_once('@') else {
        return false;
    };
    name.contains('/')
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'/'))
        && is_hex(commit, 40)
}

/// `# vX`, `# vX.Y` or `# vX.Y.Z`, the version the commit was verified against.
fn is_version_comment(comment: &str) -> bool {
    let Some(version) = comment.trim().strip_prefix("# v") else {
        return false;
    };
    let parts: Vec<&str> = version.split('.').collect();
    (1..=3).contains(&parts.len())
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
}

/// Whether `reference` ends in `@sha256:<64 hex>`.
fn has_digest(reference: &str) -> bool {
    reference
        .rsplit_once("@sha256:")
        .is_some_and(|(_, digest)| is_hex(digest, 64))
}

/// The references of a container image in `text` that start with `prefix`:
/// up to the next whitespace, quote or backslash.
fn image_references<'a>(text: &'a str, prefix: &str) -> Vec<&'a str> {
    let mut found = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find(prefix) {
        let tail = &rest[start..];
        let end = tail
            .find(|character: char| {
                character.is_whitespace() || matches!(character, '"' | '\\' | '\'')
            })
            .unwrap_or(tail.len());
        found.push(&tail[..end]);
        rest = &tail[end..];
    }
    found
}

#[test]
fn actions_are_pinned_by_commit_with_their_version_in_a_comment() {
    for (name, relative) in workflows() {
        for (number, line) in read(&relative).lines().enumerate() {
            // Every spelling of the key: `uses:`, `- uses:`, `"uses":`, `uses :`.
            let trimmed = line
                .trim_start()
                .trim_start_matches('-')
                .trim_start()
                .trim_start_matches(['"', '\''])
                .trim_start();
            let Some(value) = trimmed.strip_prefix("uses").and_then(|rest| {
                rest.trim_start_matches(['"', '\''])
                    .trim_start()
                    .strip_prefix(':')
            }) else {
                continue;
            };
            let value = value.trim().trim_matches(['"', '\'']);
            let (reference, comment) = value
                .split_once(" #")
                .map_or((value, ""), |(reference, rest)| (reference.trim(), rest));
            if reference.starts_with("./") {
                continue;
            }
            assert!(
                is_pinned_action(reference) && is_version_comment(&format!("#{comment}")),
                "{name}:{}: `{}` must be owner/repo@<40-hex commit> # vX.Y.Z",
                number + 1,
                line.trim()
            );
        }
    }
}

#[test]
fn workflow_containers_are_pinned_by_digest() {
    for (name, relative) in workflows() {
        for (number, line) in read(&relative).lines().enumerate() {
            // A service `image:`, and the `container: <image>` shorthand.
            // `- image:` is a matrix entry (release.yml), not a container.
            let trimmed = line.trim();
            let Some(image) = trimmed
                .strip_prefix("image:")
                .or_else(|| trimmed.strip_prefix("container:"))
            else {
                continue;
            };
            let image = image
                .split_once(" #")
                .map_or(image, |(code, _)| code)
                .trim()
                .trim_matches(['"', '\'']);
            if image.is_empty() {
                // `container:` opening a mapping; its `image:` line is checked.
                continue;
            }
            assert!(
                has_digest(image),
                "{name}:{}: service image `{}` must be pinned by digest",
                number + 1,
                image
            );
        }
    }
}

#[test]
fn sbom_generators_are_pinned_by_digest_and_every_copy_agrees() {
    let files = [
        ".github/workflows/ci.yml",
        ".github/workflows/release.yml",
        "scripts/sbom.sh",
    ];
    for prefix in [
        "ghcr.io/anchore/syft:",
        "docker.io/docker/buildkit-syft-scanner:",
    ] {
        let mut seen = BTreeSet::new();
        for file in files {
            for reference in image_references(&read(file), prefix) {
                assert!(
                    has_digest(reference),
                    "{file}: `{reference}` must be pinned by digest"
                );
                seen.insert(reference.to_owned());
            }
        }
        assert_eq!(
            seen.len(),
            1,
            "want one pinned `{prefix}` reference across the files, found {seen:?}"
        );
    }
}

#[test]
fn published_dockerfiles_pin_their_base_images() {
    let toolchain = read("rust-toolchain.toml");
    let channel = toolchain
        .lines()
        .find_map(|line| line.trim().strip_prefix("channel = "))
        .map(|channel| channel.trim_matches('"').to_owned())
        .expect("rust-toolchain.toml names a channel");
    let dockerfile = read("crates/forge-proxy/Dockerfile");
    let mut stages = Vec::new();
    let mut bases = 0;
    for line in dockerfile.lines() {
        let words: Vec<&str> = line.split_whitespace().collect();
        if !words
            .first()
            .is_some_and(|word| word.eq_ignore_ascii_case("FROM"))
        {
            continue;
        }
        let image = words[1];
        if !stages.contains(&image.to_owned()) {
            bases += 1;
            assert!(
                has_digest(image),
                "forge-proxy/Dockerfile: `{image}` must be pinned by digest"
            );
        }
        if let Some(position) = words
            .iter()
            .position(|word| word.eq_ignore_ascii_case("AS"))
        {
            stages.push(words[position + 1].to_owned());
        }
        if image.starts_with("rust:") {
            assert!(
                image.starts_with(&format!("rust:{channel}-")),
                "forge-proxy/Dockerfile: `{image}` must use the rust-toolchain.toml channel {channel}"
            );
        }
    }
    assert!(bases >= 2, "the build and runtime base images");
}

#[test]
fn checkouts_keep_no_credentials() {
    for (name, relative) in workflows() {
        let text = read(&relative);
        assert_eq!(
            text.matches("uses: actions/checkout@").count(),
            text.matches("persist-credentials: false").count(),
            "{name}: every checkout must set persist-credentials: false"
        );
    }
}

#[test]
fn permissions_are_least_privilege_and_secrets_stay_out_of_pull_requests() {
    for (name, relative) in workflows() {
        let text = read(&relative);
        assert!(
            text.lines().any(|line| line.starts_with("permissions:")),
            "{name}: no top-level permissions"
        );
        assert!(
            !text.contains("pull_request_target"),
            "{name}: pull_request_target runs fork code with secrets"
        );
        assert!(
            !text.contains("write-all"),
            "{name}: write access to everything"
        );
        let release = name == "release.yml";
        // The release job's `id-token` and `packages` are the only scopes any
        // workflow may write; checks, statuses, contents, actions... stay read.
        for (number, line) in text.lines().enumerate() {
            let line = line.split_once(" #").map_or(line, |(code, _)| code);
            let Some((scope, access)) = line.trim().split_once(':') else {
                continue;
            };
            if access.trim().trim_matches(['"', '\'']) != "write" {
                continue;
            }
            let scope = scope.trim().trim_matches(['"', '\'']);
            assert!(
                release && matches!(scope, "id-token" | "packages"),
                "{name}:{}: `{scope}: write` is not allowed",
                number + 1
            );
        }
        for permission in ["id-token: write", "packages: write"] {
            let count = text.matches(permission).count();
            assert!(
                (release && count == 1) || (!release && count == 0),
                "{name}: `{permission}` appears {count} times"
            );
        }
        // No workflow hands its secrets on wholesale or by computed name.
        for forbidden in [
            "secrets[",
            "secrets)",
            "secrets }",
            "secrets}",
            "secrets: inherit",
        ] {
            assert!(
                !text.contains(forbidden),
                "{name}: `{forbidden}` reaches secrets outside the reviewed list"
            );
        }
        // The only secret any workflow reads is the release job's token.
        for (position, _) in text.match_indices("secrets.") {
            let reference: String = text[position..]
                .chars()
                .take_while(|character| {
                    character.is_ascii_alphanumeric() || matches!(character, '.' | '_')
                })
                .collect();
            assert!(
                release && reference == "secrets.GITHUB_TOKEN",
                "{name}: unexpected `{reference}`"
            );
        }
    }
}

#[test]
fn the_release_workflow_runs_only_for_version_tags_and_signs_by_digest() {
    let text = read(".github/workflows/release.yml");
    let trigger = text
        .split("\non:\n")
        .nth(1)
        .and_then(|rest| rest.split("\n\n").next())
        .expect("release.yml has an on: block");
    assert_eq!(
        trigger.trim(),
        "push:\n    tags: [\"v*\"]",
        "release must trigger only on push of v* tags"
    );
    assert!(
        text.lines().any(|line| line == "permissions: {}"),
        "release must default to no permissions"
    );
    for required in [
        "cosign sign --yes \"${IMAGE}@${DIGEST}\"",
        "cosign verify \"${IMAGE}@${DIGEST}\"",
        "provenance: mode=max",
        "sbom: generator=",
        "git merge-base --is-ancestor",
    ] {
        assert!(text.contains(required), "release.yml: missing `{required}`");
    }
}

/// The names the `bin` tables of fuzz/Cargo.toml declare.
fn fuzz_bins() -> BTreeSet<String> {
    let manifest = read("fuzz/Cargo.toml");
    let mut names = BTreeSet::new();
    let mut in_bin = false;
    for line in manifest.lines() {
        let line = line.trim();
        if line == "[[bin]]" {
            in_bin = true;
        } else if line.starts_with('[') {
            in_bin = false;
        } else if let Some(name) = line.strip_prefix("name = ").filter(|_| in_bin) {
            names.insert(name.trim_matches('"').to_owned());
        }
    }
    names
}

#[test]
fn every_fuzz_target_is_declared_seeded_and_documented() {
    let mut files = BTreeSet::new();
    for entry in fs::read_dir(root().join("fuzz/fuzz_targets")).expect("fuzz/fuzz_targets") {
        let name = entry
            .expect("entry")
            .file_name()
            .to_string_lossy()
            .into_owned();
        if let Some(target) = name.strip_suffix(".rs") {
            files.insert(target.to_owned());
        }
    }
    assert_eq!(
        fuzz_bins(),
        files,
        "fuzz/Cargo.toml must declare a [[bin]] for every file of fuzz/fuzz_targets, and no other"
    );
    let manifest = read("fuzz/Cargo.toml");
    let documentation = read("docs/supply-chain.md");
    for target in &files {
        assert!(
            manifest.contains(&format!("path = \"fuzz_targets/{target}.rs\"")),
            "{target}: wrong bin path"
        );
        let seeds = root().join("fuzz/seeds").join(target);
        let count = fs::read_dir(&seeds)
            .unwrap_or_else(|error| panic!("{target}: no seed corpus {}: {error}", seeds.display()))
            .filter_map(Result::ok)
            .filter(|entry| entry.path().is_file())
            .count();
        assert!(count >= 1, "{target}: the seed corpus is empty");
        assert!(
            documentation.contains(&format!("`{target}`")),
            "{target}: missing from the target table of docs/supply-chain.md"
        );
    }
}

#[test]
fn the_fuzz_package_stays_outside_the_framework_workspace() {
    let workspace = read("Cargo.toml");
    let exclude = workspace
        .lines()
        .find(|line| line.starts_with("exclude"))
        .expect("the workspace excludes the standalone packages");
    assert!(exclude.contains("\"fuzz\""), "Cargo.toml: {exclude}");
    let fuzz = read("fuzz/Cargo.toml");
    assert!(
        fuzz.lines().any(|line| line == "[workspace]"),
        "fuzz/Cargo.toml needs its own [workspace]"
    );
    assert!(
        root().join("fuzz/Cargo.lock").is_file(),
        "fuzz/Cargo.lock is committed: the fuzz graph is locked like the framework's"
    );
    // libfuzzer-sys is the one fuzz-only crate; it must not leak into the
    // framework's lockfile.
    assert!(
        !read("Cargo.lock").contains("name = \"libfuzzer-sys\""),
        "libfuzzer-sys entered the framework's Cargo.lock"
    );
}
