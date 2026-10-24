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
//! The metadata records the inode of the blob file it was written for (the
//! temporary file keeps its inode through the rename). A record that names
//! another inode (the instant between the two renames of an overwrite, a
//! restored backup) or none (written before inodes were recorded) is trusted
//! only when the content matches its SHA-256, so the new metadata is never
//! served with the old blob or the reverse. A blob whose metadata is missing,
//! does not match its size or fails that check is served as
//! `application/octet-stream`, which the handler downloads. Keys of user
//! content are unique (UUIDv7), so overwrites are the exception.
//!
//! An overwrite keeps the previous blob and metadata as second hard links in
//! `tmp/` until the write is complete: a failed publish, or a failed
//! [`Storage::put_all`] batch, puts the previous version back instead of
//! deleting the key.
//!
//! Disk storage suits one application host (or hosts sharing a volume with
//! working `rename` and `fsync` semantics, not NFS). Blocking file-system work
//! runs on Tokio's blocking pool and is awaited by the caller (never
//! detached); dropping the future abandons the result, not the syscall in
//! flight, and `tmp/` cleanup covers what it leaves.

use std::{
    fs::File,
    io::{self, Read, Write},
    os::{fd::OwnedFd, unix::fs::MetadataExt},
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
    fs::{AtFlags, Dir, Mode, OFlags, linkat, mkdirat, openat, renameat, statat, unlinkat},
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
    /// The inode of the blob file this record describes; absent in records
    /// written before it existed, which are trusted on their size alone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    inode: Option<u64>,
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

        let temp = uuid::Uuid::now_v7().simple().to_string();
        // Whatever a failure leaves behind goes (the guard unlinks on drop).
        let staged = Staged {
            dirs: self,
            key: key.clone(),
            temp_meta: format!("{temp}.meta"),
            backup: format!("{temp}.old"),
            backup_blob: format!("{temp}.oldblob"),
            temp,
            info: Info {
                key: key.clone(),
                size: body.len() as u64,
                content_type,
                filename,
                sha256: Some(digest.clone()),
                modified: SystemTime::now(),
            },
        };
        let inode = self.write_temp(&staged.temp, body)?;
        let metadata = Metadata {
            content_type: staged.info.content_type.clone(),
            filename: staged.info.filename.clone(),
            sha256: digest,
            size: body.len() as u64,
            inode: Some(inode),
        };
        let encoded = serde_json::to_vec(&metadata)
            .map_err(|error| StorageError::Io(io::Error::other(error)))?;
        self.write_temp(&staged.temp_meta, &encoded)?;
        Ok(staged)
    }

    /// Publishes a staged blob: the metadata, then the blob. The previous
    /// blob and metadata, if any, stay reachable as second hard links in
    /// `tmp/` (no instant without a file under the key), so a failure here,
    /// or the rollback of a batch, puts them back with [`Dirs::revert`].
    fn promote(&self, staged: &Staged<'_>) -> Result<Promoted, StorageError> {
        let key = &staged.key;
        let blob_dir = self.parent(&self.blobs, key, true).map_err(io_error)?;
        let meta_dir = self.parent(&self.meta, key, true).map_err(io_error)?;
        let meta_leaf = Self::meta_leaf(key);
        let old_blob = self.keep(&blob_dir, Self::leaf(key), &staged.backup_blob)?;
        let old_meta = self.keep(&meta_dir, &meta_leaf, &staged.backup)?;
        let promoted = Promoted {
            blob_dir,
            meta_dir,
            old_blob,
            old_meta,
        };
        let published = renameat(
            &self.tmp,
            staged.temp_meta.as_str(),
            &promoted.meta_dir,
            meta_leaf.as_str(),
        )
        .and_then(|()| {
            renameat(
                &self.tmp,
                staged.temp.as_str(),
                &promoted.blob_dir,
                Self::leaf(key),
            )
        });
        if let Err(error) = published {
            self.revert(staged, &promoted);
            return Err(io_error(error));
        }
        for directory in [&promoted.blob_dir, &promoted.meta_dir] {
            // Best effort: the rename is already atomic; this makes it durable.
            let _ = rustix::fs::fsync(directory);
        }
        Ok(promoted)
    }

    /// Links the current `leaf` of `directory` as `backup` in `tmp/`; false
    /// when there is none.
    fn keep(&self, directory: &OwnedFd, leaf: &str, backup: &str) -> Result<bool, StorageError> {
        match linkat(directory, leaf, &self.tmp, backup, AtFlags::empty()) {
            Ok(()) => Ok(true),
            Err(Errno::NOENT) => Ok(false),
            Err(error) => Err(io_error(error)),
        }
    }

    /// Undoes a (partial) publish: the previous blob and metadata go back
    /// under the key, or the key is removed when it had none.
    fn revert(&self, staged: &Staged<'_>, promoted: &Promoted) {
        let meta_leaf = Self::meta_leaf(&staged.key);
        for (directory, leaf, backup, existed) in [
            (
                &promoted.blob_dir,
                Self::leaf(&staged.key),
                staged.backup_blob.as_str(),
                promoted.old_blob,
            ),
            (
                &promoted.meta_dir,
                meta_leaf.as_str(),
                staged.backup.as_str(),
                promoted.old_meta,
            ),
        ] {
            // Best effort: there is nothing better to do with a failure here.
            if existed {
                let _ = renameat(&self.tmp, backup, directory, leaf);
            } else {
                let _ = unlinkat(directory, leaf, AtFlags::empty());
            }
        }
    }

    fn put(
        &self,
        key: &StorageKey,
        body: &[u8],
        options: &PutOptions,
    ) -> Result<Info, StorageError> {
        let staged = self.stage(key, body, options)?;
        drop(self.promote(&staged)?);
        Ok(staged.info.clone())
    }

    /// Stages every file, then publishes them all, or none: a failure puts
    /// back the previous version of every key already published (or removes
    /// the key when it was new), and a cancelled request (`cancelled`, set
    /// when the awaiting future is dropped) discards the staged files before
    /// anything is published. `before_promote` runs between the two phases.
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
        let mut promoted = Vec::with_capacity(staged.len());
        for file in &staged {
            match self.promote(file) {
                Ok(done) => promoted.push(done),
                Err(error) => {
                    // Newest first, so a key listed twice ends at its oldest.
                    for (file, done) in staged.iter().zip(&promoted).rev() {
                        self.revert(file, done);
                    }
                    return Err(error);
                }
            }
        }
        Ok(staged.iter().map(|file| file.info.clone()).collect())
    }

    /// Writes and flushes `tmp/<name>`; returns its inode.
    fn write_temp(&self, name: &str, content: &[u8]) -> Result<u64, StorageError> {
        let descriptor = openat(
            &self.tmp,
            name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            mode(FILE_MODE),
        )
        .map_err(io_error)?;
        let mut file = File::from(descriptor);
        file.write_all(content).map_err(io_error)?;
        file.sync_all().map_err(io_error)?;
        Ok(file.metadata().map_err(io_error)?.ino())
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

    /// The blob's info, with its metadata when the record belongs to this
    /// file. The second value is the SHA-256 the content must have before the
    /// metadata may be trusted: set when the record names another inode (the
    /// instant between the two renames of an overwrite, or a restored backup)
    /// or no inode (a record from before inodes were recorded).
    fn describe(
        &self,
        key: &StorageKey,
        file: &File,
    ) -> Result<(Info, Option<String>), StorageError> {
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
            return Ok((info, None));
        };
        let mut raw = Vec::new();
        if meta_file
            .take(MAX_META_BYTES)
            .read_to_end(&mut raw)
            .is_err()
        {
            return Ok((info, None));
        }
        let Ok(meta) = serde_json::from_slice::<Metadata>(&raw) else {
            return Ok((info, None));
        };
        if meta.size != stat.len() {
            return Ok((info, None));
        }
        let pending = (meta.inode != Some(stat.ino())).then(|| meta.sha256.clone());
        if let Ok(content_type) = normalize_content_type(&meta.content_type) {
            info.content_type = content_type;
        }
        info.filename = meta
            .filename
            .map(|name| sanitize_filename(&name))
            .filter(|name| !name.is_empty());
        info.sha256 = Some(meta.sha256);
        Ok((info, pending))
    }

    fn open(&self, key: &StorageKey, max_bytes: u64) -> Result<Blob, StorageError> {
        let file = self.open_leaf(&self.blobs, key, Self::leaf(key))?;
        let (mut info, pending) = self.describe(key, &file)?;
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
        if pending.is_some_and(|digest| hex(&Sha256::digest(&body)) != digest) {
            info = untrusted(info);
        }
        Ok(Blob {
            info,
            body: Bytes::from(body),
        })
    }

    fn stat(&self, key: &StorageKey) -> Result<Info, StorageError> {
        let file = self.open_leaf(&self.blobs, key, Self::leaf(key))?;
        let (info, pending) = self.describe(key, &file)?;
        let Some(digest) = pending else {
            return Ok(info);
        };
        let mut hasher = Sha256::new();
        let mut buffer = vec![0_u8; 64 << 10];
        let mut content = (&file).take(info.size);
        loop {
            let read = content.read(&mut buffer).map_err(io_error)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        Ok(if hex(&hasher.finalize()) == digest {
            info
        } else {
            untrusted(info)
        })
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
    /// The previous metadata, while the publish is not final.
    backup: String,
    /// The previous blob, while the publish is not final.
    backup_blob: String,
    info: Info,
}

/// A published blob's directories and whether it replaced a previous blob
/// and metadata (kept in `tmp/` as the [`Staged`] backups until it drops).
struct Promoted {
    blob_dir: OwnedFd,
    meta_dir: OwnedFd,
    old_blob: bool,
    old_meta: bool,
}

impl Drop for Staged<'_> {
    fn drop(&mut self) {
        for name in [&self.temp, &self.temp_meta, &self.backup, &self.backup_blob] {
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

/// `info` without what only its metadata could tell.
fn untrusted(mut info: Info) -> Info {
    info.content_type = OCTET_STREAM.to_owned();
    info.filename = None;
    info.sha256 = None;
    info
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

    fn typed(
        text: &str,
        body: &'static [u8],
        content_type: &str,
    ) -> (StorageKey, Bytes, PutOptions) {
        (
            key(text),
            Bytes::from_static(body),
            PutOptions::new(100)
                .expect("options")
                .content_type(content_type),
        )
    }

    #[test]
    fn a_failed_batch_puts_back_the_blobs_it_overwrote() {
        let dir = tempfile::tempdir().expect("tempdir");
        let storage = DiskStorage::open(dir.path().join("root")).expect("open");
        let (old, body, options) = typed("kept", b"old content", "text/plain");
        storage.inner.put(&old, &body, &options).expect("put");
        storage
            .inner
            .put_all(&items(&["d/x"]), &AtomicBool::new(false), &|| {})
            .expect("setup");
        // "kept" is overwritten first, then "d" (a directory) fails the batch.
        let result = storage.inner.put_all(
            &[
                typed("kept", b"replacement", "image/png"),
                typed("fresh", b"new", "text/plain"),
                typed("d", b"x", "text/plain"),
            ],
            &AtomicBool::new(false),
            &|| {},
        );
        assert!(result.is_err());
        let blob = storage.inner.open(&old, 100).expect("the old blob is back");
        assert_eq!(&blob.body[..], b"old content");
        assert_eq!(blob.info.content_type, "text/plain");
        assert!(blob.info.sha256.is_some(), "its metadata is back too");
        assert!(matches!(
            storage.inner.stat(&key("fresh")),
            Err(StorageError::NotFound)
        ));
        let root = dir.path().join("root");
        assert!(!root.join("meta/fresh.json").exists());
        assert_eq!(file_count(&root, "tmp"), 0, "backups were left behind");
    }

    #[test]
    fn a_successful_overwrite_leaves_no_backup() {
        let dir = tempfile::tempdir().expect("tempdir");
        let storage = DiskStorage::open(dir.path().join("root")).expect("open");
        let (target, body, options) = typed("k", b"one", "text/plain");
        storage.inner.put(&target, &body, &options).expect("put");
        let (_, body, options) = typed("k", b"two!", "image/png");
        storage
            .inner
            .put(&target, &body, &options)
            .expect("overwrite");
        let blob = storage.inner.open(&target, 100).expect("open");
        assert_eq!(&blob.body[..], b"two!");
        assert_eq!(blob.info.content_type, "image/png");
        assert_eq!(file_count(&dir.path().join("root"), "tmp"), 0);
    }

    #[test]
    fn metadata_is_trusted_only_for_the_blob_it_was_written_for() {
        let dir = tempfile::tempdir().expect("tempdir");
        let storage = DiskStorage::open(dir.path().join("root")).expect("open");
        for (text, body, content_type) in [
            ("x", b"same size", "text/plain"),
            ("y", b"SAME SIZE", "image/png"),
        ] {
            let (target, body, options) = typed(text, body, content_type);
            storage.inner.put(&target, &body, &options).expect("put");
        }
        let meta = dir.path().join("root/meta");
        // y's record next to x's blob (same size): what a reader sees between
        // the two renames of an overwrite. It is ignored.
        std::fs::copy(meta.join("y.json"), meta.join("x.json")).expect("copy");
        let info = storage.inner.stat(&key("x")).expect("stat");
        assert_eq!(info.content_type, OCTET_STREAM);
        assert_eq!(info.sha256, None);
        let blob = storage.inner.open(&key("x"), 100).expect("open");
        assert_eq!(blob.info.content_type, OCTET_STREAM);
        // A restored backup (new inodes) or a record without an inode is
        // trusted once the content matches its digest.
        let blobs = dir.path().join("root/blobs");
        std::fs::copy(blobs.join("y"), blobs.join("y.copy")).expect("copy");
        std::fs::rename(blobs.join("y.copy"), blobs.join("y")).expect("rename");
        assert_eq!(
            storage.inner.stat(&key("y")).expect("stat").content_type,
            "image/png"
        );
        assert_eq!(
            storage
                .inner
                .open(&key("y"), 100)
                .expect("open")
                .info
                .content_type,
            "image/png"
        );
        let raw = std::fs::read_to_string(meta.join("y.json")).expect("read");
        let mut legacy: serde_json::Value = serde_json::from_str(&raw).expect("json");
        legacy.as_object_mut().expect("object").remove("inode");
        std::fs::write(meta.join("y.json"), legacy.to_string()).expect("write");
        assert_eq!(
            storage.inner.stat(&key("y")).expect("stat").content_type,
            "image/png"
        );
        // ...and not when it does not.
        std::fs::write(meta.join("x.json"), legacy.to_string()).expect("write");
        assert_eq!(
            storage.inner.stat(&key("x")).expect("stat").content_type,
            OCTET_STREAM
        );
    }
}
