//! The local-disk adapter (Unix).
//!
//! Files live under one root directory. Every path below the root is resolved
//! with `openat`-style calls relative to directory file descriptors, one
//! component at a time, with `O_NOFOLLOW` on **every** component: the operating
//! system itself refuses a symbolic link anywhere below the root, with no
//! window between "check" and "use" (a canonicalize-and-compare would have one),
//! on top of [`StorageKey`] which rejects `..`, hidden names and separators
//! before any file-system call. The directory descriptors of `blobs/`, `meta/`
//! and `tmp/` are opened once, at construction, so replacing one of those
//! directories with a link later changes nothing.
//!
//! Writes are atomic: the content goes to `tmp/<unique>` created with
//! `O_EXCL`, is flushed (`fsync`) and renamed over the final name, then the
//! directory is flushed, so readers see the old file or the new one, never a
//! partial one, and a crash leaves at most an orphaned temporary file (see
//! [`DiskStorage::prune_temp`]). Files are created `0600` and directories
//! `0750`.
//!
//! Layout below the root:
//!
//! ```text
//! blobs/<key>        the content
//! meta/<key>.json    content type, file name, size and SHA-256 digest
//! tmp/               uploads in progress
//! ```
//!
//! The metadata is renamed into place before the content, so a blob is never
//! visible with metadata of a different upload except in the instant between
//! two overwrites of one key; keys of user content are unique (UUIDv7), so
//! overwrites are the exception. A blob whose metadata is missing or does not
//! match its size is served as `application/octet-stream`, which the handler
//! downloads.
//!
//! Disk storage suits one application host (or hosts sharing a volume with
//! working `rename` and `fsync` semantics, not NFS). Blocking file-system work
//! runs on Tokio's blocking pool and is awaited by the caller (never
//! detached); dropping the future abandons the result, not the syscall in
//! flight, and `tmp/` cleanup covers what it leaves.

