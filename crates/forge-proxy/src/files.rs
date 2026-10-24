//! Files on the proxy's volume: private directories and atomic, durable
//! writes. Every function here blocks; async callers run them on the
//! blocking pool or at startup, before serving.

use std::io::Write as _;
use std::os::unix::fs::{
    DirBuilderExt as _, MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _,
};
use std::path::{Path, PathBuf};

/// Makes `path` a directory only the proxy's user can use (0700). A missing
/// directory is created with that mode; an existing one is restricted to
/// it, because `DirBuilder::mode` applies only to directories it creates.
///
/// The directory is inspected and changed through one open handle (`fstat`,
/// `fchmod`), so the path cannot be swapped between the check and the
/// change. std cannot open a directory with `O_NOFOLLOW` without a new
/// dependency, so a symbolic link is refused beforehand with
/// `symlink_metadata`, and the handle must be the very directory that call
/// saw (same device and inode). A directory owned by another user is
/// refused where the effective uid is known (`/proc/self/status`, Linux).
///
/// # Errors
///
/// The directory cannot be created or opened, is a link or not a directory,
/// changed while it was checked, belongs to another user, or cannot be
/// restricted.
pub(crate) fn private_directory(path: &Path) -> Result<(), String> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .map_err(|error| format!("create {}: {error}", path.display()))?;
    let before = std::fs::symlink_metadata(path)
        .map_err(|error| format!("inspect {}: {error}", path.display()))?;
    if !before.file_type().is_dir() {
        return Err(format!("{} is not a directory", path.display()));
    }
    let handle =
        std::fs::File::open(path).map_err(|error| format!("open {}: {error}", path.display()))?;
    let metadata = handle
        .metadata()
        .map_err(|error| format!("inspect {}: {error}", path.display()))?;
    if !metadata.is_dir() || metadata.dev() != before.dev() || metadata.ino() != before.ino() {
        return Err(format!("{} changed while it was checked", path.display()));
    }
    if let Some(uid) = effective_uid()
        && metadata.uid() != uid
    {
        return Err(format!(
            "{} is owned by uid {}, not by the proxy's uid {uid}",
            path.display(),
            metadata.uid()
        ));
    }
    if metadata.permissions().mode() & 0o7777 != 0o700 {
        handle
            .set_permissions(std::fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("restrict {} to 0700: {error}", path.display()))?;
    }
    Ok(())
}

/// The process's effective uid from `/proc/self/status` (std has no
/// `geteuid`); `None` where that file is missing (not Linux).
fn effective_uid() -> Option<u32> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    // `Uid:` real, effective, saved set, file system.
    status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

/// The outcome of [`write_atomically`]: the new content is in place.
#[must_use]
#[derive(Debug)]
pub(crate) struct Written {
    /// Why the directory could not be synced after the rename, if it could
    /// not: the new content is in place but may not survive a crash. Callers
    /// log it rather than fail.
    pub(crate) unsynced: Option<std::io::Error>,
}

/// Suffix of temporary files: `.NAME.<12 hex digits>.tmp`.
const TEMPORARY_SUFFIX: &str = ".tmp";

