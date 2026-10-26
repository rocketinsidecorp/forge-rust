//! The sleep budget (docs/test-strategy.md): a test that sleeps waits on the real
//! clock, which makes it slow and, on a loaded runner, flaky. The Rust twin of
//! Forge-Go's `test_sleeps_test.go`.
//!
//! Every `sleep(` call in test code (`std::thread::sleep`,
//! `tokio::time::sleep`) is one of two things:
//!
//! - **fake clock**: inside a test that runs on the tokio paused clock
//!   (`#[tokio::test(start_paused = true)]`, or a body that calls
//!   `tokio::time::pause()`). Time only moves when the test says so, so the
//!   sleep costs nothing and is deterministic. These need no comment.
//! - **real clock**: it carries a `// real-clock: <reason>` comment on the same
//!   line or in the comment block directly above, saying why no fake clock or
//!   event can replace it (a child process, a real socket, the database's
//!   clock, a poll with a deadline). The number of them may never grow: it is
//!   capped by [`BUDGET`], which only goes down. A new test waits for an event
//!   (a `Notify`, a channel, a counter), polls with a deadline, or uses the
//!   paused clock.
//!
//! Test code is a file under a `tests/` directory, a file named `tests.rs`,
//! `*_tests.rs`, `testing.rs` or `suite.rs`, or an item marked
//! `#[cfg(test)]` or `#[cfg(all(test, ...))]` (other `cfg` forms, such as
//! `any(test, feature = "testing")`, are not only test code and are not
//! scanned). The scanner reads text, like `supply_chain.rs`; its rules
//! are functions with their own tests below, so each bypass it closes stays
//! closed.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};

/// The most real-clock sleeps test code may hold. The gate fails above it and
/// prints a hint when the count is below it: lower it when a sleep goes away,
/// so that the slack cannot be used by a new sleep.
const BUDGET: usize = 107;

/// Directories that hold no workspace test code.
const SKIPPED_DIRECTORIES: [&str; 4] = ["target", ".git", "corpus", "artifacts"];

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// One `sleep(` call in test code.
#[derive(Debug, PartialEq, Eq)]
struct Sleep {
    line: usize,
    kind: Kind,
}

#[derive(Debug, PartialEq, Eq)]
enum Kind {
    /// On the tokio paused clock: free and deterministic.
    FakeClock,
    /// Carries a `// real-clock:` reason and counts against the budget.
    RealClock,
    /// Has no reason: a violation.
    Unexplained,
}

/// Whether the whole file is test code, by its path (relative, `/` separated).
fn is_test_file(relative: &str) -> bool {
    let mut parts: Vec<&str> = relative.split('/').collect();
    let name = parts.pop().unwrap_or_default();
    parts.contains(&"tests")
        || name.ends_with("_tests.rs")
        || matches!(name, "tests.rs" | "testing.rs" | "suite.rs")
}

fn indent(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// The lines inside `#[cfg(test)]` items: from the attribute to the closing
/// brace at the attribute's indentation (rustfmt puts it there), or only the
/// item's own line when it ends in `;` (`use`, `mod tests;`).
fn cfg_test_lines(lines: &[&str]) -> Vec<bool> {
    let mut inside = vec![false; lines.len()];
    let mut index = 0;
    while index < lines.len() {
        let marker = lines[index].trim();
        if marker != "#[cfg(test)]" && !marker.starts_with("#[cfg(all(test,") {
            index += 1;
            continue;
        }
        let depth = indent(lines[index]);
        let mut item = index + 1;
        while item < lines.len() && lines[item].trim_start().starts_with("#[") {
            item += 1;
        }
        let single = item < lines.len()
            && !lines[item].contains('{')
            && lines[item].trim_end().ends_with(';');
        let mut end = item;
        if !single {
            while end < lines.len()
                && !(indent(lines[end]) == depth && lines[end].trim_start().starts_with('}'))
            {
                end += 1;
            }
        }
        for flag in &mut inside[index..=end.min(lines.len() - 1)] {
            *flag = true;
        }
        index = end + 1;
    }
    inside
}

/// The byte offsets of `sleep(` calls in the code part of `line`: outside
/// string literals and line comments, at a word boundary, and not the
/// definition `fn sleep(`.
fn sleep_calls(line: &str) -> Vec<usize> {
    let bytes = line.as_bytes();
    let mut found = Vec::new();
    let mut in_string = false;
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if in_string {
            match byte {
                b'\\' => index += 1,
                b'"' => in_string = false,
                _ => {}
            }
        } else if bytes[index..].starts_with(b"'\"'") {
            index += 2;
        } else if byte == b'"' {
            in_string = true;
        } else if bytes[index..].starts_with(b"//") {
            break;
        } else if bytes[index..].starts_with(b"sleep(")
            && (index == 0
                || !(bytes[index - 1].is_ascii_alphanumeric() || bytes[index - 1] == b'_'))
            && !line[..index].trim_end().ends_with("fn")
        {
            found.push(index);
        }
        index += 1;
    }
    found
}