use std::{
    fs::File,
    io::{self, Read, Write},
    os::fd::OwnedFd,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use bytes::Bytes;
use rustix::{
    fs::{AtFlags, Dir, Mode, OFlags, mkdirat, openat, renameat, statat, unlinkat},
    io::Errno,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    Blob, Info, OCTET_STREAM, PutOptions, Storage, StorageError, StorageKey,
    normalize_content_type, sanitize_filename,
};

const DIR_MODE: u32 = 0o750;
const FILE_MODE: u32 = 0o600;
/// Bounds the metadata read for one blob.
const MAX_META_BYTES: u64 = 8 << 10;
/// Uploads younger than this are never pruned: they may still be in progress.
const MIN_PRUNE_AGE: Duration = Duration::from_secs(60);

/// A [`Storage`] on a local directory. Cheap to clone; safe for concurrent use.
#[derive(Clone, Debug)]
pub struct DiskStorage {
    inner: Arc<Dirs>,
}

#[derive(Debug)]
struct Dirs {
    blobs: OwnedFd,
    meta: OwnedFd,
    tmp: OwnedFd,
}

#[derive(Debug, Deserialize, Serialize)]
struct Metadata {
    content_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    filename: Option<String>,
    sha256: String,
    size: u64,
}

fn io_error(error: impl Into<io::Error>) -> StorageError {
    StorageError::Io(error.into())
}

fn mode(bits: u32) -> Mode {
    Mode::from_bits_truncate(bits)
}

/// Directory flags: no links, no controlling terminal, closed on exec.
fn dir_flags() -> OFlags {
    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
}

/// Whether an `openat` failure means "there is nothing readable here": a
/// missing path, a component that is a file, or a symbolic link (refused by
/// `O_NOFOLLOW`).
fn is_absent(error: Errno) -> bool {
    matches!(error, Errno::NOENT | Errno::NOTDIR | Errno::LOOP)
}

impl DiskStorage {
    /// Opens (creating it if needed) the storage directory `root`. A relative
    /// path is resolved against the working directory, once.
    pub fn open(root: impl AsRef<Path>) -> Result<Self, StorageError> {
        let root = root.as_ref();
        if root.as_os_str().is_empty() {
            return Err(StorageError::Configuration(
                "storage root directory is required",
            ));
        }
        let mut builder = std::fs::DirBuilder::new();
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.recursive(true).mode(DIR_MODE);
        }
        builder.create(root).map_err(io_error)?;
        let root_fd = rustix::fs::open(root, dir_flags() & !OFlags::NOFOLLOW, Mode::empty())
            .map_err(io_error)?;
        let open_subdir = |name: &str| -> Result<OwnedFd, StorageError> {
            match mkdirat(&root_fd, name, mode(DIR_MODE)) {
                Ok(()) | Err(Errno::EXIST) => {}
                Err(error) => return Err(io_error(error)),
            }
            // NOFOLLOW: a planted link in place of a storage directory is refused.
            openat(&root_fd, name, dir_flags(), Mode::empty()).map_err(io_error)
        };
        Ok(Self {
            inner: Arc::new(Dirs {
                blobs: open_subdir("blobs")?,
                meta: open_subdir("meta")?,
                tmp: open_subdir("tmp")?,
            }),
        })
    }

    /// Opens the directory of `FORGE_STORAGE_ROOT`.
    pub fn from_config(config: &forge_config::StorageConfig) -> Result<Self, StorageError> {
        match &config.root {
            Some(root) => Self::open(root),
            None => Err(StorageError::Configuration(
                "FORGE_STORAGE_ROOT is required for disk storage",
            )),
        }
    }

    /// Removes temporary files older than `older_than` (at least one minute),
    /// left by uploads that crashed, and returns how many. Call it periodically
    /// (for example from a scheduled job) with a generous age, such as an hour.
    pub async fn prune_temp(&self, older_than: Duration) -> Result<usize, StorageError> {
        if older_than < MIN_PRUNE_AGE {
            return Err(StorageError::Configuration(
                "storage temporary file age must be at least one minute",
            ));
        }
        let dirs = Arc::clone(&self.inner);
        run(move || dirs.prune_temp(older_than)).await
    }
}

/// Runs blocking work on the blocking pool and awaits it.
async fn run<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, StorageError> + Send + 'static,
) -> Result<T, StorageError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|_| StorageError::Io(io::Error::other("storage worker failed")))?
}

impl Dirs {
    /// Opens the directory that holds the leaf of `key` below `tree`,
    /// component by component without following links; with `create`, missing
    /// directories are made.
    fn parent(&self, tree: &OwnedFd, key: &StorageKey, create: bool) -> Result<OwnedFd, Errno> {
        let mut segments = key.segments().collect::<Vec<_>>();
        segments.pop(); // the leaf
        let mut current = rustix::io::fcntl_dupfd_cloexec(tree, 0)?;
        for segment in segments {
            if create {
                match mkdirat(&current, segment, mode(DIR_MODE)) {
                    Ok(()) | Err(Errno::EXIST) => {}
                    Err(error) => return Err(error),
                }
            }
            current = openat(&current, segment, dir_flags(), Mode::empty())?;
        }
        Ok(current)
    }

    fn leaf(key: &StorageKey) -> &str {
        key.as_str().rsplit('/').next().unwrap_or("")
    }

    fn meta_leaf(key: &StorageKey) -> String {
        format!("{}.json", Self::leaf(key))
    }