/// Writes `content` to a new 0600 temporary file next to `path` (named
/// `.NAME.<random>.tmp` after the file it replaces), syncs it, renames it
/// over `path` and syncs the directory, so the new content survives a crash
/// once this returns.
///
/// # Errors
///
/// The temporary file cannot be written or synced, or the rename failed;
/// `path` is unchanged then. A failed directory sync is reported in
/// [`Written::unsynced`] instead.
pub(crate) fn write_atomically(path: &Path, content: &[u8]) -> std::io::Result<Written> {
    let directory = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let purpose = path
        .file_name()
        .map_or_else(|| "file".into(), |name| name.to_string_lossy());
    let mut suffix = [0_u8; 6];
    getrandom::fill(&mut suffix).map_err(|error| std::io::Error::other(error.to_string()))?;
    let random: String = suffix.iter().map(|b| format!("{b:02x}")).collect();
    let temporary = directory.join(format!(".{purpose}.{random}{TEMPORARY_SUFFIX}"));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true).mode(0o600);
    let result = (|| {
        let mut file = options.open(&temporary)?;
        file.write_all(content)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result?;
    Ok(Written {
        unsynced: sync_directory(directory).err(),
    })
}

/// Whether `name` is a temporary file [`write_atomically`] leaves behind
/// when the process dies between creating and renaming it, or one of the
/// `.state-<12 hex>.json` names earlier versions used.
fn is_temporary(name: &str) -> bool {
    let hex = |text: &str| text.len() == 12 && text.bytes().all(|b| b.is_ascii_hexdigit());
    if let Some(random) = name
        .strip_prefix(".state-")
        .and_then(|rest| rest.strip_suffix(".json"))
    {
        return hex(random);
    }
    name.strip_prefix('.')
        .and_then(|rest| rest.strip_suffix(TEMPORARY_SUFFIX))
        .and_then(|rest| rest.rsplit_once('.'))
        .is_some_and(|(purpose, random)| !purpose.is_empty() && hex(random))
}

/// Removes the regular files in `directory` that [`is_temporary`] names,
/// left by a process that died mid-write. Run at start-up, before anything
/// writes there. Returns the paths removed; files that cannot be removed
/// are left alone.
pub(crate) fn remove_stale_temporaries(directory: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut removed = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !is_temporary(name) {
            continue;
        }
        let path = entry.path();
        let regular = std::fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.is_file());
        if regular && std::fs::remove_file(&path).is_ok() {
            removed.push(path);
        }
    }
    removed.sort();
    removed
}

/// Logs a [`Written::unsynced`] failure as a warning.
pub(crate) fn report(logger: &crate::log::Logger, path: &Path, written: Written) {
    if let Some(error) = written.unsynced {
        logger.warn(
            "directory not synced after a write; the file may not survive a crash",
            &[
                ("path", crate::log::text(path.display().to_string())),
                ("error", crate::log::text(error.to_string())),
            ],
        );
    }
}

/// [`remove_stale_temporaries`], logging what it removed.
pub(crate) fn clean_up(logger: &crate::log::Logger, directory: &Path) {
    for path in remove_stale_temporaries(directory) {
        logger.info(
            "removed a temporary file left by an interrupted write",
            &[("path", crate::log::text(path.display().to_string()))],
        );
    }
}

