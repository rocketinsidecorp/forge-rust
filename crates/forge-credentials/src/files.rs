//! Where credentials live and how they reach the disk: key loading with
//! permission checks, bounded reads and atomic writes.

use std::fmt;
use std::fs::{self, File, OpenOptions};
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
    match fs::metadata(dir) {
        Ok(metadata) => check_private_dir(dir, &metadata)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(CredentialsError::KeyMissing);
        }
        Err(error) => return Err(CredentialsError::io("inspect directory", dir, error)),
    }
    let path = dir.join(key_file_name(environment));
    let mut file = match open_no_follow(&path) {
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

/// Refuses a credentials directory that group or others can write: they
/// could replace the key file (and the encrypted file with one sealed under
/// their key) even though the key file itself is 0600.
#[cfg(unix)]
fn check_private_dir(dir: &Path, metadata: &fs::Metadata) -> Result<(), CredentialsError> {
    use std::os::unix::fs::PermissionsExt as _;

    let mode = metadata.permissions().mode() & 0o777;
    if mode & 0o022 != 0 {
        return Err(CredentialsError::DirectoryPermissions {
            path: dir.to_path_buf(),
            mode,
        });
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_private_dir(_dir: &Path, _metadata: &fs::Metadata) -> Result<(), CredentialsError> {
    Ok(())
}

/// The metadata of an opened file; `None` when it is not a regular file (a
/// FIFO, a device or a directory), which is refused before any read.
fn regular_file(file: &File, path: &Path) -> Result<Option<fs::Metadata>, CredentialsError> {
    let metadata = file
        .metadata()
        .map_err(|error| CredentialsError::io("inspect", path, error))?;
    Ok(metadata.is_file().then_some(metadata))
}

/// Opens `path` for reading without following a final symbolic link, so a
/// link planted in the credentials directory cannot point elsewhere, and
/// without blocking: opening a FIFO for reading waits for a writer unless
/// `O_NONBLOCK` is set, which would hang the command before the
/// regular-file check could refuse it. Reads of a regular file ignore the
/// flag.
fn open_no_follow(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;

        let flags = rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK;
        let flags = i32::try_from(flags.bits())
            .map_err(|_| io::Error::other("O_NOFOLLOW | O_NONBLOCK does not fit an open flag"))?;
        options.custom_flags(flags);
    }
    let opened = options.open(path);
    #[cfg(unix)]
    let opened = opened.map_err(|error| {
        // ELOOP: the final component is a symbolic link.
        if error.raw_os_error() == Some(rustix::io::Errno::LOOP.raw_os_error()) {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "symbolic links are not accepted here",
            )
        } else {
            error
        }
    });
    opened
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
    let mut file = match open_no_follow(&path) {
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
    write_atomic(dir, &file_name(environment), data, replace)
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
    create_private_dir(dir)?;
    let metadata =
        fs::metadata(dir).map_err(|error| CredentialsError::io("inspect directory", dir, error))?;
    check_private_dir(dir, &metadata)?;
    write_atomic(dir, &key_file_name(environment), text.as_bytes(), replace)
}

fn write_atomic(
    dir: &Path,
    name: &str,
    data: &[u8],
    replace: bool,
) -> Result<(), CredentialsError> {
    create_private_dir(dir)?;
    let target = dir.join(name);
    if !replace && fs::symlink_metadata(&target).is_ok() {
        return Err(CredentialsError::Exists(target));
    }
    let mut suffix = [0_u8; 6];
    getrandom::fill(&mut suffix).map_err(|_| forge_encryption::EncryptionError::Randomness)?;
    let suffix: String = suffix.iter().map(|byte| format!("{byte:02x}")).collect();
    let temp = dir.join(format!(".{name}.{suffix}.tmp"));
    let written = write_new_private(&temp, data);
    let result = written.and_then(|()| {
        if replace {
            fs::rename(&temp, &target)
                .map_err(|error| CredentialsError::io("replace", &target, error))
        } else {
            // A hard link refuses to replace an existing file, which closes
            // the window between the check above and the final name appearing.
            fs::hard_link(&temp, &target).map_err(|error| {
                if error.kind() == io::ErrorKind::AlreadyExists {
                    CredentialsError::Exists(target.clone())
                } else {
                    CredentialsError::io("create", &target, error)
                }
            })
        }
    });
    // After a rename the temporary name is gone; after a link or a failure it
    // must not stay behind.
    let _ = fs::remove_file(&temp);
    result?;
    sync_dir(dir);
    Ok(())
}

fn write_new_private(path: &Path, data: &[u8]) -> Result<(), CredentialsError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;

        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|error| CredentialsError::io("create", path, error))?;
    file.write_all(data)
        .and_then(|()| file.sync_all())
        .map_err(|error| CredentialsError::io("write", path, error))
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

/// Makes a rename durable. Best effort: some platforms cannot fsync a
/// directory.
fn sync_dir(dir: &Path) {
    if let Ok(handle) = File::open(dir) {
        let _ = handle.sync_all();
    }
}