    /// Writes the content and metadata of one blob to `tmp/` without publishing
    /// anything: the first half of a put.
    fn stage<'a>(
        &'a self,
        key: &StorageKey,
        body: &[u8],
        options: &PutOptions,
    ) -> Result<Staged<'a>, StorageError> {
        if body.len() as u64 > options.max_bytes() {
            return Err(StorageError::TooLarge);
        }
        let content_type = normalize_content_type(options.declared_content_type().unwrap_or(""))?;
        let filename = options
            .declared_filename()
            .map(sanitize_filename)
            .filter(|name| !name.is_empty());
        let digest = hex(&Sha256::digest(body));
        let metadata = Metadata {
            content_type: content_type.clone(),
            filename: filename.clone(),
            sha256: digest.clone(),
            size: body.len() as u64,
        };
        let encoded = serde_json::to_vec(&metadata)
            .map_err(|error| StorageError::Io(io::Error::other(error)))?;

        let temp = uuid::Uuid::now_v7().simple().to_string();
        // Whatever a failure leaves behind goes (the guard unlinks on drop).
        let staged = Staged {
            dirs: self,
            key: key.clone(),
            temp_meta: format!("{temp}.meta"),
            backup: format!("{temp}.old"),
            temp,
            info: Info {
                key: key.clone(),
                size: body.len() as u64,
                content_type,
                filename,
                sha256: Some(digest),
                modified: SystemTime::now(),
            },
        };
        self.write_temp(&staged.temp, body)?;
        self.write_temp(&staged.temp_meta, &encoded)?;
        Ok(staged)
    }

    /// Publishes a staged blob: the metadata first, the blob is the commit
    /// point. The previous metadata is kept aside, so a failed overwrite puts it
    /// back instead of losing it.
    fn promote(&self, staged: &Staged<'_>) -> Result<(), StorageError> {
        let key = &staged.key;
        let blob_dir = self.parent(&self.blobs, key, true).map_err(io_error)?;
        let meta_dir = self.parent(&self.meta, key, true).map_err(io_error)?;
        let meta_leaf = Self::meta_leaf(key);
        let had_old = match renameat(
            &meta_dir,
            meta_leaf.as_str(),
            &self.tmp,
            staged.backup.as_str(),
        ) {
            Ok(()) => true,
            Err(Errno::NOENT) => false,
            Err(error) => return Err(io_error(error)),
        };
        let restore = || {
            if had_old {
                let _ = renameat(
                    &self.tmp,
                    staged.backup.as_str(),
                    &meta_dir,
                    meta_leaf.as_str(),
                );
            }
        };
        if let Err(error) = renameat(
            &self.tmp,
            staged.temp_meta.as_str(),
            &meta_dir,
            meta_leaf.as_str(),
        ) {
            restore();
            return Err(io_error(error));
        }
        if let Err(error) = renameat(&self.tmp, staged.temp.as_str(), &blob_dir, Self::leaf(key)) {
            // The new metadata describes a blob that never arrived: drop it and
            // bring the previous record back.
            let _ = unlinkat(&meta_dir, meta_leaf.as_str(), AtFlags::empty());
            restore();
            return Err(io_error(error));
        }
        for directory in [&blob_dir, &meta_dir] {
            // Best effort: the rename is already atomic; this makes it durable.
            let _ = rustix::fs::fsync(directory);
        }
        Ok(())
    }

    /// Removes a blob and its metadata that were just published (rollback of a
    /// multi-file commit; the keys are fresh).
    fn unpublish(&self, key: &StorageKey) {
        let _ = self.delete(key);
    }

    fn put(
        &self,
        key: &StorageKey,
        body: &[u8],
        options: &PutOptions,
    ) -> Result<Info, StorageError> {
        let staged = self.stage(key, body, options)?;
        self.promote(&staged)?;
        Ok(staged.info.clone())
    }

    /// Stages every file, then publishes them all, or none: a failure removes
    /// what was published, and a cancelled request (`cancelled`, set when the
    /// awaiting future is dropped) discards the staged files before anything is
    /// published. `before_promote` runs between the two phases.
    fn put_all(
        &self,
        items: &[(StorageKey, Bytes, PutOptions)],
        cancelled: &AtomicBool,
        before_promote: &dyn Fn(),
    ) -> Result<Vec<Info>, StorageError> {
        let abandoned = || StorageError::Io(io::Error::other("upload cancelled"));
        let mut staged = Vec::with_capacity(items.len());
        for (key, body, options) in items {
            if cancelled.load(Ordering::SeqCst) {
                return Err(abandoned());
            }
            staged.push(self.stage(key, body, options)?);
        }
        before_promote();
        if cancelled.load(Ordering::SeqCst) {
            return Err(abandoned());
        }
        for (index, file) in staged.iter().enumerate() {
            if let Err(error) = self.promote(file) {
                for published in &staged[..index] {
                    self.unpublish(&published.key);
                }
                return Err(error);
            }
        }
        Ok(staged.iter().map(|file| file.info.clone()).collect())
    }

    fn write_temp(&self, name: &str, content: &[u8]) -> Result<(), StorageError> {
        let descriptor = openat(
            &self.tmp,
            name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            mode(FILE_MODE),
        )
        .map_err(io_error)?;
        let mut file = File::from(descriptor);
        file.write_all(content).map_err(io_error)?;
        file.sync_all().map_err(io_error)
    }

    fn open_leaf(
        &self,
        tree: &OwnedFd,
        key: &StorageKey,
        leaf: &str,
    ) -> Result<File, StorageError> {
        let absent = |error: Errno| {
            if is_absent(error) {
                StorageError::NotFound
            } else {
                io_error(error)
            }
        };
        let parent = self.parent(tree, key, false).map_err(absent)?;
        // NONBLOCK: a FIFO planted under a key cannot hang the worker.
        let descriptor = openat(
            &parent,
            leaf,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::empty(),
        )
        .map_err(absent)?;
        Ok(File::from(descriptor))
    }

    fn describe(&self, key: &StorageKey, file: &File) -> Result<Info, StorageError> {
        let stat = file.metadata().map_err(io_error)?;
        if !stat.is_file() {
            return Err(StorageError::NotFound);
        }
        let mut info = Info {
            key: key.clone(),
            size: stat.len(),
            content_type: OCTET_STREAM.to_owned(),
            filename: None,
            sha256: None,
            modified: stat.modified().unwrap_or(UNIX_EPOCH),
        };
        // Metadata is trusted only while its recorded size equals the file's.
        let Ok(meta_file) = self.open_leaf(&self.meta, key, &Self::meta_leaf(key)) else {
            return Ok(info);
        };
        let mut raw = Vec::new();
        if meta_file
            .take(MAX_META_BYTES)
            .read_to_end(&mut raw)
            .is_err()
        {
            return Ok(info);
        }
        let Ok(meta) = serde_json::from_slice::<Metadata>(&raw) else {
            return Ok(info);
        };
        if meta.size != stat.len() {
            return Ok(info);
        }
        if let Ok(content_type) = normalize_content_type(&meta.content_type) {
            info.content_type = content_type;
        }
        info.filename = meta
            .filename
            .map(|name| sanitize_filename(&name))
            .filter(|name| !name.is_empty());
        info.sha256 = Some(meta.sha256);
        Ok(info)
    }

    fn open(&self, key: &StorageKey, max_bytes: u64) -> Result<Blob, StorageError> {
        let file = self.open_leaf(&self.blobs, key, Self::leaf(key))?;
        let info = self.describe(key, &file)?;
        if info.size > max_bytes {
            return Err(StorageError::TooLarge);
        }
        let mut body = Vec::with_capacity(usize::try_from(info.size).unwrap_or(0));
        // One byte past the size: a file that grew since `stat` is refused, not truncated.
        file.take(info.size + 1)
            .read_to_end(&mut body)
            .map_err(io_error)?;
        if body.len() as u64 != info.size {
            return Err(StorageError::Io(io::Error::other(
                "file changed while it was read",
            )));
        }
        Ok(Blob {
            info,
            body: Bytes::from(body),
        })
    }

    fn stat(&self, key: &StorageKey) -> Result<Info, StorageError> {
        let file = self.open_leaf(&self.blobs, key, Self::leaf(key))?;
        self.describe(key, &file)
    }

    fn delete(&self, key: &StorageKey) -> Result<(), StorageError> {
        // The blob first: once it is gone the key is gone, whatever happens to
        // the metadata after.
        for (tree, leaf) in [
            (&self.blobs, Self::leaf(key).to_owned()),
            (&self.meta, Self::meta_leaf(key)),
        ] {
            let parent = match self.parent(tree, key, false) {
                Ok(parent) => parent,
                Err(Errno::NOENT) => continue,
                Err(error) => return Err(io_error(error)),
            };
            match unlinkat(&parent, leaf.as_str(), AtFlags::empty()) {
                Ok(()) | Err(Errno::NOENT) => {}
                Err(error) => return Err(io_error(error)),
            }
        }
        Ok(())
    }

    fn prune_temp(&self, older_than: Duration) -> Result<usize, StorageError> {
        let directory = Dir::read_from(&self.tmp).map_err(io_error)?;
        let cutoff = SystemTime::now()
            .checked_sub(older_than)
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |elapsed| elapsed.as_secs());
        let mut removed = 0;
        for entry in directory {
            let entry = entry.map_err(io_error)?;
            let name = entry.file_name();
            if name == c"." || name == c".." {
                continue;
            }
            let Ok(stat) = statat(&self.tmp, name, AtFlags::SYMLINK_NOFOLLOW) else {
                continue;
            };
            let regular = rustix::fs::FileType::from_raw_mode(stat.st_mode)
                == rustix::fs::FileType::RegularFile;
            let modified = u64::try_from(stat.st_mtime).unwrap_or(u64::MAX);
            if regular && modified <= cutoff && unlinkat(&self.tmp, name, AtFlags::empty()).is_ok()
            {
                removed += 1;
            }
        }
        Ok(removed)
    }
}

