//! Enforces the hexagonal dependency direction: an inner ring must never import
//! an outer ring or a transport, database or vendor crate.
//!
//! The check is syntactic. It flattens `use` trees (including grouped and
//! aliased imports) and scans fully qualified paths. Relative `super::` paths
//! that climb out of a ring are not resolved; keep cross-ring references
//! absolute (`crate::...`) so this test can see them.

use std::fs;
use std::path::{Path, PathBuf};

struct Rule {
    ring: &'static str,
    forbidden: &'static [&'static str],
}

const RULES: &[Rule] = &[
    Rule {
        ring: "src/domain",
        forbidden: &[
            "crate::application",
            "crate::adapters",
            "crate::infrastructure",
            "crate::bootstrap",
            "askama",
            "forge::config",
            "forge::db",
            "forge::http",
            "forge::openapi",
            "forge::views",
            "forge_config",
            "forge_db",
            "forge_http",
            "forge_openapi",
            "forge_views",
            "http",
            "hyper",
            "reqwest",
            "serde_json",
            "sqlx",
            "std::env",
            "std::fs",
            "std::net",
            "std::process",
            "tokio",
        ],
    },
    Rule {
        ring: "src/application",
        forbidden: &[
            "crate::adapters",
            "crate::infrastructure",
            "crate::bootstrap",
            "askama",
            "forge::db",
            "forge::http",
            "forge::openapi",
            "forge::views",
            "forge_db",
            "forge_http",
            "forge_openapi",
            "forge_views",
            "http",
            "hyper",
            "reqwest",
            "sqlx",
        ],
    },
    Rule {
        ring: "src/adapters",
        forbidden: &["crate::bootstrap"],
    },
    Rule {
        ring: "src/infrastructure",
        forbidden: &["crate::adapters", "crate::bootstrap"],
    },
];

#[test]
fn rings_respect_dependency_direction() -> Result<(), Box<dyn std::error::Error>> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut violations = Vec::new();

    for rule in RULES {
        for file in rust_files(&root.join(rule.ring))? {
            let source = fs::read_to_string(&file)?;
            for path in referenced_paths(&source) {
                if let Some(forbidden) = rule.forbidden.iter().find(|f| is_within(&path, f)) {
                    let relative = file.strip_prefix(root).unwrap_or(&file);
                    violations.push(format!(
                        "{} references `{path}` ({} must not depend on `{forbidden}`)",
                        relative.display(),
                        rule.ring
                    ));
                }
            }
        }
    }

    assert!(
        violations.is_empty(),
        "architecture violations:\n{}",
        violations.join("\n")
    );
    Ok(())
}

#[test]
fn detects_grouped_aliased_and_inline_references() {
    let source = r#"
        use crate::{domain::entities, adapters::http as web};
        pub(crate) use ::tokio::sync::{Mutex, RwLock};
        // use crate::bootstrap; comments are ignored
        fn load() { let _ = crate::infrastructure::database::pool(); }
    "#;

    let paths = referenced_paths(source);

    assert!(paths.iter().any(|p| is_within(p, "crate::adapters")));
    assert!(paths.iter().any(|p| is_within(p, "tokio")));
    assert!(paths.iter().any(|p| is_within(p, "crate::infrastructure")));
    assert!(!paths.iter().any(|p| is_within(p, "crate::bootstrap")));
    assert!(paths.iter().all(|p| !p.ends_with("::web")));
}

fn rust_files(directory: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    if !directory.exists() {
        return Ok(files);
    }
    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        if path.is_dir() {
            files.extend(rust_files(&path)?);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            files.push(path);
        }
    }
    Ok(files)
}

/// Returns `true` when `path` is `prefix` or a path nested below it.
fn is_within(path: &str, prefix: &str) -> bool {
    path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with("::"))
}

/// Collects imported paths plus fully qualified paths used inline.
fn referenced_paths(source: &str) -> Vec<String> {
    let code = strip_comments(source);
    let mut paths = Vec::new();
    for tree in use_trees(&code) {
        flatten_use_tree("", &tree, &mut paths);
    }
    paths.extend(inline_paths(&code));
    paths
        .into_iter()
        .map(|path| path.trim_start_matches("::").to_owned())
        .collect()
}

fn strip_comments(source: &str) -> String {
    let mut code = String::with_capacity(source.len());
    let mut rest = source;
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix("//") {
            rest = after.find('\n').map_or("", |end| &after[end..]);
        } else if let Some(after) = rest.strip_prefix("/*") {
            rest = after.find("*/").map_or("", |end| &after[end + 2..]);
        } else {
            let mut characters = rest.chars();
            if let Some(character) = characters.next() {
                code.push(character);
            }
            rest = characters.as_str();
        }
    }
    code
}

fn is_identifier(character: char) -> bool {
    character.is_alphanumeric() || character == '_'
}

/// Extracts the body of every `use ...;` declaration with aliases removed and
/// whitespace collapsed.
fn use_trees(code: &str) -> Vec<String> {
    let mut trees = Vec::new();
    let mut search_from = 0;
    while let Some(offset) = code[search_from..].find("use") {
        let start = search_from + offset;
        let end_of_keyword = start + "use".len();
        search_from = end_of_keyword;

        let before_ok = code[..start]
            .chars()
            .next_back()
            .is_none_or(|c| !is_identifier(c));
        let after_ok = code[end_of_keyword..]
            .chars()
            .next()
            .is_some_and(char::is_whitespace);
        if !before_ok || !after_ok {
            continue;
        }
        let Some(length) = code[end_of_keyword..].find(';') else {
            break;
        };
        let declaration = &code[end_of_keyword..end_of_keyword + length];
        trees.push(remove_aliases(declaration));
        search_from = end_of_keyword + length + 1;
    }
    trees
}

/// Drops `as alias` clauses and all whitespace from a use tree.
fn remove_aliases(declaration: &str) -> String {
    let mut tree = String::new();
    let mut tokens = declaration.split_whitespace().peekable();
    while let Some(token) = tokens.next() {
        if token == "as" {
            if let Some(alias) = tokens.next() {
                tree.extend(alias.chars().skip_while(|c| is_identifier(*c)));
            }
        } else {
            tree.push_str(token);
        }
    }
    tree
}

fn flatten_use_tree(prefix: &str, tree: &str, paths: &mut Vec<String>) {
    match (tree.find('{'), tree.ends_with('}')) {
        (Some(open), true) => {
            let head = format!("{prefix}{}", &tree[..open]);
            for branch in split_top_level(&tree[open + 1..tree.len() - 1]) {
                flatten_use_tree(&head, branch, paths);
            }
        }
        _ => paths.push(format!("{prefix}{tree}")),
    }
}

fn split_top_level(group: &str) -> Vec<&str> {
    let mut branches = Vec::new();
    let mut depth = 0_usize;
    let mut start = 0;
    for (index, character) in group.char_indices() {
        match character {
            '{' => depth += 1,
            '}' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                branches.push(&group[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    branches.push(&group[start..]);
    branches
        .into_iter()
        .filter(|branch| !branch.is_empty())
        .collect()
}

/// Finds `a::b::c` paths written inline in expressions and types.
fn inline_paths(code: &str) -> Vec<String> {
    let mut paths = Vec::new();
    let mut current = String::new();
    for character in code.chars().chain(std::iter::once(' ')) {
        if is_identifier(character) || character == ':' {
            current.push(character);
        } else {
            if current.contains("::") {
                paths.push(current.trim_end_matches(':').to_owned());
            }
            current.clear();
        }
    }
    paths
}
