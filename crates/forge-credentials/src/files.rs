//! Where credentials live and how they reach the disk: key loading with
//! permission checks, bounded reads and atomic writes.

use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};

use forge_encryption::{Key, encode_key};
use zeroize::Zeroizing;

use crate::{CredentialsError, MAX_FILE_BYTES, MAX_KEYS, parse_keys, validate_environment};

/// Where credentials live, relative to the application root.
pub const DEFAULT_DIR: &str = "config/credentials";

/// The environment variable that carries the key list.
pub const ENV_MASTER_KEY: &str = "FORGE_MASTER_KEY";

const FILE_SUFFIX: &str = ".json.enc";
const KEY_SUFFIX: &str = ".key";
const MAX_KEY_FILE: usize = 4096;

/// The credentials file name of `environment`, such as `production.json.enc`.
#[must_use]
pub fn file_name(environment: &str) -> String {
    format!("{environment}{FILE_SUFFIX}")
}

/// The key file name of `environment`, such as `production.key`.
#[must_use]
pub fn key_file_name(environment: &str) -> String {
    format!("{environment}{KEY_SUFFIX}")
}

/// Where [`load_keys`] found the key.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum KeySource {
    /// `FORGE_MASTER_KEY`.
    Environment,
    /// The environment's key file.
    File(PathBuf),
}

impl fmt::Display for KeySource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Environment => formatter.write_str(ENV_MASTER_KEY),
            Self::File(path) => write!(formatter, "{}", path.display()),
        }
    }
}

/// Reads the keys of `environment`: `FORGE_MASTER_KEY` when it is set and
/// not blank (read through `lookup`), otherwise `<dir>/<environment>.key`.
/// The first key is active; the others only decrypt. Key files that group or
/// others can access, key files in a directory that group or others can
/// write, symbolic links and non-regular files (a FIFO, a device) are
/// refused. With neither source the error is [`CredentialsError::KeyMissing`].
pub fn load_keys(
    dir: &Path,
    environment: &str,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> Result<(Vec<Key>, KeySource), CredentialsError> {
    validate_environment(environment)?;
    if let Some(value) = lookup(ENV_MASTER_KEY).map(Zeroizing::new)
        && !value.trim().is_empty()
    {
        let keys = parse_keys(&value).map_err(|error| prefixed(ENV_MASTER_KEY, error))?;
        return Ok((keys, KeySource::Environment));
    }
    let directory = match Dir::open(dir) {
        Ok(directory) => directory,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(CredentialsError::KeyMissing);
        }
        Err(error) => return Err(CredentialsError::io("open directory", dir, error)),
    };
    directory.check_private()?;
    let path = dir.join(key_file_name(environment));
    let mut file = match directory.open_read(&key_file_name(environment)) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(CredentialsError::KeyMissing);
        }
        Err(error) => return Err(CredentialsError::io("open key file", path, error)),
    };
    let Some(metadata) = regular_file(&file, &path)? else {
        return Err(CredentialsError::InvalidKey(format!(
            "credentials: {} is not a regular file",
            path.display()
        )));
    };
    check_private(&path, &metadata)?;
    let data = read_bounded(&mut file, MAX_KEY_FILE)
        .map_err(|error| CredentialsError::io("read key file", &path, error))?;
    let Some(data) = data else {
        return Err(CredentialsError::InvalidKey(format!(
            "credentials: key file {} is too large",
            path.display()
        )));
    };
    let text = Zeroizing::new(String::from_utf8_lossy(&data).into_owned());
    let keys = parse_keys(&text).map_err(|error| prefixed(&path.display().to_string(), error))?;
    Ok((keys, KeySource::File(path)))
}

fn prefixed(source: &str, error: CredentialsError) -> CredentialsError {
    match error {
        CredentialsError::InvalidKey(message) => {
            CredentialsError::InvalidKey(format!("{source}: {message}"))
        }
        other => other,
    }
}

#[cfg(unix)]
fn check_private(path: &Path, metadata: &fs::Metadata) -> Result<(), CredentialsError> {
    use std::os::unix::fs::PermissionsExt as _;

    let mode = metadata.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(CredentialsError::KeyPermissions {
            path: path.to_path_buf(),
            mode,
        });
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_private(_path: &Path, _metadata: &fs::Metadata) -> Result<(), CredentialsError> {
    Ok(())
}

/// The credentials directory, opened once. On Unix its permissions are
/// checked on the open descriptor and every file in it is opened, created,
/// linked, renamed and removed relative to that descriptor, so the directory
/// checked is the directory used even if its path is swapped meanwhile. The
/// directory path itself may be a symbolic link (as with Forge-Go's
/// `os.OpenRoot`); the files in it may not.
struct Dir<'p> {
    path: &'p Path,
    #[cfg(unix)]
    handle: File,
}

impl<'p> Dir<'p> {
    #[cfg(unix)]
    fn open(path: &'p Path) -> io::Result<Self> {
        use rustix::fs::{Mode, OFlags};

        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
        let handle = File::from(rustix::fs::open(path, flags, Mode::empty())?);
        Ok(Self { path, handle })
    }