/// A blob written to `tmp/` and not yet published. Dropping it removes whatever
/// is left of its temporary files (after a publish only the backup, if any).
struct Staged<'a> {
    dirs: &'a Dirs,
    key: StorageKey,
    temp: String,
    temp_meta: String,
    backup: String,
    info: Info,
}

impl Drop for Staged<'_> {
    fn drop(&mut self) {
        for name in [&self.temp, &self.temp_meta, &self.backup] {
            let _ = unlinkat(&self.dirs.tmp, name.as_str(), AtFlags::empty());
        }
    }
}

/// Raises a flag when dropped while armed: the awaiting future was cancelled.
struct CancelOnDrop {
    flag: Arc<AtomicBool>,
    armed: bool,
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if self.armed {
            self.flag.store(true, Ordering::SeqCst);
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    bytes
        .iter()
        .flat_map(|byte| {
            [
                char::from(DIGITS[usize::from(byte >> 4)]),
                char::from(DIGITS[usize::from(byte & 15)]),
            ]
        })
        .collect()
}

#[async_trait]
impl Storage for DiskStorage {
    async fn put(
        &self,
        key: &StorageKey,
        body: Bytes,
        options: PutOptions,
    ) -> Result<Info, StorageError> {
        // Nothing is written for a body already known to be too large.
        if body.len() as u64 > options.max_bytes() {
            return Err(StorageError::TooLarge);
        }
        let (dirs, key) = (Arc::clone(&self.inner), key.clone());
        run(move || dirs.put(&key, &body, &options)).await
    }

