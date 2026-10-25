//! `forge destroy`, the counterpart of `forge generate` (Rails' `rails
//! destroy`, ADR 0038).
//!
//! It renders what the generator would write from the same arguments and
//! removes exactly those files, and only while each one is still what the
//! generator wrote: a file someone edited is never removed without
//! `--force`. Migrations are kept unless `--migrations` says the database no
//! longer has them, because a migration that already ran is history. Shared
//! files (the access policy, the live-update transport, module directories)
//! are never planned, and nothing is removed through a symlink. Each file is
//! checked again right before it is removed, so one edited (or replaced by
//! a link) between the check and the removal is left alone too.

use std::fs;
use std::io;
use std::path::Path;

use crate::CliError;
use crate::generators::{Planned, Role, refuse_symlinks};

/// What one generator would create, found where it is.
#[derive(Debug)]
pub(crate) struct DestroyPlan {
    pub(crate) files: Vec<Planned>,
    /// The wiring the generator printed, to take out of the composition
    /// root and module declarations by hand.
    pub(crate) unwire: String,
    /// Why the files still belong to, or are needed by, another generation
    /// (a model inside a scaffold, a model whose permission a controller
    /// reuses): the destroy is refused unless `--force`.
    pub(crate) owned_elsewhere: Option<String>,
}

/// The flags every `forge destroy GENERATOR` takes.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct DestroyFlags {
    pub(crate) dry_run: bool,
    pub(crate) force: bool,
    pub(crate) migrations: bool,
}

/// Directories `forge new` creates: never removed, however empty they
/// become, because they are the application's layout.
const DIRECTORIES_KEPT: &[&str] = &[
    "config",
    "config/locales",
    "migrations",
    "src",
    "src/adapters",
    "src/adapters/email",
    "src/adapters/http",
    "src/adapters/http/views",
    "src/application",
    "src/application/ports",
    "src/application/use_cases",
    "src/domain",
    "src/domain/entities",
    "src/infrastructure",
    "src/infrastructure/database",
    "templates",
    "tests",
];

fn refused(reason: impl Into<String>) -> CliError {
    CliError::Destroy(reason.into())
}

