//! The supply-chain gate (ADR 0043, docs/supply-chain.md): what CI and a
//! release execute is pinned, least-privileged and reachable only the way the
//! documentation says, and every fuzz target is wired in. The Rust twin of
//! Forge-Go's `supply_chain_test.go`; it reads the workflow, script and
//! Dockerfile text, so it needs no YAML parser and no network.
//!
//! Each rule is a function from a file's text to its violations, so the
//! `*_rejects_*` tests below can prove, on hand-written snippets, that every
//! known bypass is caught and that the accepted spellings stay accepted.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

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
    is_numeric_version(version, 1..=3)
}

/// `X`, `X.Y` or `X.Y.Z` (as many parts as `parts` allows), digits only.
fn is_numeric_version(version: &str, parts: std::ops::RangeInclusive<usize>) -> bool {
    let split: Vec<&str> = version.split('.').collect();
    parts.contains(&split.len())
        && split
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

/// A YAML line split into its code and its comment: a `#` that starts the
/// line or follows a space or a tab.
fn split_comment(line: &str) -> (&str, &str) {
    let bytes = line.as_bytes();
    let position = bytes.iter().enumerate().position(|(index, byte)| {
        *byte == b'#' && (index == 0 || matches!(bytes[index - 1], b' ' | b'\t'))
    });
    position.map_or((line, ""), |index| line.split_at(index))
}

fn indentation(line: &str) -> usize {
    line.len() - line.trim_start_matches([' ', '\t']).len()
}

/// The values of every `key` in the code of a line, unquoted: block style
/// (`key: x`, `- key: x`, `"key": x`, `key : x`) and flow style
/// (`- { key: x, other: y }`). A flow value ends at `,` or `}`.
fn key_values<'a>(code: &'a str, key: &str) -> Vec<&'a str> {
    let mut found = Vec::new();
    for (start, _) in code.match_indices(key) {
        // What precedes the key, past an opening quote and blanks: the start
        // of the line, a list dash, or a flow mapping's `{` or `,`.
        let before = code[..start].trim_end_matches(['"', '\'']).trim_end();
        if !(before.is_empty() || before.ends_with(['-', '{', ','])) {
            continue;
        }
        let after = code[start + key.len()..]
            .trim_start_matches(['"', '\''])
            .trim_start();
        let Some(value) = after.strip_prefix(':') else {
            continue;
        };
        let value = value.split([',', '}']).next().unwrap_or_default();
        found.push(value.trim().trim_matches(['"', '\'']));
    }
    found
}

/// The `uses: ./path` references of a workflow or action, without `./`.
fn local_actions(text: &str) -> Vec<String> {
    text.lines()
        .flat_map(|line| key_values(split_comment(line).0, "uses"))
        .filter_map(|reference| reference.strip_prefix("./"))
        .map(|path| path.trim_end_matches('/').to_owned())
        .collect()
}

/// Every action is `owner/repo[/path]@<40-hex commit> # vX.Y.Z`, every
/// `docker://` action is pinned by digest, and no `uses:` escapes the check
/// through flow style, an anchor or an alias. Local `./` actions are checked
/// as files of their own (`workflows_and_local_actions`).
fn pin_violations(name: &str, text: &str) -> Vec<String> {
    let mut violations = Vec::new();
    for (number, line) in text.lines().enumerate() {
        let (code, comment) = split_comment(line);
        for reference in key_values(code, "uses") {
            if reference.starts_with("./") {
                continue;
            }
            let pinned = match reference.strip_prefix("docker://") {
                Some(image) => has_digest(image),
                None => is_pinned_action(reference) && is_version_comment(comment),
            };
            if !pinned {
                violations.push(format!(
                    "{name}:{}: `{}` must be owner/repo@<40-hex commit> # vX.Y.Z, or docker://<image>@sha256:<digest>",
                    number + 1,
                    line.trim()
                ));
            }
        }
    }
    violations
}

/// The action repositories a workflow may use, compared without case
/// (GitHub resolves owner and name case-insensitively). Adding one is a
/// supply-chain decision: review the action's code, pin it by SHA, and add it
/// here and to the table of docs/supply-chain.md in the same change. Local
/// `./` actions are checked as files of their own; `docker://` actions are not
/// allowed.
const ALLOWED_ACTIONS: [&str; 10] = [
    "actions/cache",
    "actions/checkout",
    "actions/upload-artifact",
    "docker/build-push-action",
    "docker/login-action",
    "docker/setup-buildx-action",
    "embarkstudios/cargo-deny-action",
    "ossf/scorecard-action",
    "sigstore/cosign-installer",
    "swatinem/rust-cache",
];