/// Flushes a directory's entries (a rename in it) to disk. File systems
/// that cannot sync a directory (`EINVAL`) are accepted, as the rename is
/// all they offer.
fn sync_directory(directory: &Path) -> std::io::Result<()> {
    match std::fs::File::open(directory).and_then(|handle| handle.sync_all()) {
        Err(error) if error.kind() == std::io::ErrorKind::InvalidInput => Ok(()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(path: &Path) -> u32 {
        std::fs::symlink_metadata(path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o7777
    }

    #[test]
    fn private_directories_are_created_and_existing_ones_restricted() {
        let root = tempfile::tempdir().expect("tempdir");
        let created = root.path().join("a/b");
        private_directory(&created).expect("create");
        assert_eq!(mode(&created), 0o700);

        let existing = root.path().join("open");
        std::fs::create_dir(&existing).expect("mkdir");
        std::fs::set_permissions(&existing, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        private_directory(&existing).expect("restrict");
        assert_eq!(mode(&existing), 0o700);

        let sticky = root.path().join("sticky");
        std::fs::create_dir(&sticky).expect("mkdir");
        std::fs::set_permissions(&sticky, std::fs::Permissions::from_mode(0o1700)).expect("chmod");
        private_directory(&sticky).expect("restrict");
        assert_eq!(mode(&sticky), 0o700);
    }

    #[test]
    fn links_and_files_are_not_private_directories() {
        let root = tempfile::tempdir().expect("tempdir");
        let target = root.path().join("target");
        std::fs::create_dir(&target).expect("mkdir");
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let link = root.path().join("link");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");
        let error = private_directory(&link).expect_err("symlink refused");
        assert!(error.contains("is not a directory"), "{error}");
        // The link's target was left alone.
        assert_eq!(mode(&target), 0o755);

        let file = root.path().join("file");
        std::fs::write(&file, b"x").expect("write");
        assert!(private_directory(&file).is_err());
    }

    #[test]
    fn atomic_writes_replace_the_file_with_a_private_one() {
        let root = tempfile::tempdir().expect("tempdir");
        let path = root.path().join("state.json");
        std::fs::write(&path, b"old").expect("write");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        let written = write_atomically(&path, b"new").expect("write atomically");
        assert!(written.unsynced.is_none(), "{:?}", written.unsynced);
        assert_eq!(std::fs::read(&path).expect("read"), b"new");
        assert_eq!(mode(&path), 0o600);
        let names: Vec<_> = std::fs::read_dir(root.path())
            .expect("read dir")
            .map(|entry| entry.expect("entry").file_name())
            .collect();
        assert_eq!(names, ["state.json"], "temporary file left behind");
    }

    #[test]
    fn a_relative_path_without_directory_syncs_the_working_directory() {
        assert_eq!(
            Path::new("state.json").parent(),
            Some(Path::new("")),
            "the empty parent is mapped to ."
        );
        sync_directory(Path::new(".")).expect("sync working directory");
    }

    #[test]
    fn a_directory_of_another_user_is_refused() {
        let Some(uid) = effective_uid() else {
            eprintln!("skipped: the effective uid is unknown here");
            return;
        };
        if uid != 0 {
            eprintln!("skipped: needs root to give a directory to another user");
            return;
        }
        let root = tempfile::tempdir().expect("tempdir");
        let foreign = root.path().join("foreign");
        std::fs::create_dir(&foreign).expect("mkdir");
        std::os::unix::fs::chown(&foreign, Some(54321), Some(54321)).expect("chown");
        std::fs::set_permissions(&foreign, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let error = private_directory(&foreign).expect_err("foreign directory refused");
        assert!(error.contains("is owned by uid 54321"), "{error}");
        assert_eq!(mode(&foreign), 0o755, "a refused directory is left alone");
    }

    #[test]
    fn the_effective_uid_is_read_where_proc_exists() {
        if Path::new("/proc/self/status").exists() {
            let uid = effective_uid().expect("uid from /proc/self/status");
            let probe = tempfile::NamedTempFile::new().expect("probe");
            assert_eq!(probe.as_file().metadata().expect("metadata").uid(), uid);
        }
    }

    #[test]
    fn temporary_files_are_named_after_their_purpose_and_cleaned_up() {
        let root = tempfile::tempdir().expect("tempdir");
        let path = root.path().join("app.example.com");
        // A temporary file of a write that died before its rename.
        let leftover = root.path().join(".app.example.com.0123456789ab.tmp");
        std::fs::write(&leftover, b"half").expect("write");
        let legacy = root.path().join(".state-a1b2c3d4e5f6.json");
        std::fs::write(&legacy, b"half").expect("write");
        // Not temporaries of ours.
        for kept in [
            "app.example.com",
            ".app.example.com.tmp",
            ".x.0123456789ab.tmp.bak",
            ".state-zz.json",
            "acme_account+key",
        ] {
            std::fs::write(root.path().join(kept), b"keep").expect("write");
        }
        let link = root.path().join(".link.0123456789ab.tmp");
        std::os::unix::fs::symlink(&path, &link).expect("symlink");
        assert_eq!(
            remove_stale_temporaries(root.path()),
            vec![leftover.clone(), legacy.clone()]
        );
        assert!(!leftover.exists() && !legacy.exists());
        assert!(
            std::fs::symlink_metadata(&link).is_ok(),
            "links are left alone"
        );
        assert!(is_temporary(".acme_account+key.ffffffffffff.tmp"));
        assert!(is_temporary(".state.json.000000000000.tmp"));
        assert!(!is_temporary("..0123456789ab.tmp"));
        let written = write_atomically(&path, b"new").expect("write");
        assert!(written.unsynced.is_none());
        assert!(remove_stale_temporaries(root.path()).is_empty());
    }
}
