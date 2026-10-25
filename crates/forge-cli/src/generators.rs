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
/// `permissions`, and the permission.
///
/// Every permission has exactly one owner, the generated module that
/// declares its constant: a controller reuses a model's `READ_PERMISSION`
/// instead of declaring it again, and a model or scaffold refuses a plural
/// whose permissions another module already declares (ADR 0038). Any
/// mention counts, so hand-written code is found too: a string literal
/// (`"posts:read"`), or a name assembled by `concat!`/`format!` from the
/// literals `"posts"` and `":read"` (best effort: a name built at runtime
/// cannot be found by reading source).
///
/// The scan fails closed. A symbolic link to a directory or a `.rs` file, or
/// a file or directory that cannot be read, is an error rather than a
/// silent gap, because the code behind it could declare the permission. A
/// file that is not UTF-8 is still searched.
pub(crate) fn permission_owner(
    root: &Path,
    permissions: &[String],
    except: &[&str],
) -> Result<Option<(String, String)>, String> {
    let mut files = Vec::new();
    rust_files(root, &root.join("src"), &mut files)?;
    files.sort();
    for path in files
        .into_iter()
        .filter(|path| !except.contains(&path.as_str()))
    {
        let bytes = fs::read(root.join(&path))
            .map_err(|error| format!("could not read {path} to look for permissions: {error}"))?;
        let source = String::from_utf8_lossy(&bytes);
        if let Some(permission) = permissions
            .iter()
            .find(|permission| mentions(&source, permission))
        {
            return Ok(Some((path, permission.clone())));
        }
    }
    Ok(None)
}

/// Whether `source` names `permission` (`posts:read`) as a literal or as
/// the pieces of a `concat!`/`format!`.
fn mentions(source: &str, permission: &str) -> bool {
    if source.contains(&format!("\"{permission}\"")) {
        return true;
    }
    let Some((namespace, action)) = permission.split_once(':') else {
        return false;
    };
    (source.contains("concat!") || source.contains("format!"))
        && (source.contains(&format!("\"{namespace}\""))
            || source.contains(&format!("\"{namespace}:")))
        && source.contains(&format!(":{action}\""))
}

fn rust_files(root: &Path, directory: &Path, files: &mut Vec<String>) -> Result<(), String> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(format!(
                "could not read {} to look for permissions: {error}",
                relative(root, directory)
            ));
        }
    };
    for entry in entries {
        let entry = entry.map_err(|error| {
            format!(
                "could not read {} to look for permissions: {error}",
                relative(root, directory)
            )
        })?;
        let path = entry.path();
        let shown = relative(root, &path);
        let kind = entry
            .file_type()
            .map_err(|error| format!("could not inspect {shown}: {error}"))?;
        if kind.is_symlink() {
            // A link to code (a directory or a Rust file) is code this scan
            // cannot vouch for; a link to anything else, or a dangling one,
            // holds none.
            let holds_code = fs::metadata(&path)
                .is_ok_and(|target| target.is_dir() || path.extension().is_some_and(|e| e == "rs"));
            if holds_code {
                return Err(format!(
                    "{shown} is a symbolic link under src/: the code behind it could already \
                     declare a permission, and the generator does not follow links. Replace \
                     the link with the real files (or move it aside) and run the command again"
                ));
            }
        } else if kind.is_dir() {
            rust_files(root, &path, files)?;
        } else if kind.is_file() && path.extension().is_some_and(|extension| extension == "rs") {
            files.push(shown);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