/// Removes the files of `plan` from the application at `root` (with
/// `--dry-run`, lists them) and prints what was removed, kept or skipped,
/// then the wiring to take out.
pub(crate) fn apply(
    root: &Path,
    plan: &DestroyPlan,
    flags: DestroyFlags,
    output: &mut dyn io::Write,
) -> Result<(), CliError> {
    if !flags.force
        && let Some(reason) = &plan.owned_elsewhere
    {
        return Err(refused(format!(
            "refusing to destroy, nothing was removed: {reason}"
        )));
    }
    let mut actions: Vec<(String, &'static str, &'static str)> = Vec::new();
    let mut kept = Vec::new();
    let mut modified = Vec::new();
    // The path and the bytes that were checked: removal needs them unchanged.
    let mut removable: Vec<(String, Vec<u8>)> = Vec::new();
    for file in &plan.files {
        if file.role == Role::Shared {
            continue;
        }
        refuse_symlinks(root, [file.path.as_str()])
            .map_err(|reason| refused(format!("refusing to destroy: {reason}")))?;
        let path = root.join(&file.path);
        let current = match fs::read(&path) {
            Ok(current) => current,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                actions.push((file.path.clone(), "skip", "not found"));
                continue;
            }
            Err(error) => {
                return Err(refused(format!("could not read {}: {error}", file.path)));
            }
        };
        if file.role == Role::Migration && !flags.migrations {
            actions.push((
                file.path.clone(),
                "keep",
                "a migration may already have run: `forge rollback` first, then pass --migrations",
            ));
            kept.push(file.path.clone());
            continue;
        }
        if current == file.content.as_bytes() {
            actions.push((file.path.clone(), "remove", ""));
        } else if file.role == Role::Translation && !flags.force {
            actions.push((
                file.path.clone(),
                "keep",
                "translated since it was generated: remove it by hand if no longer needed",
            ));
            kept.push(file.path.clone());
            continue;
        } else {
            modified.push(file.path.clone());
            if !flags.force {
                continue;
            }
            actions.push((
                file.path.clone(),
                "remove",
                "differs from what the generator writes; removed because of --force",
            ));
        }
        removable.push((file.path.clone(), current));
    }
    if !modified.is_empty() && !flags.force {
        return Err(refused(format!(
            "refusing to destroy: {} file(s) differ from what this generator would write from \
             these arguments (edited since, generated with other arguments, or written by \
             another generator), so nothing was removed:\n  {}\nReview them, or pass --force \
             to remove them anyway",
            modified.len(),
            modified.join("\n  ")
        )));
    }
    if removable.is_empty() && !kept.is_empty() {
        return Err(refused(format!(
            "nothing to destroy but files that are kept:\n  {}\nRevert the migrations with \
             `forge rollback` if they ran, then run this command again with --migrations",
            kept.join("\n  ")
        )));
    }
    if removable.is_empty() {
        return Err(refused(
            "nothing to destroy: none of the files this generator creates exist here (check \
             the arguments: they must be the ones the generator was run with)",
        ));
    }
    if !flags.dry_run {
        let mut removed: Vec<String> = Vec::new();
        for (path, checked) in &removable {
            if let Err(reason) = remove_checked(root, path, checked) {
                remove_empty_parents(root, &removed);
                return Err(refused(format!(
                    "{reason}; {}",
                    if removed.is_empty() {
                        "nothing was removed".to_owned()
                    } else {
                        format!("removed before it: {}", removed.join(", "))
                    }
                )));
            }
            removed.push(path.clone());
        }
        remove_empty_parents(root, &removed);
    }
    for (path, verb, reason) in &actions {
        let verb = if flags.dry_run && *verb == "remove" {
            "would remove"
        } else {
            verb
        };
        let line = if reason.is_empty() {
            format!("  {verb:<12} {path}")
        } else {
            format!("  {verb:<12} {path}  ({reason})")
        };
        writeln!(output, "{line}").map_err(CliError::Output)?;
    }
    let wiring = plan
        .unwire
        .trim_start_matches('\n')
        .split_once('\n')
        .map_or(plan.unwire.as_str(), |(_, rest)| rest);
    write!(
        output,
        "\nForge did not edit any existing file. Take out the wiring you added by hand, which \
         the generator printed as:\n{wiring}\n  Then run `forge test`.\n"
    )
    .map_err(CliError::Output)
}

/// Removes `path` only if it is still, right now, what was checked: not a
/// symlink, and byte-identical to `checked`. The check ran earlier, and a
/// file edited or swapped for a link since then (by an editor, a build or
/// another process) must not be removed on the strength of it. A file that
/// is already gone counts as removed. A final race between this check and
/// the unlink cannot be closed without a descriptor-relative unlink, which
/// std does not offer; the window is the width of one read. In that window a
/// parent directory swapped for a symlink would make the unlink go through
/// the link and remove a same-named file outside the application, which
/// takes someone with write access racing the command.
fn remove_checked(root: &Path, path: &str, checked: &[u8]) -> Result<(), String> {
    refuse_symlinks(root, [path])
        .map_err(|reason| format!("refusing to remove {path}: {reason}"))?;
    let full = root.join(path);
    match fs::read(&full) {
        Ok(current) if current == checked => {}
        Ok(_) => {
            return Err(format!(
                "refusing to remove {path}: it changed while the command was running"
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("could not read {path} again: {error}")),
    }
    match fs::remove_file(&full) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("could not remove {path}: {error}")),
    }
}

/// Removes the directories the removed files were in, and their parents,
/// while they are empty and not part of the application's layout.
fn remove_empty_parents(root: &Path, removed: &[String]) {
    let mut directories: Vec<String> = Vec::new();
    for path in removed {
        let mut current = Path::new(path).parent();
        while let Some(directory) = current {
            let name = directory.to_string_lossy().replace('\\', "/");
            if name.is_empty() || DIRECTORIES_KEPT.contains(&name.as_str()) {
                break;
            }
            if !directories.contains(&name) {
                directories.push(name);
            }
            current = directory.parent();
        }
    }
    // Deepest first, so a directory is empty once its children are gone.
    directories.sort_by_key(|directory| std::cmp::Reverse(directory.len()));
    for directory in directories {
        let path = root.join(&directory);
        let empty = fs::read_dir(&path).is_ok_and(|mut entries| entries.next().is_none());
        if empty {
            let _ = fs::remove_dir(&path);
        }
    }
}

#[cfg(test)]
mod tests;