/// Whether `text` (the part of a comment after `real-clock:`) is a reason.
fn is_reason(text: &str) -> bool {
    text.split_whitespace().count() >= 3
}

/// Whether the sleep on `lines[at]` has a real-clock reason: on its own line
/// or in the comment block directly above it.
fn has_reason(lines: &[&str], at: usize) -> bool {
    let reason_in = |line: &str| {
        line.split_once("// real-clock:")
            .is_some_and(|(_, reason)| is_reason(reason))
    };
    if reason_in(lines[at]) {
        return true;
    }
    lines[..at]
        .iter()
        .rev()
        .take_while(|line| line.trim_start().starts_with("//"))
        .any(|line| reason_in(line))
}

/// Whether the function around `lines[at]` runs on the paused clock: its
/// attributes say `start_paused = true`, or its body up to the sleep calls
/// `time::pause()`.
fn on_paused_clock(lines: &[&str], at: usize) -> bool {
    let Some(start) = (0..=at).rev().find(|&index| {
        let line = lines[index].trim_start();
        let line = line.strip_prefix("pub ").unwrap_or(line);
        let line = line
            .strip_prefix("pub(crate) ")
            .unwrap_or(line)
            .trim_start_matches("async ");
        line.starts_with("fn ")
    }) else {
        return false;
    };
    if lines[start..=at]
        .iter()
        .any(|line| line.contains("time::pause()"))
    {
        return true;
    }
    lines[..start]
        .iter()
        .rev()
        .take_while(|line| line.trim_start().starts_with("#["))
        .any(|line| line.contains("start_paused = true"))
}

/// Every sleep in the test code of one file.
fn sleeps(relative: &str, text: &str) -> Vec<Sleep> {
    let lines: Vec<&str> = text.lines().collect();
    let whole_file = is_test_file(relative);
    let in_cfg_test = cfg_test_lines(&lines);
    let mut found = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if !(whole_file || in_cfg_test[index]) || sleep_calls(line).is_empty() {
            continue;
        }
        let kind = if on_paused_clock(&lines, index) {
            Kind::FakeClock
        } else if has_reason(&lines, index) {
            Kind::RealClock
        } else {
            Kind::Unexplained
        };
        found.push(Sleep {
            line: index + 1,
            kind,
        });
    }
    found
}

/// Every Rust file of the workspace below `directory`, relative to `base`.
fn rust_files(base: &Path, directory: &Path, found: &mut Vec<String>) {
    let mut entries: Vec<_> = fs::read_dir(directory)
        .unwrap_or_else(|error| panic!("{}: {error}", directory.display()))
        .filter_map(Result::ok)
        .collect();
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if path.is_dir() {
            if !SKIPPED_DIRECTORIES.contains(&name.as_str()) {
                rust_files(base, &path, found);
            }
        } else if name.ends_with(".rs") {
            let relative = path.strip_prefix(base).expect("below the root");
            found.push(relative.to_string_lossy().replace('\\', "/"));
        }
    }
}

#[test]
fn every_real_clock_sleep_in_a_test_says_why_and_the_budget_never_grows() {
    let base = root();
    let mut files = Vec::new();
    rust_files(&base, &base.join("crates"), &mut files);
    assert!(
        files.len() > 200,
        "the scan found only {} files",
        files.len()
    );

    let mut real = 0;
    let mut fake = 0;
    let mut unexplained = Vec::new();
    for relative in &files {
        let text = fs::read_to_string(base.join(relative))
            .unwrap_or_else(|error| panic!("{relative}: {error}"));
        for sleep in sleeps(relative, &text) {
            match sleep.kind {
                Kind::FakeClock => fake += 1,
                Kind::RealClock => real += 1,
                Kind::Unexplained => unexplained.push(format!("{relative}:{}", sleep.line)),
            }
        }
    }
    assert!(
        unexplained.is_empty(),
        "sleep in test code without a `// real-clock: <reason>` comment (wait for an event, \
         poll with a deadline, or use the tokio paused clock instead; docs/test-strategy.md):\n{}",
        unexplained.join("\n")
    );
    assert!(
        fake > 0,
        "no sleep on the paused clock was recognised: the detector is broken"
    );
    assert!(
        real <= BUDGET,
        "{real} real-clock sleeps in tests, over the budget of {BUDGET}: remove one, or use a fake \
         clock or an event (docs/test-strategy.md)"
    );
    if real < BUDGET {
        // A ceiling, not an equality: two branches that each remove a sleep
        // must not turn main red when both merge.
        eprintln!(
            "{real} real-clock sleeps in tests, under the budget of {BUDGET}: lower BUDGET to {real} \
             in crates/forge/tests/test_sleeps.rs, so the budget only goes down"
        );
    }
}

