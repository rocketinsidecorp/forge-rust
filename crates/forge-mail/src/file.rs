//! The development sender: `.eml` files instead of delivery.

use std::{
    io::Write as _,
    path::{Path, PathBuf},
    time::SystemTime,
};

use async_trait::async_trait;
use uuid::Uuid;

use crate::{MailError, Message, Sender, encode, encoding};

/// Writes each message as an `.eml` file (RFC 5322, openable in any mail
/// client) and logs only the file path, never headers, recipients or
/// bodies. Files are created `0600` with `O_CREAT | O_EXCL` under a
/// generated name in a `0700` directory, so an existing file or symlink is
/// never followed or overwritten. A directory that already exists with group
/// or other permissions is tightened to `0700` (the owner bits are kept) so
/// file names and timing stay private too; one that cannot be tightened is
/// an error. A sticky directory such as `/tmp` is a shared system location
/// and a symbolic link may point anywhere: both are left alone with a warning
/// (the files in them are `0600` anyway).
///
/// It exists to read mail without a mail server and is never used in
/// production: configuration validation and [`from_config`] refuse it
/// there. Messages written here are **not delivered**; the constructor says
/// so once in the log.
///
/// [`from_config`]: crate::from_config
#[derive(Clone, Debug)]
pub struct FileSender {
    dir: PathBuf,
}

impl FileSender {
    /// Creates `dir` (`0700`) if needed, tightens an existing one and returns
    /// a sender writing into it.
    pub fn new(dir: impl Into<PathBuf>) -> Result<Self, MailError> {
        let dir = dir.into();
        if dir.as_os_str().is_empty() {
            return Err(MailError::Configuration(
                "file sender directory is required".to_owned(),
            ));
        }
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder
            .create(&dir)
            .map_err(|error| MailError::File { kind: error.kind() })?;
        tighten_directory(&dir)?;
        // Mail written to files is never delivered: say so loudly, once, so
        // a development URL that survived into a deployment shows up in the
        // first lines of the log rather than as missing email.
        tracing::warn!(
            dir = %dir.display(),
            "development mail sender active: messages are written to files and NOT delivered"
        );
        Ok(Self { dir })
    }

    /// The directory messages are written to.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

#[async_trait]
impl Sender for FileSender {
    /// Validates and encodes `message` and stores it as
    /// `<time>-<uuid>.eml`. A leading `X-Forge-Envelope-To` header lists
    /// every recipient, including `Bcc` ones, which no real header carries.
    async fn send(&self, message: &Message) -> Result<(), MailError> {
        let encoded = encode(message)?;
        let name = format!(
            "{}-{}.eml",
            encoding::compact_utc(SystemTime::now()),
            Uuid::now_v7()
        );
        let path = self.dir.join(name);
        let mut data = encoding::fold_header("X-Forge-Envelope-To", &encoded.recipients.join(", "))
            .into_bytes();
        data.extend_from_slice(b"\r\n");
        data.extend_from_slice(&encoded.data);
        let target = path.clone();
        tokio::task::spawn_blocking(move || write_new(&target, &data))
            .await
            .map_err(|_| MailError::File {
                kind: std::io::ErrorKind::Other,
            })??;
        tracing::info!(path = %path.display(), "mail written to file");
        Ok(())
    }
}

/// Removes group and other access from an existing directory (Unix).
///
/// The directory is inspected and changed through one open handle (`fstat`,
/// `fchmod`), so the path cannot be swapped for another between the check and
/// the change. std cannot open a directory with `O_NOFOLLOW` portably without
/// a new dependency, so a symbolic link is detected beforehand with
/// `symlink_metadata`, and the handle must be the very directory that call
/// saw (same device and inode); a link, or a path swapped in between, is not
/// tightened.
#[cfg(unix)]
fn tighten_directory(dir: &Path) -> Result<(), MailError> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    let file_error = |error: std::io::Error| MailError::File { kind: error.kind() };
    let before = std::fs::symlink_metadata(dir).map_err(file_error)?;
    if before.file_type().is_symlink() {
        tracing::warn!(
            dir = %dir.display(),
            "development mail directory is a symbolic link and was not tightened"
        );
        return Ok(());
    }
    let handle = std::fs::File::open(dir).map_err(file_error)?;
    let metadata = handle.metadata().map_err(file_error)?;
    if !metadata.is_dir() || metadata.dev() != before.dev() || metadata.ino() != before.ino() {
        return Err(MailError::File {
            kind: std::io::ErrorKind::InvalidInput,
        });
    }
    let mode = metadata.permissions().mode();
    if mode & 0o077 == 0 {
        return Ok(());
    }
    if mode & 0o1000 != 0 {
        tracing::warn!(
            dir = %dir.display(),
            "development mail directory is shared (sticky) and was not tightened"
        );
        return Ok(());
    }
    handle
        .set_permissions(std::fs::Permissions::from_mode(mode & 0o7700))
        .map_err(file_error)?;
    tracing::warn!(
        dir = %dir.display(),
        "development mail directory was group- or world-accessible; tightened to 0700"
    );
    Ok(())
}