    #[cfg(not(unix))]
    fn open(path: &'p Path) -> io::Result<Self> {
        if fs::metadata(path)?.is_dir() {
            Ok(Self { path })
        } else {
            Err(io::Error::other("not a directory"))
        }
    }

    /// Refuses a directory that group or others can write: they could
    /// replace the key file (and the encrypted file with one sealed under
    /// their key) even though the key file itself is 0600.
    #[cfg(unix)]
    fn check_private(&self) -> Result<(), CredentialsError> {
        use std::os::unix::fs::PermissionsExt as _;

        let metadata = self
            .handle
            .metadata()
            .map_err(|error| CredentialsError::io("inspect directory", self.path, error))?;
        let mode = metadata.permissions().mode() & 0o777;
        if mode & 0o022 != 0 {
            return Err(CredentialsError::DirectoryPermissions {
                path: self.path.to_path_buf(),
                mode,
            });
        }
        Ok(())
    }

    #[cfg(not(unix))]
    fn check_private(&self) -> Result<(), CredentialsError> {
        Ok(())
    }

    /// Opens `name` for reading without following a symbolic link, so a
    /// link planted in the directory cannot point elsewhere, and without
    /// blocking: opening a FIFO for reading waits for a writer unless
    /// `O_NONBLOCK` is set, which would hang the command before the
    /// regular-file check could refuse it. Reads of a regular file ignore the
    /// flag.
    #[cfg(unix)]
    fn open_read(&self, name: &str) -> io::Result<File> {
        use rustix::fs::{Mode, OFlags};

        let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
        match rustix::fs::openat(&self.handle, name, flags, Mode::empty()) {
            Ok(fd) => Ok(File::from(fd)),
            // ELOOP: the final component is a symbolic link.
            Err(errno) if errno == rustix::io::Errno::LOOP => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "symbolic links are not accepted here",
            )),
            Err(errno) => Err(errno.into()),
        }
    }

    #[cfg(not(unix))]
    fn open_read(&self, name: &str) -> io::Result<File> {
        File::open(self.path.join(name))
    }

    /// Stores `name` atomically: a temporary file in the directory, fsync,
    /// then a rename (`replace`) or a hard link, which refuses to replace an
    /// existing file and so closes the window between the existence check
    /// and the final name appearing.
    #[cfg(unix)]
    fn write_atomic(&self, name: &str, data: &[u8], replace: bool) -> Result<(), CredentialsError> {
        use rustix::fs::{AtFlags, Mode, OFlags};

        let target = self.path.join(name);
        if !replace && rustix::fs::statat(&self.handle, name, AtFlags::SYMLINK_NOFOLLOW).is_ok() {
            return Err(CredentialsError::Exists(target));
        }
        let temp = temporary_name(name)?;
        let temp_path = self.path.join(&temp);
        let flags =
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let written = rustix::fs::openat(&self.handle, &temp, flags, Mode::RUSR | Mode::WUSR)
            .map_err(|errno| CredentialsError::io("create", &temp_path, io::Error::from(errno)))
            .and_then(|fd| {
                let mut file = File::from(fd);
                file.write_all(data)
                    .and_then(|()| file.sync_all())
                    .map_err(|error| CredentialsError::io("write", &temp_path, error))
            });
        let result = written.and_then(|()| {
            if replace {
                rustix::fs::renameat(&self.handle, &temp, &self.handle, name)
                    .map_err(|errno| CredentialsError::io("replace", &target, errno.into()))
            } else {
                rustix::fs::linkat(&self.handle, &temp, &self.handle, name, AtFlags::empty())
                    .map_err(|errno| {
                        if errno == rustix::io::Errno::EXIST {
                            CredentialsError::Exists(target.clone())
                        } else {
                            CredentialsError::io("create", &target, errno.into())
                        }
                    })
            }
        });
        // After a rename the temporary name is gone; after a link or a
        // failure it must not stay behind.
        let _ = rustix::fs::unlinkat(&self.handle, &temp, AtFlags::empty());
        result?;
        // Makes the rename durable. Best effort: some file systems cannot
        // fsync a directory.
        let _ = self.handle.sync_all();
        Ok(())
    }

    #[cfg(not(unix))]
    fn write_atomic(&self, name: &str, data: &[u8], replace: bool) -> Result<(), CredentialsError> {
        let target = self.path.join(name);
        if !replace && fs::symlink_metadata(&target).is_ok() {
            return Err(CredentialsError::Exists(target));
        }
        let temp = self.path.join(temporary_name(name)?);
        let written = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .map_err(|error| CredentialsError::io("create", &temp, error))
            .and_then(|mut file| {
                file.write_all(data)
                    .and_then(|()| file.sync_all())
                    .map_err(|error| CredentialsError::io("write", &temp, error))
            });
        let result = written.and_then(|()| {
            if replace {
                fs::rename(&temp, &target)
                    .map_err(|error| CredentialsError::io("replace", &target, error))
            } else {
                fs::hard_link(&temp, &target).map_err(|error| {
                    if error.kind() == io::ErrorKind::AlreadyExists {
                        CredentialsError::Exists(target.clone())
                    } else {
                        CredentialsError::io("create", &target, error)
                    }
                })
            }
        });
        let _ = fs::remove_file(&temp);
        result
    }
}

