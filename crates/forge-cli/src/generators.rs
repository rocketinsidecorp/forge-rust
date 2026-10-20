//! What every file generator shares (ADR 0038): the planned files and their
//! roles, the refusal to overwrite or to write through a symlink, the
//! one-owner rule for permissions and the printed file list.
//!
//! A generator renders a [`Planned`] list without touching the disk, checks
//! it, then writes every file with create-new semantics (all or nothing).
//! `forge destroy` renders the same list from the same arguments and
//! removes a file only while it is byte-identical.

use std::fs;
use std::io;
use std::path::{Component, Path};

use crate::CliError;
use crate::scaffold::{File, write_all};

/// What a planned file is to the application.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Role {
    /// Belongs to this generation: `forge destroy` removes it while it is
    /// unchanged.
    Owned,
    /// Written by the first generator that needs it and used by others (the
    /// access policy, the live-update transport, a module directory):
    /// `forge destroy` never removes it.
    Shared,
    /// A migration: `forge destroy` keeps it unless `--migrations`, because
    /// the database may already have applied it.
    Migration,
    /// Text the developer is asked to translate (a locale file of a locale
    /// other than English): `forge destroy` keeps it when it changed, instead
    /// of refusing the whole run.
    Translation,
}

/// One file a generator writes, relative to the application root.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Planned {
    pub(crate) path: String,
    pub(crate) content: String,
    pub(crate) role: Role,
}

impl Planned {
    pub(crate) fn new(path: impl Into<String>, content: impl Into<String>, role: Role) -> Self {
        Self {
            path: path.into(),
            content: content.into(),
            role,
        }
    }
}

/// Longest generator, action, field or method name.
pub(crate) const MAX_NAME_LENGTH: usize = 40;

/// Rust strict and reserved keywords: generated names become identifiers.
pub(crate) const RUST_KEYWORDS: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern",
    "false", "fn", "for", "gen", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut",
    "pub", "ref", "return", "self", "static", "struct", "super", "trait", "true", "type", "unsafe",
    "use", "where", "while", "abstract", "become", "box", "do", "final", "macro", "override",
    "priv", "try", "typeof", "unsized", "virtual", "yield", "union",
];

/// `[a-z][a-z0-9]*(_[a-z0-9]+)*`, at most `max` bytes, and not a Rust
/// keyword. The reason names `what`.
pub(crate) fn snake_case(what: &str, name: &str, max: usize) -> Result<(), String> {
    let valid = !name.is_empty()
        && name.len() <= max
        && name.as_bytes()[0].is_ascii_lowercase()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        && !name.ends_with('_')
        && !name.contains("__");
    if !valid {
        return Err(format!(
            "{what} `{name}` must be snake_case: lowercase ASCII letters, digits and single \
             underscores, starting with a letter, at most {max} characters"
        ));
    }
    if RUST_KEYWORDS.contains(&name) {
        return Err(format!("{what} `{name}` is a Rust keyword"));
    }
    Ok(())
}

/// Refuses a path that runs through a symlink, or names something other
/// than a regular file, below `root`: nothing is written or removed through
/// a link. Paths that do not exist yet pass.
pub(crate) fn refuse_symlinks<'a>(
    root: &Path,
    paths: impl IntoIterator<Item = &'a str>,
) -> Result<(), String> {
    for path in paths {
        let mut current = root.to_path_buf();
        let components: Vec<Component<'_>> = Path::new(path).components().collect();
        for (index, component) in components.iter().enumerate() {
            let Component::Normal(part) = component else {
                return Err(format!("{path} is not a plain relative path"));
            };
            current.push(part);
            match fs::symlink_metadata(&current) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Err(format!(
                        "{} is a symbolic link; nothing is written or removed through a link",
                        relative(root, &current)
                    ));
                }
                Ok(metadata) if index + 1 == components.len() && !metadata.is_file() => {
                    return Err(format!("{path} exists and is not a regular file"));
                }
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => break,
                Err(error) => return Err(format!("{path}: {error}")),
            }
        }
    }
    Ok(())
}

fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// The planned files that already exist (including dangling symlinks).
pub(crate) fn existing(root: &Path, files: &[Planned]) -> Vec<String> {
    files
        .iter()
        .filter(|file| fs::symlink_metadata(root.join(&file.path)).is_ok())
        .map(|file| file.path.clone())
        .collect()
}

/// Refuses to write when a planned file exists or a path runs through a
/// symlink. `generator` labels the error.
pub(crate) fn refuse_overwrite(
    root: &Path,
    generator: &'static str,
    files: &[Planned],
) -> Result<(), CliError> {
    refuse_symlinks(root, files.iter().map(|file| file.path.as_str()))
        .map_err(|reason| generate_error(generator, reason))?;
    let existing = existing(root, files);
    if existing.is_empty() {
        return Ok(());
    }
    Err(generate_error(
        generator,
        format!(
            "refusing to overwrite existing files; nothing was written:\n  {}",
            existing.join("\n  ")
        ),
    ))
}

pub(crate) fn generate_error(generator: &'static str, reason: impl Into<String>) -> CliError {
    CliError::Generate {
        generator,
        reason: reason.into(),
    }
}

/// Writes `files` with create-new semantics, all or nothing.
pub(crate) fn write(root: &Path, files: &[Planned]) -> Result<(), CliError> {
    let files: Vec<File> = files
        .iter()
        .map(|file| File {
            path: file.path.clone(),
            content: file.content.clone(),
        })
        .collect();
    write_all(root, &files)
}

