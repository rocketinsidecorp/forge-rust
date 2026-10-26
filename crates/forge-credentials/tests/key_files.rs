//! Key files and key sources: sizes, unusable directories and how a key
//! source prints.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::os::unix::fs::PermissionsExt as _;

use forge_credentials::{
    CredentialsError, KeySource, MAX_FILE_BYTES, init, load_keys, read_file, write_file,
    write_key_file,
};

fn no_env(_: &str) -> Option<String> {
    None
}

#[test]
fn a_key_file_over_the_limit_is_refused_without_reading_it_all() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("credentials");
    init(&dir, "production").unwrap();
    let key_path = dir.join("production.key");
    let marker = "DISTINCTIVE-KEY-TEXT";
    fs::write(&key_path, format!("{marker}{}", "A".repeat(8192))).unwrap();
    fs::set_permissions(&key_path, fs::Permissions::from_mode(0o600)).unwrap();
    let error = load_keys(&dir, "production", &no_env).unwrap_err();
    let printed = error.to_string();
    assert!(
        matches!(&error, CredentialsError::InvalidKey(message) if message.contains("is too large")),
        "{printed}"
    );
    assert!(printed.contains("production.key"), "{printed}");
    assert!(!printed.contains(marker), "{printed}");
}

#[test]
fn a_credentials_directory_that_is_a_file_is_an_io_error_not_a_missing_key() {
    let root = tempfile::tempdir().unwrap();
    let not_a_directory = root.path().join("credentials");
    fs::write(&not_a_directory, b"x").unwrap();
    let error = load_keys(&not_a_directory, "production", &no_env).unwrap_err();
    assert!(
        !matches!(error, CredentialsError::KeyMissing),
        "an unusable directory is not an absent key: {error}"
    );
    assert!(error.to_string().contains("open directory"), "{error}");
}

#[test]
fn a_key_source_prints_where_the_key_came_from() {
    assert_eq!(KeySource::Environment.to_string(), "FORGE_MASTER_KEY");
    assert_eq!(
        KeySource::File("config/credentials/production.key".into()).to_string(),
        "config/credentials/production.key"
    );
}

#[test]
fn the_encrypted_file_is_read_only_when_it_is_a_regular_file_of_bounded_size() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("credentials");
    fs::create_dir(&dir).unwrap();
    let path = dir.join("production.json.enc");

    assert!(matches!(
        read_file(&dir, "production"),
        Err(CredentialsError::NotFound(missing)) if missing == path
    ));
    assert!(matches!(
        read_file(&root.path().join("absent"), "production"),
        Err(CredentialsError::NotFound(_))
    ));

    // Over the limit: refused as a format error, whatever is inside.
    fs::write(&path, vec![b'x'; MAX_FILE_BYTES + 1]).unwrap();
    assert!(matches!(
        read_file(&dir, "production"),
        Err(CredentialsError::Format(message)) if message == "file is too large"
    ));
    // At the limit it is read as is.
    fs::write(&path, vec![b'x'; MAX_FILE_BYTES]).unwrap();
    assert_eq!(read_file(&dir, "production").unwrap().len(), MAX_FILE_BYTES);

    // A directory in its place is not a regular file.
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    let error = read_file(&dir, "production").unwrap_err();
    assert!(error.to_string().contains("not a regular file"), "{error}");

    // A credentials directory that is a file is an open error.
    let file_as_dir = root.path().join("plain");
    fs::write(&file_as_dir, b"x").unwrap();
    let error = read_file(&file_as_dir, "production").unwrap_err();
    assert!(
        matches!(&error, CredentialsError::Io { .. }) && error.to_string().contains("open"),
        "{error}"
    );
}

#[test]
fn writes_into_an_unusable_location_fail_without_leaving_files() {
    let root = tempfile::tempdir().unwrap();
    let blocker = root.path().join("blocker");
    fs::write(&blocker, b"x").unwrap();
    let below = blocker.join("credentials");
    for error in [
        write_file(&below, "production", b"{}", false).unwrap_err(),
        write_key_file(&below, "production", &[], false).unwrap_err(),
    ] {
        assert!(!matches!(error, CredentialsError::Exists(_)), "{error}");
    }
    assert_eq!(fs::read(&blocker).unwrap(), b"x");
    assert!(!blocker.join("credentials").exists());

    // No key at all is refused before anything is created.
    let dir = root.path().join("credentials");
    let error = write_key_file(&dir, "production", &[], false).unwrap_err();
    assert!(
        matches!(&error, CredentialsError::InvalidKey(message) if message.contains("between 1 and")),
        "{error}"
    );
    assert!(!dir.exists());
}