    async fn put_all(
        &self,
        items: Vec<(StorageKey, Bytes, PutOptions)>,
    ) -> Result<Vec<Info>, StorageError> {
        if items
            .iter()
            .any(|(_, body, options)| body.len() as u64 > options.max_bytes())
        {
            return Err(StorageError::TooLarge);
        }
        // The blocking job outlives a dropped future, so it is told: until the
        // publish phase starts, a dropped future discards its staged files.
        let cancelled = Arc::new(AtomicBool::new(false));
        let mut guard = CancelOnDrop {
            flag: Arc::clone(&cancelled),
            armed: true,
        };
        let dirs = Arc::clone(&self.inner);
        let result = run(move || dirs.put_all(&items, &cancelled, &|| {})).await;
        guard.armed = false;
        result
    }

    async fn open(&self, key: &StorageKey, max_bytes: u64) -> Result<Blob, StorageError> {
        let (dirs, key) = (Arc::clone(&self.inner), key.clone());
        run(move || dirs.open(&key, max_bytes)).await
    }

    async fn stat(&self, key: &StorageKey) -> Result<Info, StorageError> {
        let (dirs, key) = (Arc::clone(&self.inner), key.clone());
        run(move || dirs.stat(&key)).await
    }

    async fn delete(&self, key: &StorageKey) -> Result<(), StorageError> {
        let (dirs, key) = (Arc::clone(&self.inner), key.clone());
        run(move || dirs.delete(&key)).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;

    use super::*;

    fn key(text: &str) -> StorageKey {
        StorageKey::parse(text).expect("key")
    }

    fn items(keys: &[&str]) -> Vec<(StorageKey, Bytes, PutOptions)> {
        keys.iter()
            .map(|k| {
                (
                    key(k),
                    Bytes::from_static(b"content"),
                    PutOptions::new(100).expect("options"),
                )
            })
            .collect()
    }

    fn file_count(root: &Path, tree: &str) -> usize {
        fn count(path: &Path) -> usize {
            std::fs::read_dir(path).map_or(0, |entries| {
                entries
                    .map(|entry| entry.expect("entry").path())
                    .map(|path| if path.is_dir() { count(&path) } else { 1 })
                    .sum()
            })
        }
        count(&root.join(tree))
    }

    #[test]
    fn a_cancellation_before_the_publish_phase_discards_every_staged_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let storage = DiskStorage::open(dir.path().join("root")).expect("open");
        let cancelled = AtomicBool::new(false);
        // The future is dropped after the files were staged, before publishing.
        let result = storage
            .inner
            .put_all(&items(&["a", "b", "c"]), &cancelled, &|| {
                cancelled.store(true, Ordering::SeqCst);
            });
        assert!(result.is_err());
        let root = dir.path().join("root");
        assert_eq!(file_count(&root, "blobs"), 0, "a blob was published");
        assert_eq!(file_count(&root, "meta"), 0, "metadata was published");
        assert_eq!(file_count(&root, "tmp"), 0, "staged files were left behind");
    }

