//! The supply-chain gate (ADR 0043, docs/supply-chain.md): what CI and a
//! release execute is pinned, least-privileged and reachable only the way the
//! documentation says, and every fuzz target is wired in. The Rust twin of
//! Forge-Go's `supply_chain_test.go`. It reads the workflow, script and
//! Dockerfile text line by line, and parses every gated workflow with a YAML
//! parser (yaml-rust2, a test-only dependency) to check that the line reader
//! reads what YAML does; it needs no network.
//!
//! Each rule is a function from a file's text to its violations
//! (`gate/rules.rs`), so the `*_rejects_*` tests below can prove, on
//! hand-written snippets, that every known bypass is caught and that the
//! accepted spellings stay accepted. The generated application's workflows
//! are gated as rendered, for every `forge new` option, by forge-cli's
//! `src/generate/supply_chain.rs` with the same rules; the template is read
//! here too, all branches merged, as an extra check only.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[path = "gate/rules.rs"]
mod rules;
use rules::*;

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

/// The workflows plus the `action.yml` of every local `./` action they reach,
/// directly or through another local action: a local action runs with the
/// workflow's permissions, so the same rules apply to it.
fn workflows_and_local_actions() -> Vec<(String, String)> {
    let mut files = workflows();
    let mut pending: Vec<String> = files
        .iter()
        .flat_map(|(_, relative)| local_actions(&read(relative)))
        .collect();
    let mut seen = BTreeSet::new();
    while let Some(action) = pending.pop() {
        if !seen.insert(action.clone()) {
            continue;
        }
        if action.ends_with(".yml") || action.ends_with(".yaml") {
            // A local reusable workflow: it must be one of the gated workflows.
            // Directly in .github/workflows: `../` cannot lead elsewhere.
            let direct = action
                .strip_prefix(".github/workflows/")
                .is_some_and(|file| !file.contains('/'));
            assert!(
                direct && root().join(&action).is_file(),
                "`./{action}` is not a workflow of .github/workflows"
            );
            continue;
        }
        let relative = ["action.yml", "action.yaml"]
            .iter()
            .map(|file| format!("{action}/{file}"))
            .find(|relative| root().join(relative).is_file())
            .unwrap_or_else(|| panic!("`./{action}` has no action.yml"));
        pending.extend(local_actions(&read(&relative)));
        files.push((relative.clone(), relative));
    }
    files
}

/// The release workflow runs only for `v*` tags, holds no permission by
/// default, refuses a tag that is not a version, is off the default branch or
/// differs from the workspace version, and signs and attests by digest.
fn release_violations(text: &str) -> Vec<String> {
    let mut violations = Vec::new();
    let trigger = text
        .split("\non:\n")
        .nth(1)
        .and_then(|rest| rest.split("\n\n").next());
    if trigger.map(str::trim) != Some("push:\n    tags: [\"v*\"]") {
        violations.push("release must trigger only on push of v* tags".to_owned());
    }
    if !text.lines().any(|line| line == "permissions: {}") {
        violations.push("release must default to no permissions".to_owned());
    }
    for required in [
        "cosign sign --yes \"${IMAGE}@${DIGEST}\"",
        "cosign verify \"${IMAGE}@${DIGEST}\"",
        "provenance: mode=max",
        "sbom: generator=",
        "git merge-base --is-ancestor",
        // The tag is the workspace version (Cargo.toml [workspace.package]).
        "if [ \"v$version\" != \"$GITHUB_REF_NAME\" ]; then",
    ] {
        if !text.contains(required) {
            violations.push(format!("release.yml: missing `{required}`"));
        }
    }
    violations
}