/// Every `uses:` names an allowlisted repository (or a local `./` action),
/// whatever its pin: an installer action such as taiki-e/install-action
/// would otherwise put any tool on PATH, pinned or not.
fn allowlist_violations(name: &str, text: &str) -> Vec<String> {
    let mut violations = Vec::new();
    for (number, line) in text.lines().enumerate() {
        for reference in key_values(split_comment(line).0, "uses") {
            if reference.starts_with("./") {
                continue;
            }
            let action = reference
                .split_once('@')
                .map_or(reference, |(action, _)| action);
            let repository: Vec<&str> = action.splitn(3, '/').take(2).collect();
            let allowed = repository.len() == 2
                && ALLOWED_ACTIONS
                    .iter()
                    .any(|allowed| allowed.eq_ignore_ascii_case(&repository.join("/")));
            if !allowed {
                violations.push(format!(
                    "{name}:{}: `{reference}` is not an allowlisted action (ALLOWED_ACTIONS, docs/supply-chain.md)",
                    number + 1
                ));
            }
        }
    }
    violations
}

/// The lines of the step (list item) that line `index` belongs to: from its
/// `- ` to the next line indented no deeper than that dash.
fn step_lines<'a, 'b>(lines: &'b [&'a str], index: usize) -> &'b [&'a str] {
    let own = indentation(lines[index]);
    let start = (0..=index)
        .rev()
        .find(|&candidate| {
            split_comment(lines[candidate])
                .0
                .trim_start()
                .starts_with('-')
                && indentation(lines[candidate]) <= own
        })
        .unwrap_or(index);
    let dash = indentation(lines[start]);
    let end = (start + 1..lines.len())
        .find(|&candidate| {
            !split_comment(lines[candidate]).0.trim().is_empty()
                && indentation(lines[candidate]) <= dash
        })
        .unwrap_or(lines.len());
    &lines[start..end]
}

/// Every `actions/checkout` step, in any spelling of the owner and name
/// (GitHub resolves them case-insensitively), sets
/// `persist-credentials: false` itself: the token stays out of .git/config,
/// where later steps could read it.
fn checkout_violations(name: &str, text: &str) -> Vec<String> {
    let lines: Vec<&str> = text.lines().collect();
    let mut violations = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let checkout = key_values(split_comment(line).0, "uses")
            .iter()
            .any(|reference| {
                reference
                    .split_once('@')
                    .map_or(*reference, |(action, _)| action)
                    .eq_ignore_ascii_case("actions/checkout")
            });
        if !checkout {
            continue;
        }
        let persists = !step_lines(&lines, index).iter().any(|line| {
            key_values(split_comment(line).0, "persist-credentials").contains(&"false")
        });
        if persists {
            violations.push(format!(
                "{name}:{}: this checkout step must set persist-credentials: false",
                index + 1
            ));
        }
    }
    violations
}

/// A mapping entry `key: value` (the value may be empty), with the key
/// unquoted; `None` for anything else, such as a shell line of a `run:` block.
fn mapping_entry(content: &str) -> Option<(&str, &str)> {
    let (key, value) = content.split_once(':')?;
    let key = key.trim().trim_matches(['"', '\'']);
    let plain = !key.is_empty()
        && key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'));
    (plain && (value.is_empty() || value.starts_with([' ', '\t']))).then_some((key, value.trim()))
}