#[test]
fn a_sleep_needs_a_reason_on_its_line_or_in_the_comment_directly_above() {
    let kinds = |text: &str| -> Vec<Kind> {
        sleeps("crates/x/tests/t.rs", text)
            .into_iter()
            .map(|sleep| sleep.kind)
            .collect()
    };
    assert_eq!(
        kinds("fn t() {\n    std::thread::sleep(D);\n}\n"),
        [Kind::Unexplained]
    );
    assert_eq!(
        kinds(
            "fn t() {\n    // real-clock: polls a child process\n    std::thread::sleep(D);\n}\n"
        ),
        [Kind::RealClock]
    );
    assert_eq!(
        kinds("fn t() {\n    sleep(D); // real-clock: a real socket must idle\n}\n"),
        [Kind::RealClock]
    );
    // The reason may sit anywhere in the comment block above.
    assert_eq!(
        kinds(
            "fn t() {\n    // real-clock: the store keeps its own time\n    // (second line)\n    tokio::time::sleep(D).await;\n}\n"
        ),
        [Kind::RealClock]
    );
    // Not adjacent (code in between), empty, or a bare marker: no reason.
    assert_eq!(
        kinds("fn t() {\n    // real-clock: stale reason here\n    let x = 1;\n    sleep(D);\n}\n"),
        [Kind::Unexplained]
    );
    assert_eq!(
        kinds(
            "fn t() {\n    // real-clock:\n    sleep(D);\n    // real-clock: ok\n    sleep(D);\n}\n"
        ),
        [Kind::Unexplained, Kind::Unexplained]
    );
}

#[test]
fn the_paused_clock_needs_no_reason() {
    let kinds = |text: &str| -> Vec<Kind> {
        sleeps("crates/x/tests/t.rs", text)
            .into_iter()
            .map(|sleep| sleep.kind)
            .collect()
    };
    assert_eq!(
        kinds(
            "#[tokio::test(start_paused = true)]\nasync fn t() {\n    tokio::time::sleep(D).await;\n}\n"
        ),
        [Kind::FakeClock]
    );
    assert_eq!(
        kinds(
            "#[tokio::test]\nasync fn t() {\n    tokio::time::pause();\n    tokio::time::sleep(D).await;\n}\n"
        ),
        [Kind::FakeClock]
    );
    // The next test is not paused because the previous one was.
    assert_eq!(
        kinds(
            "#[tokio::test(start_paused = true)]\nasync fn a() {}\n\n#[tokio::test]\nasync fn b() {\n    tokio::time::sleep(D).await;\n}\n"
        ),
        [Kind::Unexplained]
    );
    assert_eq!(
        kinds("#[tokio::test]\nasync fn t() {\n    tokio::time::sleep(D).await;\n}\n"),
        [Kind::Unexplained]
    );
}

#[test]
fn only_test_code_is_scanned_and_only_calls_count() {
    let production = "fn run() {\n    std::thread::sleep(D);\n}\n";
    assert!(sleeps("crates/x/src/lib.rs", production).is_empty());
    // A `#[cfg(test)]` module at the end, and a nested one, are test code.
    let module = "fn run() {\n    sleep(D);\n}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn t() {\n        sleep(D);\n    }\n}\n";
    assert_eq!(
        sleeps("crates/x/src/lib.rs", module),
        [Sleep {
            line: 9,
            kind: Kind::Unexplained
        }]
    );
    // A one-line `#[cfg(test)]` item does not swallow the code after it.
    let single = "#[cfg(test)]\nuse std::time::Duration;\n\nfn run() {\n    sleep(D);\n}\n";
    assert!(sleeps("crates/x/src/lib.rs", single).is_empty());
    // `cfg(all(test, ...))` is test code too; `any(test, ...)` is not.
    let all = "#[cfg(all(test, unix))]\nmod tests {\n    fn t() {\n        sleep(D);\n    }\n}\n";
    assert_eq!(sleeps("crates/x/src/lib.rs", all).len(), 1);
    let any = "#[cfg(any(test, feature = \"testing\"))]\nfn helper() {\n    sleep(D);\n}\n";
    assert!(sleeps("crates/x/src/lib.rs", any).is_empty());
    // File names that mean test code.
    for name in [
        "crates/x/src/a_tests.rs",
        "crates/x/src/tests.rs",
        "crates/x/src/testing.rs",
        "crates/x/src/suite.rs",
        "crates/x/tests/support/mod.rs",
    ] {
        assert_eq!(sleeps(name, production).len(), 1, "{name}");
    }
    // Comments, strings, definitions and other names are not calls.
    let not_calls = "// sleep(D)\nlet a = \"sleep(D)\";\nlet b = 'x'; // sleep(D)\nasync fn sleep(d: D) {}\nlet c = oversleep(D);\nlet d = sleep_until(D);\nlet e = '\"'; let f = \"http://x\";\n";
    assert!(sleeps("crates/x/tests/t.rs", not_calls).is_empty());
    let calls = "sleep(D);\nstd::thread::sleep(D);\nx(tokio::time::sleep(D));\n";
    assert_eq!(sleeps("crates/x/tests/t.rs", calls).len(), 3);
}