/// Lists the created (or, with `--dry-run`, planned) files.
pub(crate) fn report(
    output: &mut dyn io::Write,
    dry_run: bool,
    files: &[Planned],
) -> Result<(), CliError> {
    let verb = if dry_run { "would create" } else { "create" };
    for file in files {
        writeln!(output, "  {verb:<12} {}", file.path).map_err(CliError::Output)?;
    }
    Ok(())
}

/// The migration files named `<version>_<name>.up.sql` and `.down.sql` in
/// `migrations/`, sorted: up files first.
pub(crate) fn existing_migrations(root: &Path, name: &str) -> (Vec<String>, Vec<String>) {
    let mut up = Vec::new();
    let mut down = Vec::new();
    if let Ok(entries) = fs::read_dir(root.join("migrations")) {
        for entry in entries.filter_map(Result::ok) {
            let file = entry.file_name().to_string_lossy().into_owned();
            let Some((version, rest)) = file.split_once('_') else {
                continue;
            };
            if version.is_empty() || !version.bytes().all(|byte| byte.is_ascii_digit()) {
                continue;
            }
            if rest == format!("{name}.up.sql") {
                up.push(format!("migrations/{file}"));
            } else if rest == format!("{name}.down.sql") {
                down.push(format!("migrations/{file}"));
            }
        }
    }
    up.sort();
    down.sort();
    (up, down)
}

/// The first Rust file under `src/` (other than `except`) that names one of
/// `permissions` as a string literal (`"posts:read"`), and the permission.
///
/// Every permission has exactly one owner, the generated module that
/// declares its constant: a controller reuses a model's `READ_PERMISSION`
/// instead of declaring it again, and a model or scaffold refuses a plural
/// whose permissions another module already declares (ADR 0038). Any
/// mention counts, so hand-written code is found too.
pub(crate) fn permission_owner(
    root: &Path,
    permissions: &[String],
    except: &[&str],
) -> Option<(String, String)> {
    let mut files = Vec::new();
    rust_files(root, &root.join("src"), &mut files);
    files.sort();
    files
        .into_iter()
        .filter(|path| !except.contains(&path.as_str()))
        .find_map(|path| {
            let source = fs::read_to_string(root.join(&path)).ok()?;
            permissions
                .iter()
                .find(|permission| source.contains(&format!("\"{permission}\"")))
                .map(|permission| (path, permission.clone()))
        })
}

fn rust_files(root: &Path, directory: &Path, files: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if kind.is_dir() {
            rust_files(root, &path, files);
        } else if kind.is_file() && path.extension().is_some_and(|extension| extension == "rs") {
            files.push(relative(root, &path));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snake_case_names_are_bounded_and_not_keywords() {
        for good in ["pages", "about_us", "a1", "send_welcome"] {
            assert_eq!(snake_case("name", good, MAX_NAME_LENGTH), Ok(()), "{good}");
        }
        for bad in [
            "", "Pages", "1a", "a__b", "a_", "a-b", "a.b", "a/b", "fn", "type", "café",
        ] {
            assert!(snake_case("name", bad, MAX_NAME_LENGTH).is_err(), "{bad}");
        }
        assert!(snake_case("name", &"a".repeat(41), MAX_NAME_LENGTH).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_directories_and_files_are_refused() {
        let directory = tempfile::tempdir().unwrap_or_else(|error| panic!("{error}"));
        let root = directory.path();
        let outside = tempfile::tempdir().unwrap_or_else(|error| panic!("{error}"));
        fs::create_dir_all(root.join("src")).unwrap_or_else(|error| panic!("{error}"));
        std::os::unix::fs::symlink(outside.path(), root.join("templates"))
            .unwrap_or_else(|error| panic!("{error}"));
        std::os::unix::fs::symlink(outside.path().join("x"), root.join("src/a.rs"))
            .unwrap_or_else(|error| panic!("{error}"));
        fs::create_dir_all(root.join("tests/dir.rs")).unwrap_or_else(|error| panic!("{error}"));

        assert!(refuse_symlinks(root, ["templates/pages/home.html"]).is_err());
        assert!(refuse_symlinks(root, ["src/a.rs"]).is_err());
        assert!(refuse_symlinks(root, ["tests/dir.rs"]).is_err());
        assert!(refuse_symlinks(root, ["src/b.rs", "new/dir/file.rs"]).is_ok());
        assert!(refuse_symlinks(root, ["../escape.rs"]).is_err());
    }

    #[test]
    fn permission_owners_are_found_by_string_literal() {
        let directory = tempfile::tempdir().unwrap_or_else(|error| panic!("{error}"));
        let root = directory.path();
        fs::create_dir_all(root.join("src/a")).unwrap_or_else(|error| panic!("{error}"));
        fs::write(root.join("src/a/b.rs"), "const P: &str = \"posts:read\";")
            .unwrap_or_else(|error| panic!("{error}"));
        fs::write(root.join("src/c.rs"), "// posts:write without quotes")
            .unwrap_or_else(|error| panic!("{error}"));
        let permissions = ["posts:read".to_owned(), "posts:write".to_owned()];
        assert_eq!(
            permission_owner(root, &permissions, &[]),
            Some(("src/a/b.rs".to_owned(), "posts:read".to_owned()))
        );
        assert_eq!(permission_owner(root, &permissions, &["src/a/b.rs"]), None);
        assert_eq!(
            permission_owner(root, &["notes:read".to_owned()], &[]),
            None
        );
    }
}