/// The containers a workflow runs, as (line number, image): a service's
/// `image:` (`services.<id>.image`), a job's `container.image`, the
/// `container: <image>` shorthand, and the same in flow style. An `image:`
/// anywhere else, such as an action input under a step's `with:` or a matrix
/// entry (release.yml), is not a container.
fn container_images(text: &str) -> Vec<(usize, String)> {
    // The keys that enclose the current line, with their indentation.
    let mut parents: Vec<(usize, String)> = Vec::new();
    let mut found = Vec::new();
    for (number, line) in text.lines().enumerate() {
        let code = split_comment(line).0;
        if code.trim().is_empty() {
            continue;
        }
        let mut indent = indentation(code);
        let mut content = code.trim();
        // A list item's mapping starts after its dash, one level deeper.
        if let Some(rest) = content
            .strip_prefix('-')
            .filter(|rest| rest.is_empty() || rest.starts_with([' ', '\t']))
        {
            while parents.last().is_some_and(|(depth, _)| *depth >= indent) {
                parents.pop();
            }
            let item = rest.trim_start();
            indent += content.len() - item.len();
            content = item;
        }
        while parents.last().is_some_and(|(depth, _)| *depth >= indent) {
            parents.pop();
        }
        let Some((key, value)) = mapping_entry(content) else {
            continue;
        };
        let parent = parents.last().map(|(_, key)| key.as_str());
        let grandparent = parents
            .len()
            .checked_sub(2)
            .map(|index| parents[index].1.as_str());
        let flow = value.starts_with('{');
        let image =
            key == "image" && (parent == Some("container") || grandparent == Some("services"));
        let shorthand = key == "container" && !value.is_empty() && !flow;
        if image || shorthand {
            found.push((number + 1, value.trim_matches(['"', '\'']).to_owned()));
        }
        if flow && (matches!(key, "services" | "container") || parent == Some("services")) {
            for image in key_values(value, "image") {
                found.push((number + 1, image.to_owned()));
            }
        }
        parents.push((indent, key.to_owned()));
    }
    found
}

fn container_violations(name: &str, text: &str) -> Vec<String> {
    container_images(text)
        .into_iter()
        .filter(|(_, image)| !has_digest(image))
        .map(|(number, image)| {
            format!(
                "{name}:{number}: service image or container `{image}` must be pinned by digest"
            )
        })
        .collect()
}