    #[test]
    fn without_cancellation_everything_is_published_and_staging_is_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let storage = DiskStorage::open(dir.path().join("root")).expect("open");
        let infos = storage
            .inner
            .put_all(&items(&["a", "b/c"]), &AtomicBool::new(false), &|| {})
            .expect("put_all");
        assert_eq!(infos.len(), 2);
        let root = dir.path().join("root");
        assert_eq!(file_count(&root, "blobs"), 2);
        assert_eq!(file_count(&root, "meta"), 2);
        assert_eq!(file_count(&root, "tmp"), 0);
    }

    #[test]
    fn a_failure_while_publishing_removes_what_was_already_published() {
        let dir = tempfile::tempdir().expect("tempdir");
        let storage = DiskStorage::open(dir.path().join("root")).expect("open");
        // "d" is a directory (it holds "d/x"), so a blob cannot be renamed onto it.
        storage
            .inner
            .put_all(&items(&["d/x"]), &AtomicBool::new(false), &|| {})
            .expect("setup");
        let result = storage
            .inner
            .put_all(&items(&["ok", "d"]), &AtomicBool::new(false), &|| {});
        assert!(result.is_err());
        let root = dir.path().join("root");
        assert!(
            !root.join("blobs/ok").exists(),
            "the first file was left published"
        );
        assert!(!root.join("meta/ok.json").exists());
        assert!(!root.join("meta/d.json").exists());
        assert!(
            root.join("blobs/d/x").exists(),
            "unrelated blobs are untouched"
        );
        assert_eq!(file_count(&root, "tmp"), 0);
    }
}