/// Days since 1970-01-01 of a proleptic Gregorian `YYYY-MM-DD`
/// (H. Hinnant's `days_from_civil`).
fn days_from_civil(date: &str) -> Option<i64> {
    let mut parts = date.splitn(3, '-');
    let (year, month, day) = (parts.next()?, parts.next()?, parts.next()?);
    if year.len() != 4 || month.len() != 2 || day.len() != 2 {
        return None;
    }
    let (year, month, day): (i64, i64, i64) =
        (year.parse().ok()?, month.parse().ok()?, day.parse().ok()?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    Some(era * 146_097 + day_of_era - 719_468)
}

fn today() -> i64 {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("the clock is after 1970")
        .as_secs();
    i64::try_from(seconds / 86_400).expect("days fit in i64")
}

/// deny.toml exceptions are reviewed records with an end: every
/// `expires YYYY-MM-DD` of a table (the header's example is not one) is a
/// valid date that has not passed, and the NCSA license exception of
/// libfuzzer-sys carries one. On the day after its expiry `cargo test`
/// fails until the exception is re-reviewed or removed.
fn deny_violations(deny: &str, today: i64) -> Vec<String> {
    let mut violations = Vec::new();
    let tables = deny.find("\n[").map_or("", |start| &deny[start..]);
    for (position, _) in tables.match_indices("expires") {
        let date: String = tables[position + "expires".len()..]
            .trim_start_matches([':', ' '])
            .chars()
            .take(10)
            .collect();
        match days_from_civil(&date) {
            None => violations.push(format!("deny.toml: `expires {date}` is not a YYYY-MM-DD date")),
            Some(expiry) if today > expiry => violations.push(format!(
                "deny.toml: the exception that expires {date} has expired: re-review it (new date) or remove it"
            )),
            Some(_) => {}
        }
    }
    let ncsa = tables
        .split("[[licenses.exceptions]]")
        .find(|table| table.contains("crate = \"libfuzzer-sys"));
    if !ncsa.is_some_and(|table| table.contains("expires")) {
        violations.push(
            "deny.toml: the libfuzzer-sys NCSA exception must name its expiry date".to_owned(),
        );
    }
    violations
}

/// CI measures line coverage and fails below the committed floor
/// (docs/test-strategy.md, "Coverage"): `.coverage-floor` is a percentage,
/// and ci.yml runs scripts/coverage.sh with a version-pinned cargo-llvm-cov.
fn coverage_violations(ci: &str, floor: &str) -> Vec<String> {
    let mut violations = Vec::new();
    let valid = floor
        .trim()
        .parse::<f64>()
        .is_ok_and(|floor| (0.0..=100.0).contains(&floor));
    if !valid {
        violations.push(format!(
            ".coverage-floor: `{}` is not a percentage",
            floor.trim()
        ));
    }
    for required in ["tool: cargo-llvm-cov", "run: scripts/coverage.sh"] {
        if !ci.contains(required) {
            violations.push(format!("ci.yml: the coverage job lost `{required}`"));
        }
    }
    violations
}

fn assert_clean(violations: &[String]) {
    assert!(violations.is_empty(), "\n{}", violations.join("\n"));
}

#[test]
fn actions_are_pinned_by_commit_with_their_version_in_a_comment() {
    for (name, relative) in workflows_and_local_actions() {
        assert_clean(&pin_violations(&name, &read(&relative)));
    }
}

const APP_CI_TEMPLATE: &str = "crates/forge-cli/templates/app/github/workflows/ci.yml.tmpl";

/// The gated files as (name, text): the workflows, the local actions they
/// reach and the generated-app CI template (markers blanked line-aligned).
fn gated_files() -> Vec<(String, String)> {
    let mut files: Vec<(String, String)> = workflows_and_local_actions()
        .into_iter()
        .map(|(name, relative)| (name, read(&relative)))
        .collect();
    files.push((
        APP_CI_TEMPLATE.to_owned(),
        blank_template_markers(&read(APP_CI_TEMPLATE)),
    ));
    files
}

#[test]
fn a_yaml_parser_reads_the_gated_files_as_the_gate_does() {
    for (name, text) in gated_files() {
        assert_clean(&yaml_subset_violations(&name, &text));
        assert_clean(&parser_violations(&name, &text));
        assert_clean(&tree_violations(&name, &text, name == "release.yml"));
    }
}

#[test]
fn only_allowlisted_actions_are_used() {
    let mut used = BTreeSet::new();
    for (name, text) in gated_files() {
        assert_clean(&allowlist_violations(&name, &text));
        assert_clean(&yaml_subset_violations(&name, &text));
        for line in text.lines() {
            for reference in key_values(split_comment(line).0, "uses") {
                used.extend(action_repository(reference));
            }
        }
    }
    // The list holds what is in use, nothing more.
    for allowed in ALLOWED_ACTIONS {
        assert!(
            used.contains(allowed),
            "{allowed} is allowlisted but unused"
        );
    }
}

/// What the gate refuses about a `uses:` line: an unlisted action, or a key
/// it cannot read.
fn uses_violations(text: &str) -> Vec<String> {
    let mut violations = allowlist_violations("t.yml", text);
    violations.extend(yaml_subset_violations("t.yml", &dedent(text)));
    violations
}

/// A fragment moved left so that its least indented line starts at column
/// 0, as a document root must.
fn dedent(text: &str) -> String {
    let margin = text
        .split('\n')
        .filter(|line| !line.trim().is_empty())
        .map(indentation)
        .min()
        .unwrap_or(0);
    text.split('\n')
        .map(|line| line.get(margin..).unwrap_or(line.trim_start()))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn the_allowlist_rejects_unlisted_actions_however_pinned() {
    for rejected in [
        "      - uses: taiki-e/install-action@SHA # v2.62.1",
        "      - uses: baptiste0928/cargo-install@SHA # v3.3.2",
        "      - { uses: dtolnay/rust-toolchain@SHA } # v1",
        "      - uses: actions-rs/cargo@SHA # v1.0.3",
        "      - uses: actions/checkout-evil@SHA # v7.0.1",
        "      - uses: docker://alpine@sha256:DIGEST",
        "      - uses: checkout@SHA # v7",
        "      - uses: actions/checkout/../../taiki-e/install-action@SHA # v2",
        "      - uses: actions/checkout/./x@SHA # v7",
        "      - uses: actions/checkout//x@SHA # v7",
        // YAML spellings of the key that a line scanner would not see.
        "      - \"u\\x73es\": taiki-e/install-action@SHA # v2",
        "      - \"use\\s\": taiki-e/install-action@SHA # v2",
        "      - ? uses\n        : taiki-e/install-action@SHA # v2",
        "    steps: [uses: taiki-e/install-action@SHA] # v2",
        "      - &k uses: taiki-e/install-action@SHA # v2",
        "      - *k : taiki-e/install-action@SHA # v2",
        "      - !!str uses: taiki-e/install-action@SHA # v2",
        "    steps: [{name: x, !!str uses: taiki-e/install-action@SHA}] # v2",
    ] {
        let text = snippet(rejected);
        assert!(!uses_violations(&text).is_empty(), "accepted: {text}");
    }
    for accepted in [
        "      - uses: actions/checkout@SHA # v7.0.1",
        "      - uses: Actions/Checkout@SHA # v7.0.1",
        "      - uses: swatinem/RUST-CACHE@SHA # v2.9.2",
        "      - uses: docker/build-push-action/subdir@SHA # v7",
        "      - uses: ./.github/actions/cargo-tool",
        "    steps: [uses: actions/checkout@SHA] # v7.0.1",
        // Inside a block scalar, `*`, `&` and `!` are shell, not YAML.
        "      - run: |\n          case \"$x\" in\n            *) echo \"!\" & wait ;;\n          esac\n      - uses: actions/checkout@SHA # v7.0.1",
        "    branches: [main]\n    tags: [\"v*\"]",
    ] {
        assert_clean(&uses_violations(&snippet(accepted)));
    }
}

#[test]
fn the_yaml_subset_refuses_what_a_line_scanner_could_misread() {
    let decoy = "      - uses: actions/checkout@SHA # v7.0.1";
    for rejected in [
        // Hidden line breaks: one line here, two for a YAML parser.
        format!("{decoy}\r      - uses: evil/tool@main\n"),
        format!("{decoy}\u{85}      - uses: evil/tool@main\n"),
        format!("{decoy}\u{2028}      - uses: evil/tool@main\n"),
        format!("{decoy}\u{2029}      - uses: evil/tool@main\n"),
        format!("{decoy}\n\u{feff}      - uses: evil/tool@main\n"),
        // Anchors, aliases, tags, explicit and merge keys anywhere.
        "    steps: !!seq [ {\"u\\x73es\": evil/tool@main} ]\n".to_owned(),
        "    steps: &s [ {uses: actions/checkout@SHA} ]\n".to_owned(),
        "    steps: *s\n".to_owned(),
        "      - <<: *base\n".to_owned(),
        "      - name: x\n        with: !!map {a: b}\n".to_owned(),
        "      - with: {a: &x b}\n".to_owned(),
        "      - ? uses\n        : evil/tool@main\n".to_owned(),
        // Flow collections that span lines, unclosed quotes.
        "    steps: [\n      {uses: evil/tool@main} ]\n".to_owned(),
        "    steps: [ {name: x},\n      uses: evil/tool@main ]\n".to_owned(),
        "      - name: \"x\n        uses: evil/tool@main\"\n".to_owned(),
        "      - \"u\\x73es\": evil/tool@main\n".to_owned(),
    ] {
        let text = snippet(&rejected);
        assert!(
            !yaml_subset_violations("t.yml", &dedent(&text)).is_empty(),
            "accepted: {text:?}"
        );
    }
    for accepted in [
        "\u{feff}name: CI\non:\n  push:\n    branches: [main]\n    tags: [\"v*\"]\n",
        "jobs:\n  x:\n    if: ${{ !cancelled() }}\n    steps:\n      - run: |\n          case \"$x\" in\n            *) echo \"!\" & wait ;;\n          esac\n          # deeper comment\n      - uses: actions/checkout@SHA # v7.0.1\n",
        "      - run: echo \"a #b\" && echo it's\n      - name: 'it''s'\n        with: {a: b, c: [d, e]}\n",
        "    if: >-\n      github.event_name == 'push'\n      && !cancelled()\n",
        "on:\r\n  push:\r\n",
    ] {
        assert_clean(&yaml_subset_violations(
            "t.yml",
            &dedent(&snippet(accepted)),
        ));
    }
    // The scanner reads every node of the real files, deep ones included:
    // an anchor or a tag anywhere in them is found.
    let ci = read(".github/workflows/ci.yml");
    for (from, to) in [
        (
            "persist-credentials: false",
            "persist-credentials: !!bool false",
        ),
        ("runs-on: ubuntu-latest", "runs-on: &runner ubuntu-latest"),
        ("timeout-minutes: 60", "timeout-minutes: *minutes"),
    ] {
        let last = ci.rfind(from).expect("present");
        let changed = format!("{}{to}{}", &ci[..last], &ci[last + from.len()..]);
        assert!(
            !yaml_subset_violations("ci.yml", &changed).is_empty(),
            "the last `{from}` of ci.yml was not read"
        );
    }
}

#[test]
fn the_parser_check_rejects_what_the_scanner_misreads() {
    let checkout = "actions/checkout@SHA # v7.0.1";
    for rejected in [
        // R2: a quoted `#` is no comment.
        "jobs:\n  a:\n    steps:\n      - {name: 'build #1', uses: evil/act@main}\n".to_owned(),
        // R3: flow nesting after a quoted key with no blank.
        "jobs:\n  b: {\"runs-on\":[ubuntu-latest], \"steps\":[{\"u\\x73es\": evil/act@main}]}\n"
            .to_owned(),
        "jobs:\n  b: {runs-on:[x], steps: [{uses: evil/act@main}]}\n".to_owned(),
        // R4: document markers, directives and an indented root.
        "---\n  jobs:\n    a:\n      steps:\n        - uses: evil/act@main\n".to_owned(),
        "--- # c\n  jobs: {}\n".to_owned(),
        "%YAML 1.2\n---\njobs: {}\n".to_owned(),
        "jobs: {}\n...\n  more: x\n".to_owned(),
        // A plain scalar continued on deeper lines.
        "jobs:\n  a:\n    if: github.event_name == 'push'\n      && !cancelled()\n".to_owned(),
        // A decoy in a block scalar cannot stand in for a hidden key.
        format!(
            "jobs:\n  a:\n    steps:\n      - run: |\n          uses: {checkout}\n      - \"u\\x73es\": evil/act@main\n"
        ),
    ] {
        let text = snippet(&rejected);
        let mut violations = yaml_subset_violations("t.yml", &text);
        violations.extend(parser_violations("t.yml", &text));
        violations.extend(allowlist_violations("t.yml", &text));
        assert!(!violations.is_empty(), "accepted: {text:?}");
    }
    for accepted in [
        format!("jobs:\n  a:\n    steps:\n      - uses: {checkout}\n        with:\n          persist-credentials: false\n      - {{uses: actions/checkout@SHA, with: {{persist-credentials: false}}}} # v7.0.1\n      - run: |\n          uses: x\n"),
        "---\njobs:\n  a:\n    if: >-\n      github.event_name == 'push'\n      && !cancelled()\n...\n".to_owned(),
    ] {
        let text = snippet(&accepted);
        assert_clean(&yaml_subset_violations("t.yml", &text));
        assert_clean(&parser_violations("t.yml", &text));
    }
}

#[test]
fn the_tree_rules_read_checkouts_and_permissions_as_yaml_does() {
    for rejected in [
        // X1 (a): a name that spells the setting.
        "steps:\n  - name: \"x, persist-credentials: false\"\n    uses: actions/checkout@SHA # v7.0.1\n",
        // X1 (b): the setting in block-scalar text.
        "steps:\n  - uses: actions/checkout@SHA # v7.0.1\n    with:\n      fetch-depth: 0\n  - run: |\n      persist-credentials: false\n",
        // X1 (c): one flow line, two steps, one setting.
        "steps: [{uses: actions/checkout@SHA, with: {persist-credentials: false}}, {uses: Actions/Checkout@SHA}]\n",
        "steps:\n  - uses: actions/checkout@SHA # v7.0.1\n    with:\n      persist-credentials: \"true\"\n",
        // X1 (d): a quoted `#` hiding a write scope.
        "permissions: {a: '#', contents: write}\n",
        "permissions: write-all\n",
        "jobs:\n  a:\n    permissions:\n      checks: write\n",
    ] {
        let text = snippet(rejected);
        assert!(
            !tree_violations("t.yml", &text, false).is_empty(),
            "accepted: {text}"
        );
    }
    let release = "permissions: {}\njobs:\n  a:\n    permissions:\n      contents: read\n      packages: write\n      id-token: write\n";
    assert_clean(&tree_violations("release.yml", release, true));
    assert!(!tree_violations("ci.yml", release, false).is_empty());
}

/// The merged reading of the template (every branch at once) refuses these
/// injections too. It is not the guarantee: a line can be `run: |` text in
/// one render and a step in another, which only the rendered gate in
/// forge-cli (`a_branch_cannot_hide_a_step_from_the_rendered_gate`) reads.
#[test]
fn the_template_is_read_in_every_branch() {
    let template = read(APP_CI_TEMPLATE);
    let point = "%%if postgresql%%\n      - run: cargo test";
    assert!(template.contains(point), "the mutation point moved");
    // R1: a `%%...%%` marker line must not hide the steps below it.
    for injected in [
        "      - \"u\\x73es\": taiki-e/install-action@main\n",
        "      - !!str uses: evil/act@main\n",
        "      - uses: evil/act@main\n",
    ] {
        let mutated = template.replacen(
            point,
            &format!("%%if postgresql%%\n{injected}      - run: cargo test"),
            1,
        );
        let text = blank_template_markers(&mutated);
        let mut violations = yaml_subset_violations("ci.yml.tmpl", &text);
        violations.extend(parser_violations("ci.yml.tmpl", &text));
        violations.extend(allowlist_violations("ci.yml.tmpl", &text));
        assert!(!violations.is_empty(), "accepted: {injected}");
    }
}

/// F4: a reader that takes a key's first value and one that takes its last
/// would disagree, so no mapping repeats a key, in any case.
#[test]
fn the_parser_refuses_duplicate_keys_in_any_case() {
    for rejected in [
        "steps:\n  - uses: actions/checkout@SHA # v7.0.1\n    with:\n      persist-credentials: false\n      Persist-Credentials: true\n",
        "steps:\n  - uses: actions/checkout@SHA # v7.0.1\n    USES: evil/act@main\n",
        "permissions:\n  contents: read\npermissions:\n  contents: write\n",
        "jobs:\n  a:\n    env: {A: x, a: y}\n",
    ] {
        let text = snippet(rejected);
        let error = parse_yaml(&text).err().unwrap_or_default();
        assert!(error.contains("appears twice"), "accepted: {text}");
        assert!(
            !tree_violations("t.yml", &text, false).is_empty(),
            "the tree rules accepted: {text}"
        );
        assert!(
            !parser_violations("t.yml", &text).is_empty(),
            "the parser check accepted: {text}"
        );
    }
    // The same key in two different mappings is fine.
    let accepted = "jobs:\n  a:\n    env: {A: x}\n  b:\n    env: {A: y}\n";
    assert!(parse_yaml(accepted).is_ok());
}

/// No gated file sets a variable that redirects where cargo or rustup fetch
/// from, or writes `$GITHUB_ENV`.
#[test]
fn gated_files_set_no_redirecting_environment() {
    for (name, text) in gated_files() {
        assert_clean(&environment_violations(&name, &text));
    }
}

/// F3: every redirecting variable is refused in an env: block at any level,
/// in any case, and in run text; a `$GITHUB_ENV` write is refused whatever
/// it sets.
#[test]
fn the_environment_rule_rejects_redirecting_variables() {
    for rejected in [
        "env:\n  CARGO_HOME: /tmp/ci-home\n",
        "jobs:\n  a:\n    env:\n      cargo_home: /tmp/ci-home\n",
        "steps:\n  - run: cargo build\n    env:\n      CARGO_REGISTRIES_MIRROR_INDEX: sparse+https://evil.example/\n",
        "env: {CARGO_REGISTRY_DEFAULT: mirror}\n",
        "env:\n  CARGO_SOURCE_CRATES_IO_REPLACE_WITH: mirror\n",
        "env:\n  CARGO_INSTALL_ROOT: /tmp/bin\n",
        "env:\n  CARGO_NET_GIT_FETCH_WITH_CLI: \"true\"\n",
        "env:\n  RUSTUP_DIST_SERVER: https://evil.example\n",
        "env:\n  RUSTUP_UPDATE_ROOT: https://evil.example/rustup\n",
        "steps:\n  - run: export CARGO_HOME=/tmp/ci-home\n",
        "steps:\n  - run: echo \"CARGO_REGISTRIES_X_INDEX=https://evil.example\" >> \"$GITHUB_ENV\"\n",
        "steps:\n  - run: echo \"A=b\" >> $GITHUB_ENV\n",
        "steps:\n  - run: |\n      printf '%s\\n' \"X=1\" >> \"${GITHUB_ENV}\"\n",
    ] {
        let text = snippet(rejected);
        assert!(
            !environment_violations("t.yml", &text).is_empty(),
            "accepted: {text}"
        );
    }
    for accepted in [
        "env:\n  CARGO_TERM_COLOR: always\n  RUSTFLAGS: -D warnings\n",
        "env:\n  FORGE_CARGO_HOMEPAGE: x\n",
        "steps:\n  - run: echo \"$HOME/bin\" >> \"$GITHUB_PATH\"\n",
    ] {
        assert_clean(&environment_violations("t.yml", &snippet(accepted)));
    }
}

#[test]
fn workflow_containers_are_pinned_by_digest() {
    for (name, relative) in workflows_and_local_actions() {
        assert_clean(&container_violations(&name, &read(&relative)));
    }
}

#[test]
fn checkouts_keep_no_credentials() {
    for (name, relative) in workflows_and_local_actions() {
        assert_clean(&checkout_violations(&name, &read(&relative)));
    }
}

#[test]
fn tool_binaries_are_installed_by_exact_version() {
    let files = workflows_and_local_actions();
    // The traversal reaches the local action the workflows install through.
    assert!(
        files
            .iter()
            .any(|(_, relative)| relative == ".github/actions/cargo-tool/action.yml"),
        "the local actions were not followed: {files:?}"
    );
    for (_, relative) in files {
        assert_clean(&tool_install_violations(&relative, &read(&relative)));
    }
}

#[test]
fn permissions_are_least_privilege_and_secrets_stay_out_of_pull_requests() {
    for (name, relative) in workflows() {
        let text = read(&relative);
        let release = name == "release.yml";
        assert_clean(&permission_violations(&name, &text, release));
        assert_clean(&secret_violations(&name, &text, release));
    }
    // A local action runs with its caller's permissions; it reads no secret.
    for (name, relative) in workflows_and_local_actions() {
        if relative.starts_with(".github/workflows/") {
            continue;
        }
        let text = read(&relative);
        assert_clean(&secret_violations(&name, &text, false));
        let scopes: Vec<String> = text
            .lines()
            .flat_map(|line| write_scopes(split_comment(line).0))
            .collect();
        assert!(scopes.is_empty(), "{name}: {scopes:?}");
    }
}

/// The `FROM` images of a Dockerfile (template), stage names excluded.
fn dockerfile_bases(text: &str) -> Vec<String> {
    let mut stages = Vec::new();
    let mut bases = Vec::new();
    for line in text.lines() {
        let words: Vec<&str> = line.split_whitespace().collect();
        if !words
            .first()
            .is_some_and(|word| word.eq_ignore_ascii_case("FROM"))
        {
            continue;
        }
        let image = words.get(1).copied().unwrap_or_default();
        if !stages.iter().any(|stage| stage == image) {
            bases.push(image.to_owned());
        }
        if let Some(position) = words
            .iter()
            .position(|word| word.eq_ignore_ascii_case("AS"))
        {
            stages.extend(words.get(position + 1).map(|stage| (*stage).to_owned()));
        }
    }
    bases
}

/// What `forge new` generates runs under the same rules: its CI pins every
/// action by commit and every service by digest and keeps no credentials,
/// and its Dockerfile pins both base images by digest, the same digests as
/// forge-proxy's Dockerfile (which the gate ties to rust-toolchain.toml).
#[test]
fn generated_application_ci_and_dockerfile_are_pinned() {
    let name = "templates/app/github/workflows/ci.yml.tmpl";
    let ci = blank_template_markers(&read(APP_CI_TEMPLATE));
    assert_clean(&pin_violations(name, &ci));
    assert_clean(&checkout_violations(name, &ci));
    assert_clean(&container_violations(name, &ci));
    assert_clean(&permission_violations(name, &ci, false));
    assert_clean(&secret_violations(name, &ci, false));
    assert_clean(&allowlist_violations(name, &ci));
    assert_clean(&tool_install_violations(name, &ci));
    assert!(
        ci.contains("FORGE_TEST_DATABASE_URL") && ci.contains("--include-ignored"),
        "{name}: a PostgreSQL application's CI runs its database tests"
    );

    let proxy = dockerfile_bases(&read("crates/forge-proxy/Dockerfile"));
    for file in [
        "crates/forge-cli/templates/app/Dockerfile.tmpl",
        "examples/hello-forge/Dockerfile",
    ] {
        let bases = dockerfile_bases(&read(file));
        assert_eq!(
            bases.len(),
            2,
            "{file}: the build and runtime base images: {bases:?}"
        );
        for image in &bases {
            assert!(
                has_digest(image),
                "{file}: `{image}` must be pinned by digest"
            );
            let digest = image.rsplit_once('@').map(|(_, digest)| digest);
            assert!(
                proxy
                    .iter()
                    .any(|pinned| pinned.rsplit_once('@').map(|(_, digest)| digest) == digest),
                "{file}: `{image}` must use the digest of crates/forge-proxy/Dockerfile ({proxy:?})"
            );
        }
    }

    // The generated Compose file runs the database images CI tests with,
    // pinned to the same digests; only the application's own image is local.
    let framework_ci = read(".github/workflows/ci.yml");
    let compose = read("crates/forge-cli/templates/app/compose.yaml.tmpl");
    let images: Vec<&str> = compose
        .lines()
        .filter_map(|line| split_comment(line).0.trim().strip_prefix("image:"))
        .map(|image| image.trim().trim_matches(['"', '\'']))
        .filter(|image| !image.starts_with("%%APP_NAME%%"))
        .collect();
    assert!(images.len() >= 3, "compose.yaml.tmpl: {images:?}");
    for image in images {
        assert!(
            has_digest(image) && framework_ci.contains(&format!("image: {image}\n")),
            "compose.yaml.tmpl: `{image}` must be pinned by digest, the same pin as .github/workflows/ci.yml"
        );
    }
}

/// Every crate takes the workspace version, so the release check (the tag is
/// the `[workspace.package]` version) covers every crate a tag publishes.
#[test]
fn every_workspace_crate_takes_the_workspace_version() {
    let workspace = read("Cargo.toml");
    let members: Vec<&str> = workspace
        .lines()
        .skip_while(|line| !line.starts_with("members"))
        .skip(1)
        .take_while(|line| !line.starts_with(']'))
        .map(|line| line.trim().trim_end_matches(',').trim_matches('"'))
        .collect();
    assert!(members.len() > 20, "{members:?}");
    for member in members {
        let manifest = read(&format!("{member}/Cargo.toml"));
        assert!(
            manifest
                .lines()
                .any(|line| line.trim() == "version.workspace = true")
                && !manifest
                    .lines()
                    .any(|line| line.trim().starts_with("version = ")),
            "{member}/Cargo.toml must say `version.workspace = true`"
        );
    }
}

#[test]
fn the_release_workflow_runs_only_for_version_tags_and_signs_by_digest() {
    assert_clean(&release_violations(&read(".github/workflows/release.yml")));
}

#[test]
fn deny_exceptions_have_not_expired() {
    assert_clean(&deny_violations(&read("deny.toml"), today()));
}

#[test]
fn coverage_is_measured_and_gated_in_ci() {
    assert_clean(&coverage_violations(
        &read(".github/workflows/ci.yml"),
        &read(".coverage-floor"),
    ));
}

const SHA: &str = "3d3c42e5aac5ba805825da76410c181273ba90b1";
const DIGEST: &str = "74935e72241653ca55e0414067e6d8763aceb8a810eb51b452253ec3dcfc4336";

/// Expands the `SHA` and `DIGEST` placeholders of a snippet.
fn snippet(text: &str) -> String {
    text.replace("SHA", SHA).replace("DIGEST", DIGEST)
}

#[test]
fn the_pin_rule_rejects_every_known_bypass() {
    for rejected in [
        "      - uses: actions/checkout@v7 # v7.0.1",
        "      - uses: actions/checkout@SHA",
        "      - uses: actions/checkout@SHA # latest",
        "      - uses: actions/checkout@SHA\t# main",
        "      - { uses: actions/checkout@v7, with: { persist-credentials: false } } # v7.0.1",
        "      - { \"uses\": owner/repo@main }",
        "    steps: [{ name: x, uses: owner/repo@main }]",
        "      - uses: &pin actions/checkout@SHA # v7.0.1",
        "      - uses: *pin",
        "      - 'uses' : owner/repo@main",
        "      - uses: docker://alpine:3.20",
        "      - uses: docker://alpine@sha256:abc",
    ] {
        let text = snippet(rejected);
        assert!(
            !pin_violations("t.yml", &text).is_empty(),
            "accepted: {text}"
        );
    }
    for accepted in [
        "      - uses: actions/checkout@SHA # v7.0.1",
        "      - uses: actions/checkout@SHA\t# v7",
        "      - { uses: actions/checkout@SHA, with: { persist-credentials: false } } # v7.0.1",
        "      - \"uses\": \"owner/repo/path@SHA\" # v1.2",
        "      - uses: docker://alpine@sha256:DIGEST",
        "      - uses: ./.github/actions/cargo-tool",
        "        run: echo uses: anything",
    ] {
        let text = snippet(accepted);
        assert_clean(&pin_violations("t.yml", &text));
    }
}

#[test]
fn the_checkout_rule_rejects_steps_that_keep_credentials() {
    for rejected in [
        "      - uses: actions/checkout@SHA # v7.0.1\n",
        "      - uses: Actions/Checkout@SHA # v7.0.1\n        with:\n          fetch-depth: 0\n",
        "      - { uses: ACTIONS/checkout@SHA } # v7.0.1\n",
        "      - uses: actions/checkout@SHA # v7.0.1\n        with:\n          persist-credentials: true\n",
        // The setting belongs to another step: counting would accept it.
        "      - uses: actions/checkout@SHA # v7.0.1\n      - uses: owner/repo@SHA # v1\n        with:\n          persist-credentials: false\n",
        "      - name: checkout\n        uses: actions/checkout@SHA # v7.0.1\n      - with:\n          persist-credentials: false\n",
    ] {
        let text = snippet(rejected);
        assert!(
            !checkout_violations("t.yml", &text).is_empty(),
            "accepted: {text}"
        );
    }
    for accepted in [
        "      - uses: actions/checkout@SHA # v7.0.1\n        with:\n          persist-credentials: false\n",
        "      - name: checkout\n        uses: Actions/Checkout@SHA # v7.0.1\n        with:\n          fetch-depth: 0\n          persist-credentials: \"false\"\n      - run: true\n",
        "      - { uses: actions/checkout@SHA, with: { persist-credentials: false } } # v7.0.1\n",
    ] {
        let text = snippet(accepted);
        assert_clean(&checkout_violations("t.yml", &text));
    }
}

#[test]
fn the_container_rule_reads_only_containers() {
    for rejected in [
        "    services:\n      db:\n        image: postgres:18\n",
        "    services:\n      db:\n        image: \"postgres:18\" # 18.6\n",
        "    container: node:24\n",
        "    container:\n      image: node:24\n      options: --cpus 1\n",
        "    services: { db: { image: postgres:18 } }\n",
        "    services:\n      db: { image: postgres:18, ports: [\"5432:5432\"] }\n",
        "    container: { image: node:24 }\n",
    ] {
        assert!(
            !container_violations("t.yml", rejected).is_empty(),
            "accepted: {rejected}"
        );
    }
    for accepted in [
        &snippet("    services:\n      db:\n        image: postgres:18@sha256:DIGEST\n")[..],
        &snippet("    container:\n      image: node@sha256:DIGEST\n")[..],
        // An action input and a matrix entry are not containers.
        "    steps:\n      - uses: owner/repo@x\n        with:\n          image: forge-proxy\n",
        "      - { uses: owner/repo@x, with: { image: forge-proxy } }\n",
        "      matrix:\n        include:\n          - image: forge-proxy\n            dockerfile: Dockerfile\n",
    ] {
        assert_clean(&container_violations("t.yml", accepted));
    }
}

#[test]
fn the_permission_rule_rejects_write_scopes_in_any_spelling() {
    let base = "permissions:\n  contents: read\n";
    for rejected in [
        "permissions: { contents: write }\n",
        "permissions:\n  contents: write\t# needed\n",
        "permissions:\n  \"contents\": 'write'\n",
        "permissions:\n  contents: read\njobs:\n  x:\n    permissions:\n      checks: write\n",
        "permissions: write-all\n",
        "permissions:\n  contents: read\non: pull_request_target\n",
        "jobs: {}\n",
    ] {
        assert!(
            !permission_violations("t.yml", rejected, false).is_empty(),
            "accepted: {rejected}"
        );
    }
    assert!(
        !permission_violations("t.yml", &format!("{base}    packages: write\n"), false).is_empty()
    );
    assert_clean(&permission_violations(
        "release.yml",
        "permissions: {}\njobs:\n  x:\n    permissions:\n      packages: write # push\n      id-token: write\n",
        true,
    ));
    assert_clean(&permission_violations("t.yml", base, false));
}

#[test]
fn the_secret_rule_rejects_every_spelling_outside_the_release_token() {
    for rejected in [
        "        env:\n          TOKEN: ${{ secrets.NPM_TOKEN }}\n",
        "        env:\n          TOKEN: ${{ SECRETS.GITHUB_TOKEN }}\n",
        "        env:\n          TOKEN: ${{ secrets . GITHUB_TOKEN }}\n",
        "    secrets:  inherit\n",
        "    secrets:\tinherit\n",
        "        run: echo '${{ toJSON(secrets) }}'\n",
        "        run: echo '${{ join(secrets, \",\") }}'\n",
        "        run: echo '${{ secrets[format(\"{0}\", inputs.name)] }}'\n",
        "        with: { all: secrets }\n",
    ] {
        assert!(
            !secret_violations("release.yml", rejected, true).is_empty(),
            "accepted: {rejected}"
        );
    }
    let token = "          password: ${{ secrets.GITHUB_TOKEN }}\n";
    assert_clean(&secret_violations("release.yml", token, true));
    assert!(!secret_violations("ci.yml", token, false).is_empty());
    assert_clean(&secret_violations(
        "ci.yml",
        "      # no secret here; my_secrets: x\n",
        false,
    ));
}

#[test]
fn the_tool_rule_rejects_installs_that_ignore_a_version_bump() {
    for rejected in [
        "        run: command -v cargo-fuzz >/dev/null || cargo install cargo-fuzz --version 0.13.2 --locked\n",
        "        run: cargo install cargo-fuzz --locked\n",
        "        run: cargo install cargo-fuzz --version 0.13.2\n",
        "        run: cargo  install cargo-fuzz --locked\n",
        "        run: cargo +nightly install cargo-fuzz --locked\n",
        "        run: cargo binstall cargo-fuzz --version 0.13.2 --locked\n",
        "        run: cargo-binstall cargo-fuzz\n",
        "        run: which cargo-fuzz || cargo install cargo-fuzz --version 0.13.2 --locked\n",
        "        run: hash cargo-fuzz 2>/dev/null || cargo install cargo-fuzz --version 0.13.2 --locked\n",
        "        run: |\n          if ! which cargo-fuzz; then\n            cargo install cargo-fuzz --version 0.13.2 --locked\n          fi\n",
        // A first, harmless cargo command must not hide the install.
        "        run: cargo fetch && cargo install cargo-fuzz\n",
        "        run: cargo fuzz --help >/dev/null || cargo install cargo-fuzz --version 0.13.2 --locked\n",
        "        run: cargo install a --version 1.2.3 --locked; cargo install b --locked\n",
        "        run: echo $(cargo install cargo-fuzz --locked)\n",
        "        run: ~/.cargo/bin/cargo install cargo-fuzz --locked\n",
        "        run: \"$HOME\"/.cargo/bin/cargo install cargo-fuzz --version ^0.13 --locked\n",
        "        run: cargo install cargo-fuzz --version ^0.13.2 --locked\n",
        "        run: cargo install cargo-fuzz --version '>=0.13.2' --locked\n",
        "        run: cargo install cargo-fuzz --version ~0.13.2 --locked\n",
        "        run: cargo install cargo-fuzz --version '*' --locked\n",
        "        run: cargo install cargo-fuzz --version=0.13 --locked\n",
        "        run: cargo install cargo-fuzz --vers 0.13.2.1 --locked\n",
        "        run: cargo install cargo-fuzz --version \"$VERSION\" --locked\n",
        "        run: cargo install cargo-fuzz --version 0.13.2 --locked --git https://example.com/fuzz\n",
        "        run: cargo install cargo-fuzz --version 0.13.2 --locked --path vendor/cargo-fuzz\n",
        "        run: cargo install cargo-fuzz --version 0.13.2 --locked --registry=mirror\n",
        "        run: cargo install cargo-fuzz --version 0.13.2 --locked --index sparse+https://example.com/\n",
        "        run: |\n          cargo \\\n            install cargo-fuzz\n",
        "        run: cargo \"install\" cargo-fuzz --version 0.13.2 --locked\n",
        "        run: |\n          if [ ! -x ~/.cargo/bin/cargo-fuzz ]; then\n            cargo install cargo-fuzz --version 0.13.2 --locked\n          fi\n",
        "        run: test -f \"$HOME/.cargo/bin/cargo-fuzz\" || true\n",
        "      - uses: ./.github/actions/cargo-tool\n        with:\n          tool: cargo-fuzz\n          version: 0.13\n",
        "      - uses: ./.github/actions/cargo-tool\n        with:\n          tool: cargo-fuzz\n          version: ${{ inputs.version }}\n",
        "      - uses: ./.github/actions/cargo-tool\n        with:\n          tool: cargo-fuzz\n",
        // Every install outside the two reviewed sites, however spelled.
        "        run: cargo install cargo-auditable --version 0.7.7 --locked\n",
        "        run: cargo -q install x --version 1.2.3 --locked\n",
        "        run: cargo --locked install x --version 1.2.3\n",
        "        run: cargo --config k=v install x --version 1.2.3 --locked\n",
        "        run: cargo -Z unstable-options install x --version 1.2.3 --locked\n",
        "        run: cargo -C /tmp install x --version 1.2.3 --locked\n",
        "        run: cargo +stable -v install x --version 1.2.3 --locked\n",
        "        run: |\n          if ! cargo fuzz --version >/dev/null 2>&1; then\n            cargo install cargo-fuzz --version 0.13.2 --locked\n          fi\n",
        "        run: if cargo fuzz --version; then :; else cargo install cargo-fuzz --version 0.13.2 --locked; fi\n",
        "        run: if ! type cargo-fuzz; then echo missing; fi\n",
        "        run: '[ ! -s ~/.cargo/bin/cargo-fuzz ] && echo stale'\n",
        "        run: |\n          cargo install x --version 1.2.3 --locked \\\n            --git https://example.com/x\n",
        "        run: >-\n          cargo\n          install x --version 1.2.3 --locked\n",
        "        run: $CARGO install x --version 1.2.3 --locked\n",
        "        run: ${CARGO:-cargo} install x --version 1.2.3 --locked\n",
        "        run: CARGO_ALIAS_I=install cargo i x\n",
        "        run: c=cargo; $c install x --version 1.2.3 --locked\n",
        "        run: ${c} install x\n",
        "        run: \"$c\" install x\n",
        "        run: cargo $'\\x69nstall' x\n",
        // A `#` inside shell quotes is no comment; a continuation inside a
        // word joins without a space.
        "        run: |\n          echo \" #\"; cargo install evil\n",
        "        run: echo \"a #\" && cargo install evil\n",
        "        run: |\n          c\\\n          argo install evil\n",
        "        run: |\n          cargo inst\\\n          all evil\n",
        "        run: ca\"rgo\" install x --version 1.2.3 --locked\n",
        "        run: c\\\\argo inst\\\\all x --version 1.2.3 --locked\n",
        "        run: cargo 'bin'stall x\n",
        "        env:\n          CARGO_ALIAS_GET: install\n",
    ] {
        assert!(
            !tool_install_violations(".github/workflows/ci.yml", rejected).is_empty(),
            "accepted: {rejected}"
        );
    }
    for accepted in [
        "        run: cargo --version && cargo test --locked\n",
        "      - name: Install cargo-fuzz\n",
        "        run: rustup toolchain install nightly --profile minimal\n",
        "        run: |\n          cargo --version\n          sudo apt-get install -y openssh-server\n",
        "      - uses: ./.github/actions/cargo-tool\n        with:\n          tool: cargo-llvm-cov\n          version: 0.9.1\n      - run: true\n",
    ] {
        assert_clean(&tool_install_violations(
            ".github/workflows/ci.yml",
            accepted,
        ));
    }

    // The reviewed release step: exact version and --locked, alone.
    let step = |run: &str| {
        format!(
            "    steps:\n      - name: Install cargo-auditable\n        run: {run}\n      - name: Build\n        run: cargo build\n"
        )
    };
    let release = ".github/workflows/release.yml";
    assert_clean(&tool_install_violations(
        release,
        &step("cargo install cargo-auditable --version 0.7.7 --locked"),
    ));
    for run in [
        "cargo install cargo-auditable --locked",
        "cargo install cargo-auditable --version ^0.7 --locked",
        "cargo -q install cargo-auditable --version 0.7.7 --locked",
        "true || cargo install cargo-auditable --version 0.7.7 --locked",
        "cargo install cargo-auditable --version 0.7.7 --locked --git https://example.com/a",
        "cargo install cargo-auditable --version 0.7.7 --locked --g\\\\it https://example.com/a",
        "cargo install cargo-auditable --version 0.7.7 --locked \\\"--registry\\\" mirror",
        "cargo install cargo-auditable cargo-fuzz --version 0.7.7 --locked",
        "cargo install cargo-fuzz --version 0.13.2 --locked",
        "cargo install --version 0.7.7 --locked",
    ] {
        assert!(
            !tool_install_violations(release, &step(run)).is_empty(),
            "accepted: {run}"
        );
    }
    // Only that step: the same install in another release step is refused.
    let other = "    steps:\n      - name: Build\n        run: cargo install cargo-auditable --version 0.7.7 --locked\n";
    assert!(!tool_install_violations(release, other).is_empty());

    // Only the cargo-tool action (by exact path) may pass its caller's
    // checked $VERSION.
    let action = "      run: cargo install \"$TOOL\" --version \"$VERSION\" --locked --root \"$HOME/.forge-tools/$TOOL-$VERSION\"\n";
    assert_clean(&tool_install_violations(TOOL_ACTION, action));
    // ... spelled exactly so: unquoted or partly quoted is refused.
    for spelled in [
        "      run: cargo install \"$TOOL\" --version $VERSION --locked\n",
        "      run: cargo install \"$TOOL\" --version $VER\"SION\" --locked\n",
        "      run: cargo install \"$TOOL\" --version=\"$VERSION\" --locked\n",
    ] {
        assert!(
            !tool_install_violations(TOOL_ACTION, spelled).is_empty(),
            "accepted: {spelled}"
        );
    }
    for elsewhere in [
        ".github/workflows/ci.yml",
        "evil/.github/actions/cargo-tool/action.yml",
        ".github/actions/cargo-tool/action.yaml",
    ] {
        assert!(
            !tool_install_violations(elsewhere, action).is_empty(),
            "{elsewhere}"
        );
    }
}

/// Every `.cargo/config` or `.cargo/config.toml` in `directory` or below
/// it, following symbolic links to them (a linked `.cargo` counts). The
/// walk does not enter `skipped` (the root's build output and git
/// metadata) nor symbolic links to directories.
fn cargo_configs(directory: &Path, skipped: &[PathBuf]) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = ["config", "config.toml"]
        .iter()
        .map(|file| directory.join(".cargo").join(file))
        .filter(|path| path.exists())
        .collect();
    let Ok(entries) = fs::read_dir(directory) else {
        return found;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let is_directory = entry.file_type().is_ok_and(|kind| kind.is_dir());
        if is_directory && !skipped.contains(&path) {
            found.extend(cargo_configs(&path, skipped));
        }
    }
    found
}

/// No cargo configuration exists anywhere in the repository: cargo reads
/// every parent directory's `.cargo/config*`, and one could redirect crate
/// sources (`[source]`, `replace-with`, `[registries]`) or alias `install`
/// past the locked, crates.io-only installs the rules check. A future need
/// goes through review and changes this test.
#[test]
fn no_cargo_configuration_exists_in_the_repository() {
    let skipped = [root().join("target"), root().join(".git")];
    let found = cargo_configs(&root(), &skipped);
    assert!(found.is_empty(), "cargo configuration found: {found:?}");
    let scratch = tempfile::tempdir().expect("tempdir");
    for (relative, content) in [
        ("a/.cargo/config.toml", "alias = { i = \"install\" }\n"),
        (
            "b/c/.cargo/config",
            "[source.crates-io]\nreplace-with = \"m\"\n",
        ),
    ] {
        let path = scratch.path().join(relative);
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        fs::write(&path, content).expect("write");
    }
    // A symlinked .cargo, a config below a nested `target`, and one in the
    // skipped root build directory.
    fs::create_dir_all(scratch.path().join("elsewhere")).expect("mkdir");
    fs::write(scratch.path().join("elsewhere/config.toml"), "").expect("write");
    fs::create_dir_all(scratch.path().join("d")).expect("mkdir");
    std::os::unix::fs::symlink(
        scratch.path().join("elsewhere"),
        scratch.path().join("d/.cargo"),
    )
    .expect("symlink");
    fs::create_dir_all(scratch.path().join("e/target/.cargo")).expect("mkdir");
    fs::write(scratch.path().join("e/target/.cargo/config"), "").expect("write");
    fs::create_dir_all(scratch.path().join("target/.cargo")).expect("mkdir");
    fs::write(scratch.path().join("target/.cargo/config.toml"), "").expect("write");
    let mut found = cargo_configs(scratch.path(), &[scratch.path().join("target")]);
    found.sort();
    assert_eq!(
        found,
        vec![
            scratch.path().join("a/.cargo/config.toml"),
            scratch.path().join("b/c/.cargo/config"),
            scratch.path().join("d/.cargo/config.toml"),
            scratch.path().join("e/target/.cargo/config"),
        ]
    );
}

#[test]
fn the_release_rule_requires_the_workspace_version_check() {
    let text = read(".github/workflows/release.yml");
    let check = "if [ \"v$version\" != \"$GITHUB_REF_NAME\" ]; then";
    assert!(!release_violations(&text.replace(check, "if false; then")).is_empty());
    let manual = text.replace("on:\n  push:\n", "on:\n  workflow_dispatch:\n  push:\n");
    assert!(!release_violations(&manual).is_empty());
}

#[test]
fn the_deny_rule_fails_after_an_exception_expires() {
    let deny = read("deny.toml");
    let expiry = days_from_civil("2027-10-20").expect("date");
    assert!(
        deny.contains("expires 2027-10-20"),
        "update this test with the new NCSA expiry"
    );
    assert_clean(&deny_violations(&deny, expiry));
    assert!(!deny_violations(&deny, expiry + 1).is_empty());
    let malformed = deny.replace("expires 2027-10-20", "expires 2027-13-40");
    assert!(!deny_violations(&malformed, expiry - 400).is_empty());
    let unbounded = deny.replace("expires 2027-10-20", "revisit someday");
    assert!(!deny_violations(&unbounded, expiry - 400).is_empty());
    // The header's example is not an exception.
    assert!(deny_violations("#   expires: 2000-01-01\n[advisories]\nignore = []\n[[licenses.exceptions]]\n# expires 2100-01-01\ncrate = \"libfuzzer-sys@0.4.13\"\n", expiry).is_empty());
    assert_eq!(days_from_civil("1970-01-01"), Some(0));
    assert_eq!(days_from_civil("2000-03-01"), Some(11_017));
}

#[test]
fn the_coverage_rule_requires_a_floor_and_the_job() {
    let ci = read(".github/workflows/ci.yml");
    assert!(!coverage_violations(&ci, "ninety").is_empty());
    assert!(!coverage_violations(&ci, "101").is_empty());
    assert!(
        !coverage_violations(&ci.replace("run: scripts/coverage.sh", "run: true"), "0").is_empty()
    );
    assert_clean(&coverage_violations(&ci, "74.7\n"));
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