/// The scopes a line of code grants `write` to, in block (`contents: write`)
/// or flow style (`permissions: { contents: write }`), quoted or not.
fn write_scopes(code: &str) -> Vec<String> {
    let mut scopes = Vec::new();
    for (position, _) in code.match_indices("write") {
        let after = code[position + "write".len()..]
            .trim_start_matches(['"', '\''])
            .trim_start();
        if !(after.is_empty() || after.starts_with([',', '}'])) {
            continue;
        }
        let Some(before) = code[..position]
            .trim_end_matches(['"', '\''])
            .trim_end()
            .strip_suffix(':')
        else {
            continue;
        };
        let before = before.trim_end().trim_end_matches(['"', '\'']);
        let start = before
            .rfind(|character: char| {
                !(character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
            })
            .map_or(0, |index| index + 1);
        scopes.push(before[start..].to_owned());
    }
    scopes
}

/// The only scopes any workflow may write are the release job's `id-token`
/// and `packages`; checks, statuses, contents, actions... stay read.
fn permission_violations(name: &str, text: &str, release: bool) -> Vec<String> {
    let mut violations = Vec::new();
    if !text.lines().any(|line| line.starts_with("permissions:")) {
        violations.push(format!("{name}: no top-level permissions"));
    }
    if text.contains("pull_request_target") {
        violations.push(format!(
            "{name}: pull_request_target runs fork code with secrets"
        ));
    }
    if text.contains("write-all") {
        violations.push(format!("{name}: write access to everything"));
    }
    for (number, line) in text.lines().enumerate() {
        for scope in write_scopes(split_comment(line).0) {
            if !(release && matches!(scope.as_str(), "id-token" | "packages")) {
                violations.push(format!(
                    "{name}:{}: `{scope}: write` is not allowed",
                    number + 1
                ));
            }
        }
    }
    for permission in ["id-token: write", "packages: write"] {
        let count = text.matches(permission).count();
        if !((release && count == 1) || (!release && count == 0)) {
            violations.push(format!("{name}: `{permission}` appears {count} times"));
        }
    }
    violations
}

/// No workflow hands its secrets on wholesale or by computed name
/// (`secrets[`, `toJSON(secrets)`, `join(secrets, ...)`, `{ x: secrets }`,
/// `secrets: inherit`), in any case and spacing: expression contexts are
/// case-insensitive. The only secret any workflow reads is the release job's
/// token, spelled `secrets.GITHUB_TOKEN`.
fn secret_violations(name: &str, text: &str, release: bool) -> Vec<String> {
    let lower = text.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut violations = Vec::new();
    for (position, _) in lower.match_indices("secrets") {
        if position > 0
            && (bytes[position - 1].is_ascii_alphanumeric() || bytes[position - 1] == b'_')
        {
            continue;
        }
        let rest = lower[position + "secrets".len()..].trim_start();
        let inherit = rest
            .strip_prefix(':')
            .is_some_and(|value| value.trim_start().starts_with("inherit"));
        if inherit || rest.starts_with(['[', ')', ',', '}']) {
            let shown: String = text[position..].chars().take(24).collect();
            violations.push(format!(
                "{name}: `{shown}` reaches secrets outside the reviewed list"
            ));
        } else if rest.starts_with('.') {
            // The reference as written: `secrets`, blanks, `.`, blanks, name.
            let dot = lower.len() - rest.len();
            let after_dot = &text[dot + 1..];
            let name_start = dot + 1 + (after_dot.len() - after_dot.trim_start().len());
            let name_end = text[name_start..]
                .find(|character: char| {
                    !(character.is_ascii_alphanumeric() || matches!(character, '.' | '_'))
                })
                .map_or(text.len(), |offset| name_start + offset);
            let reference = &text[position..name_end];
            if !(release && reference == "secrets.GITHUB_TOKEN") {
                violations.push(format!("{name}: unexpected `{reference}`"));
            }
        }
    }
    violations
}

/// The one place outside the cargo-tool action that may install a cargo
/// tool: release.yml builds from a clean cache, so it installs
/// cargo-auditable itself, in this named step.
const RELEASE_INSTALL: (&str, &str) = (".github/workflows/release.yml", "Install cargo-auditable");
const TOOL_ACTION: &str = ".github/actions/cargo-tool/action.yml";

/// The logical lines of a file: comments removed and `\`-continued lines
/// joined, each with the index of its first physical line.
fn logical_lines(text: &str) -> Vec<(usize, String)> {
    let mut lines: Vec<(usize, String)> = Vec::new();
    let mut continued = false;
    for (index, line) in text.lines().enumerate() {
        let code = split_comment(line).0.trim_end();
        let (code, continues) = match code.strip_suffix('\\') {
            Some(code) => (code, true),
            None => (code, false),
        };
        match lines.last_mut() {
            Some((_, joined)) if continued => {
                joined.push(' ');
                joined.push_str(code);
            }
            _ => lines.push((index, code.to_owned())),
        }
        continued = continues;
    }
    lines
}

/// The words of a logical line, with shell separators (`&&`, `||`, `;`,
/// `|`, `$(`, `(`, `)`, backquotes) as words of their own.
fn shell_words(code: &str) -> Vec<String> {
    const SEPARATORS: [&str; 8] = ["&&", "||", "$(", ";", "|", "(", ")", "`"];
    let mut spaced = String::with_capacity(code.len() * 2);
    let mut rest = code;
    while let Some(character) = rest.chars().next() {
        match SEPARATORS
            .iter()
            .find(|separator| rest.starts_with(**separator))
        {
            Some(separator) => {
                spaced.push_str(&format!(" {separator} "));
                rest = &rest[separator.len()..];
            }
            None => {
                spaced.push(character);
                rest = &rest[character.len_utf8()..];
            }
        }
    }
    // Quotes and backslashes change nothing a shell runs here: `ca"rgo"`,
    // `'install'` and `--g\it` are cargo, install and --git.
    spaced
        .split_whitespace()
        .map(|word| word.replace(['\'', '"', '\\'], ""))
        .filter(|word| !word.is_empty())
        .collect()
}

fn is_separator(word: &str) -> bool {
    matches!(word, "&&" | "||" | "$(" | ";" | "|" | "(" | ")" | "`")
}

/// `cargo` in any spelling a shell runs: a path to it, `$CARGO` or
/// `${CARGO...}`, quoted or not.
fn is_cargo_program(word: &str) -> bool {
    let word = word.trim_matches(['"', '\'']);
    word == "cargo"
        || word.ends_with("/cargo")
        || word.starts_with("$CARGO")
        || word.starts_with("${CARGO")
}

/// Whether a `cargo install --version` value is one exact `X.Y.Z` (an `=`
/// requirement allowed): `^`, `>=`, `~`, `*` and partial versions resolve to
/// whatever is newest.
fn is_exact_version(value: &str) -> bool {
    let value = value.trim_matches(['"', '\'']);
    is_numeric_version(value.strip_prefix('=').unwrap_or(value), 3..=3)
}

/// One cargo command that installs, as found in a logical line.
struct CargoInstall {
    /// The logical line's first physical line.
    line: usize,
    /// The whole command: `cargo`, its options, `install`, its arguments.
    words: Vec<String>,
    /// Whether the logical line holds nothing but `run:` and this command.
    alone: bool,
}

/// Every cargo command of a file that installs: a word `install` or
/// `binstall` anywhere in a command whose program is cargo (options before
/// the subcommand included). A cargo command that ends its line is read on
/// into the next line, as a YAML-folded scalar would be.
fn cargo_installs(text: &str) -> Vec<CargoInstall> {
    let lines = logical_lines(text);
    let mut found = Vec::new();
    for (at, (line, code)) in lines.iter().enumerate() {
        let words = shell_words(code);
        for (position, word) in words.iter().enumerate() {
            if !is_cargo_program(word) {
                continue;
            }
            let mut command: Vec<String> = words[position..]
                .iter()
                .take_while(|word| !is_separator(word))
                .cloned()
                .collect();
            let reaches_end = position + command.len() == words.len();
            let only_options = command[1..]
                .iter()
                .all(|word| word.starts_with('-') || word.starts_with('+'));
            if reaches_end && only_options {
                // A YAML-folded scalar continues the command on the next
                // line; a literal block starts a new one there. Read on only
                // when the next line goes on with the subcommand.
                if let Some((_, next)) = lines.get(at + 1) {
                    let next: Vec<String> = shell_words(next)
                        .into_iter()
                        .take_while(|word| !is_separator(word))
                        .collect();
                    let subcommand = next.iter().find(|word| {
                        !(word.starts_with('-') || word.starts_with('+') || word.contains('='))
                    });
                    if subcommand.is_some_and(|word| {
                        matches!(word.trim_matches(['"', '\'']), "install" | "binstall")
                    }) {
                        command.extend(next);
                    }
                }
            }
            let installs = command[1..]
                .iter()
                .any(|word| matches!(word.trim_matches(['"', '\'']), "install" | "binstall"));
            if installs {
                let prefix = &words[..position];
                let alone = (prefix.is_empty() || prefix == ["run:"] || prefix == ["-", "run:"])
                    && position + command.len() == words.len();
                found.push(CargoInstall {
                    line: *line,
                    words: command,
                    alone,
                });
            }
        }
    }
    found
}

/// Problems of an install at one of the two allowed sites: it must be a
/// plain `cargo install <crate>` alone in its `run`, with one exact
/// `--version` (the action's `"$VERSION"`, checked at each caller) and
/// `--locked`, from crates.io.
fn allowed_install_problems(install: &CargoInstall, in_tool_action: bool) -> Option<&'static str> {
    let words: Vec<&str> = install.words.iter().map(String::as_str).collect();
    if !install.alone || words.get(1) != Some(&"install") {
        return Some(
            "must be a plain `cargo install`, alone in its run, with no option before install",
        );
    }
    let arguments = &words[2..];
    let version =
        arguments
            .iter()
            .enumerate()
            .find_map(|(at, argument)| match argument.split_once('=') {
                Some(("--version" | "--vers", value)) => Some(value),
                _ if matches!(*argument, "--version" | "--vers") => arguments.get(at + 1).copied(),
                _ => None,
            });
    let exact = version
        .is_some_and(|value| is_exact_version(value) || (in_tool_action && value == "$VERSION"));
    let source = arguments.iter().any(|argument| {
        let flag = argument.split_once('=').map_or(*argument, |(flag, _)| flag);
        matches!(
            flag,
            "--git" | "--path" | "--registry" | "--index" | "--config" | "-Z"
        )
    });
    if !exact || !arguments.contains(&"--locked") || source {
        return Some("needs one exact --version X.Y.Z and --locked, from crates.io");
    }
    // The release step installs cargo-auditable and nothing else: once the
    // version and --locked are set aside, that is the whole command.
    if !in_tool_action {
        let mut rest = Vec::new();
        let mut words = arguments.iter();
        while let Some(word) = words.next() {
            match *word {
                "--locked" => {}
                "--version" | "--vers" => {
                    words.next();
                }
                word if word.starts_with("--version=") || word.starts_with("--vers=") => {}
                word => rest.push(word),
            }
        }
        if rest != ["cargo-auditable"] {
            return Some("the release step installs exactly `cargo-auditable`");
        }
    }
    None
}

