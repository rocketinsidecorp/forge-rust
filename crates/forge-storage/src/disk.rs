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
    sync::Arc,
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

    fn put(
        &self,
        key: &StorageKey,
        body: &[u8],
        options: &PutOptions,
    ) -> Result<Info, StorageError> {
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
        let temp_meta = format!("{temp}.meta");
        // Whatever a failure leaves behind goes.
        let guard = TempFiles {
            dirs: self,
            names: [&temp, &temp_meta],
        };

        self.write_temp(&temp, body)?;
        self.write_temp(&temp_meta, &encoded)?;

        let blob_dir = self.parent(&self.blobs, key, true).map_err(io_error)?;
        let meta_dir = self.parent(&self.meta, key, true).map_err(io_error)?;
        // Metadata first: the blob is the commit point.
        renameat(
            &self.tmp,
            temp_meta.as_str(),
            &meta_dir,
            Self::meta_leaf(key).as_str(),
        )
        .map_err(io_error)?;
        if let Err(error) = renameat(&self.tmp, temp.as_str(), &blob_dir, Self::leaf(key)) {
            let _ = unlinkat(&meta_dir, Self::meta_leaf(key).as_str(), AtFlags::empty());
            return Err(io_error(error));
        }
        drop(guard);
        for directory in [&blob_dir, &meta_dir] {
            // Best effort: the rename is already atomic; this makes it durable.
            let _ = rustix::fs::fsync(directory);
        }
        Ok(Info {
            key: key.clone(),
            size: body.len() as u64,
            content_type,
            filename,
            sha256: Some(digest),
            modified: SystemTime::now(),
        })
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
        let mut directory = Dir::read_from(&self.tmp).map_err(io_error)?;
        let cutoff = SystemTime::now()
            .checked_sub(older_than)
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |elapsed| elapsed.as_secs());
        let mut removed = 0;
        while let Some(entry) = directory.next() {
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

/// Removes the named temporary files when dropped.
struct TempFiles<'a> {
    dirs: &'a Dirs,
    names: [&'a str; 2],
}

impl Drop for TempFiles<'_> {
    fn drop(&mut self) {
        for name in self.names {
            let _ = unlinkat(&self.dirs.tmp, name, AtFlags::empty());
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