/// `.<name>.<12 random hex digits>.tmp`.
fn temporary_name(name: &str) -> Result<String, CredentialsError> {
    let mut suffix = [0_u8; 6];
    getrandom::fill(&mut suffix).map_err(|_| forge_encryption::EncryptionError::Randomness)?;
    let suffix: String = suffix.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!(".{name}.{suffix}.tmp"))
}

/// The metadata of an opened file; `None` when it is not a regular file (a
/// FIFO, a device or a directory), which is refused before any read.
fn regular_file(file: &File, path: &Path) -> Result<Option<fs::Metadata>, CredentialsError> {
    let metadata = file
        .metadata()
        .map_err(|error| CredentialsError::io("inspect", path, error))?;
    Ok(metadata.is_file().then_some(metadata))
}

/// Reads at most `limit` bytes; `None` when the file is larger.
fn read_bounded(file: &mut File, limit: usize) -> io::Result<Option<Zeroizing<Vec<u8>>>> {
    let mut data = Zeroizing::new(Vec::new());
    let cap = u64::try_from(limit).unwrap_or(u64::MAX).saturating_add(1);
    file.take(cap).read_to_end(&mut data)?;
    if data.len() > limit {
        return Ok(None);
    }
    Ok(Some(data))
}

/// Reads the encrypted file of `environment` from `dir`. A missing file is
/// [`CredentialsError::NotFound`].
pub fn read_file(dir: &Path, environment: &str) -> Result<Vec<u8>, CredentialsError> {
    validate_environment(environment)?;
    let path = dir.join(file_name(environment));
    let opened = Dir::open(dir).and_then(|directory| directory.open_read(&file_name(environment)));
    let mut file = match opened {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(CredentialsError::NotFound(path));
        }
        Err(error) => return Err(CredentialsError::io("open", path, error)),
    };
    if regular_file(&file, &path)?.is_none() {
        let error = io::Error::new(io::ErrorKind::InvalidInput, "not a regular file");
        return Err(CredentialsError::io("open", path, error));
    }
    let data = read_bounded(&mut file, MAX_FILE_BYTES)
        .map_err(|error| CredentialsError::io("read", &path, error))?;
    match data {
        Some(data) => Ok(data.to_vec()),
        None => Err(CredentialsError::Format("file is too large".to_owned())),
    }
}

/// Stores the encrypted file of `environment` in `dir`, creating `dir` (mode
/// 0700) when needed. With `replace` it swaps the file atomically (temporary
/// file in the same directory, fsync, rename), so readers see the old or the
/// new file and never a partial one. Without `replace` it fails with
/// [`CredentialsError::Exists`] if the file exists.
pub fn write_file(
    dir: &Path,
    environment: &str,
    data: &[u8],
    replace: bool,
) -> Result<(), CredentialsError> {
    validate_environment(environment)?;
    create_private_dir(dir)?;
    Dir::open(dir)
        .map_err(|error| CredentialsError::io("open directory", dir, error))?
        .write_atomic(&file_name(environment), data, replace)
}

/// Makes `dir` ready to hold key files: creates it with mode 0700 when
/// missing and refuses an existing directory that group or others can write
/// ([`CredentialsError::DirectoryPermissions`]). [`write_key_file`] does the
/// same; call this first to fail before showing a new key to anyone.
pub fn prepare_key_dir(dir: &Path) -> Result<(), CredentialsError> {
    key_dir(dir).map(drop)
}

fn key_dir(dir: &Path) -> Result<Dir<'_>, CredentialsError> {
    create_private_dir(dir)?;
    let directory =
        Dir::open(dir).map_err(|error| CredentialsError::io("open directory", dir, error))?;
    directory.check_private()?;
    Ok(directory)
}

/// Stores `keys` (active first) as `<dir>/<environment>.key` with mode 0600,
/// one per line. `dir` is created with mode 0700 when missing; an existing
/// directory that group or others can write is refused
/// ([`CredentialsError::DirectoryPermissions`]). Without `replace` it fails
/// if the file exists, so a key is never overwritten by accident.
pub fn write_key_file(
    dir: &Path,
    environment: &str,
    keys: &[Key],
    replace: bool,
) -> Result<(), CredentialsError> {
    validate_environment(environment)?;
    if keys.is_empty() || keys.len() > MAX_KEYS {
        return Err(CredentialsError::InvalidKey(format!(
            "credentials: between 1 and {MAX_KEYS} keys are required"
        )));
    }
    let mut text = Zeroizing::new(String::new());
    for key in keys {
        text.push_str(&encode_key(key.material()));
        text.push('\n');
    }
    key_dir(dir)?.write_atomic(&key_file_name(environment), text.as_bytes(), replace)
}

fn create_private_dir(dir: &Path) -> Result<(), CredentialsError> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;

        builder.mode(0o700);
    }
    builder
        .create(dir)
        .map_err(|error| CredentialsError::io("create directory", dir, error))
}