/// Cargo tools are installed only through the local cargo-tool action
/// (exact version, cached under it) and in one reviewed release.yml step;
/// any other install in a workflow or action is refused, however it is
/// spelled: a path to cargo, `$CARGO`, options before `install`,
/// `binstall`, `cargo-binstall`, a `CARGO_ALIAS_*` that renames `install`,
/// a `\`-continued or folded line. Probes that would skip an install
/// because some binary is already there (`command -v`, `which`, `hash`,
/// `type`, file tests on `~/.cargo/bin`) are refused too, and a cargo-tool
/// step needs an exact `version: X.Y.Z`. `relative` is the file's path in
/// the repository.
fn tool_install_violations(relative: &str, text: &str) -> Vec<String> {
    let lines: Vec<&str> = text.lines().collect();
    let in_tool_action = relative == TOOL_ACTION;
    let release_step: Vec<usize> = if relative == RELEASE_INSTALL.0 {
        lines
            .iter()
            .position(|line| key_values(split_comment(line).0, "name").contains(&RELEASE_INSTALL.1))
            .map(|index| {
                let step = step_lines(&lines, index);
                let start = lines
                    .iter()
                    .position(|line| std::ptr::eq(*line, step[0]))
                    .unwrap_or(index);
                (start..start + step.len()).collect()
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let mut violations = Vec::new();
    for install in cargo_installs(text) {
        let shown = install.words.join(" ");
        let allowed = in_tool_action || release_step.contains(&install.line);
        let problem = if allowed {
            allowed_install_problems(&install, in_tool_action)
        } else {
            Some("cargo tools are installed only through .github/actions/cargo-tool")
        };
        if let Some(problem) = problem {
            violations.push(format!(
                "{relative}:{}: `{shown}`: {problem}",
                install.line + 1
            ));
        }
    }
    for (index, line) in lines.iter().enumerate() {
        let code = split_comment(line).0;
        let words: Vec<&str> = code.split_whitespace().collect();
        let mut refuse = |reason: &str| {
            violations.push(format!(
                "{relative}:{}: `{}`: {reason}",
                index + 1,
                line.trim()
            ));
        };
        if words.iter().any(|word| {
            let word = word.trim_matches(['"', '\'']);
            word == "cargo-binstall" || word.ends_with("/cargo-binstall")
        }) {
            refuse("cargo-binstall installs prebuilt binaries");
        }
        if code.contains("CARGO_ALIAS_") {
            refuse("a CARGO_ALIAS_* can rename install");
        }
        let flat = words.join(" ");
        let probes = [
            "command -v cargo-",
            "which cargo-",
            "hash cargo-",
            "type cargo-",
            "type -p cargo-",
        ];
        let file_test = [" -x ", " -f ", " -e ", " -s "]
            .iter()
            .any(|test| flat.contains(test))
            && (flat.contains(".cargo/bin") || flat.contains("/cargo-"));
        if probes.iter().any(|probe| flat.contains(probe)) || file_test {
            refuse("a cached cargo tool is found by its version, not by a probe");
        }
        let tool = key_values(code, "uses")
            .iter()
            .any(|reference| reference.trim_end_matches('/') == "./.github/actions/cargo-tool");
        if tool {
            let exact = step_lines(&lines, index)
                .iter()
                .flat_map(|line| key_values(split_comment(line).0, "version"))
                .next()
                .is_some_and(|version| is_numeric_version(version, 3..=3));
            if !exact {
                refuse("the cargo-tool step needs an exact `version: X.Y.Z`");
            }
        }
    }
    violations
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

#[test]
fn only_allowlisted_actions_are_used() {
    let mut used = BTreeSet::new();
    for (name, relative) in workflows_and_local_actions() {
        let text = read(&relative);
        assert_clean(&allowlist_violations(&name, &text));
        for line in text.lines() {
            for reference in key_values(split_comment(line).0, "uses") {
                if let Some((action, _)) = reference.split_once('@') {
                    used.insert(action.to_ascii_lowercase());
                }
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
    ] {
        let text = snippet(rejected);
        assert!(
            !allowlist_violations("t.yml", &text).is_empty(),
            "accepted: {text}"
        );
    }
    for accepted in [
        "      - uses: actions/checkout@SHA # v7.0.1",
        "      - uses: Actions/Checkout@SHA # v7.0.1",
        "      - uses: swatinem/RUST-CACHE@SHA # v2.9.2",
        "      - uses: docker/build-push-action/subdir@SHA # v7",
        "      - uses: ./.github/actions/cargo-tool",
    ] {
        assert_clean(&allowlist_violations("t.yml", &snippet(accepted)));
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
    let ci = read("crates/forge-cli/templates/app/github/workflows/ci.yml.tmpl");
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
