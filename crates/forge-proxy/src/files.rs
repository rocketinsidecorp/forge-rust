//! Files on the proxy's volume: private directories and atomic, durable
//! writes. Every function here blocks; async callers run them on the
//! blocking pool or at startup, before serving.

use std::io::Write as _;
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::Path;

/// Makes `path` a directory only its owner can use (0700). A missing
/// directory is created with that mode; an existing one is restricted to
/// it, because `DirBuilder::mode` applies only to directories it creates. A
/// symbolic link or any other kind of file at `path` is refused, and so is
/// a directory the proxy cannot restrict (owned by someone else).
///
/// # Errors
///
/// The directory cannot be created, is not a directory, or cannot be
/// restricted.
pub(crate) fn private_directory(path: &Path) -> Result<(), String> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .map_err(|error| format!("create {}: {error}", path.display()))?;
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("inspect {}: {error}", path.display()))?;
    if !metadata.file_type().is_dir() {
        return Err(format!("{} is not a directory", path.display()));
    }
    if metadata.permissions().mode() & 0o7777 != 0o700 {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("restrict {} to 0700: {error}", path.display()))?;
    }
    Ok(())
}

/// Writes `content` to a new 0600 file next to `path`, syncs it, renames it
/// over `path` and syncs the directory, so the new content survives a
/// crash once this returns.
pub(crate) fn write_atomically(path: &Path, content: &[u8]) -> std::io::Result<()> {
    let directory = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let mut suffix = [0_u8; 6];
    getrandom::fill(&mut suffix).map_err(|error| std::io::Error::other(error.to_string()))?;
    let name: String = suffix.iter().map(|b| format!("{b:02x}")).collect();
    let temporary = directory.join(format!(".state-{name}.json"));
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
    sync_directory(directory)
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
        write_atomically(&path, b"new").expect("write atomically");
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
}