#[cfg(not(unix))]
fn tighten_directory(_dir: &Path) -> Result<(), MailError> {
    Ok(())
}

fn write_new(path: &Path, data: &[u8]) -> Result<(), MailError> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options
        .open(path)
        .map_err(|error| MailError::File { kind: error.kind() })?;
    if let Err(error) = file.write_all(data).and_then(|()| file.sync_all()) {
        drop(file);
        let _ = std::fs::remove_file(path);
        return Err(MailError::File { kind: error.kind() });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Address;

    fn message() -> Message {
        Message::new(Address::new("App", "app@example.com").expect("valid"))
            .to(Address::new("", "ada@example.org").expect("valid"))
            .bcc(Address::new("", "audit@example.org").expect("valid"))
            .subject("Hello")
            .text("Body")
    }

    #[tokio::test]
    async fn messages_become_private_eml_files_with_the_envelope() {
        let root = tempfile::tempdir().expect("temporary directory");
        let dir = root.path().join("tmp/mail");
        let sender = FileSender::new(&dir).expect("created");
        sender.send(&message()).await.expect("written");
        sender.send(&message()).await.expect("written");
        let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
            .expect("listing")
            .map(|entry| entry.expect("entry").path())
            .collect();
        files.sort();
        assert_eq!(files.len(), 2);
        let text = std::fs::read_to_string(&files[0]).expect("readable");
        assert!(
            text.starts_with("X-Forge-Envelope-To: ada@example.org, audit@example.org\r\nDate: "),
            "{text}"
        );
        assert!(
            files[0]
                .extension()
                .is_some_and(|extension| extension == "eml")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let file_mode = std::fs::metadata(&files[0])
                .expect("meta")
                .permissions()
                .mode();
            assert_eq!(file_mode & 0o777, 0o600);
            let dir_mode = std::fs::metadata(&dir).expect("meta").permissions().mode();
            assert_eq!(dir_mode & 0o777, 0o700);
        }
    }

    #[cfg(unix)]
    #[test]
    fn an_existing_open_directory_is_tightened() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().expect("temporary directory");
        let dir = root.path().join("mail");
        std::fs::create_dir(&dir).expect("create");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        FileSender::new(&dir).expect("tightened");
        let mode = std::fs::metadata(&dir).expect("meta").permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
        // Already private: untouched, including owner bits.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).expect("chmod");
        FileSender::new(&dir).expect("kept");
        let mode = std::fs::metadata(&dir).expect("meta").permissions().mode();
        assert_eq!(mode & 0o777, 0o500);
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).expect("chmod");
    }

    #[cfg(unix)]
    #[test]
    fn a_symbolic_link_is_not_followed_to_change_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().expect("temporary directory");
        let target = root.path().join("target");
        std::fs::create_dir(&target).expect("create");
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let link = root.path().join("link");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");
        FileSender::new(&link).expect("accepted");
        let mode = std::fs::metadata(&target)
            .expect("meta")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755, "the target was not changed");
    }

    #[cfg(unix)]
    #[test]
    fn a_sticky_shared_directory_is_left_alone() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().expect("temporary directory");
        let dir = root.path().join("shared");
        std::fs::create_dir(&dir).expect("create");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o1777)).expect("chmod");
        FileSender::new(&dir).expect("accepted");
        let mode = std::fs::metadata(&dir).expect("meta").permissions().mode();
        assert_eq!(mode & 0o7777, 0o1777);
    }

    #[tokio::test]
    async fn invalid_messages_write_nothing() {
        let root = tempfile::tempdir().expect("temporary directory");
        let sender = FileSender::new(root.path()).expect("created");
        let mut bad = message();
        bad.to[0].email = "ada@example.org\r\nBcc: eve@example.com".to_owned();
        assert_eq!(
            sender.send(&bad).await.expect_err("invalid").code(),
            "mail_invalid"
        );
        assert_eq!(std::fs::read_dir(root.path()).expect("listing").count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn existing_paths_are_never_followed() {
        let root = tempfile::tempdir().expect("temporary directory");
        let outside = root.path().join("outside");
        std::fs::write(&outside, b"keep").expect("write");
        let link = root.path().join("link.eml");
        std::os::unix::fs::symlink(&outside, &link).expect("symlink");
        let error = write_new(&link, b"overwrite").expect_err("exists");
        assert!(matches!(error, MailError::File { .. }));
        assert_eq!(std::fs::read(&outside).expect("read"), b"keep");
    }
}
