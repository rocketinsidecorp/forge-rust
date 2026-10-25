//! The disk adapter: conformance, traversal, symlink escapes, atomic writes,
//! permissions and temporary-file hygiene.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt, symlink},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime},
};

use bytes::Bytes;
use forge_storage::{
    DiskStorage, OCTET_STREAM, PutOptions, Storage, StorageError, StorageKey, testing::run,
};

fn key(text: &str) -> StorageKey {
    StorageKey::parse(text).expect("key")
}

fn options(max: u64) -> PutOptions {
    PutOptions::new(max).expect("options")
}

/// A root directory inside a scratch directory that also holds a secret
/// *outside* the root.
struct Fixture {
    _scratch: tempfile::TempDir,
    outer: PathBuf,
    root: PathBuf,
    store: DiskStorage,
}

fn fixture() -> Fixture {
    let scratch = tempfile::tempdir().expect("tempdir");
    let outer = scratch.path().to_path_buf();
    fs::write(outer.join("secret.txt"), "top secret").expect("secret");
    let root = outer.join("root");
    let store = DiskStorage::open(&root).expect("open");
    Fixture {
        _scratch: scratch,
        outer,
        root,
        store,
    }
}

fn assert_outside_untouched(fixture: &Fixture) {
    assert_eq!(
        fs::read_to_string(fixture.outer.join("secret.txt")).expect("secret"),
        "top secret"
    );
    let mut names: Vec<String> = fs::read_dir(&fixture.outer)
        .expect("read_dir")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    assert_eq!(
        names,
        vec!["root", "secret.txt"],
        "a file appeared outside the root"
    );
}

fn files_below(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in fs::read_dir(dir).expect("read_dir") {
        let path = entry.expect("entry").path();
        if path.is_dir() {
            found.extend(files_below(&path));
        } else {
            found.push(path);
        }
    }
    found
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn disk_storage_conforms_to_the_storage_contract() {
    run(|| async {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = DiskStorage::open(dir.path().join("root")).expect("open");
        // The directory lives as long as the process needs it; leaking one temp
        // dir per case would be wrong, so tie it to the store with a guard task-free trick:
        // keep it alive in a static-lifetime box that is dropped at test end.
        std::mem::forget(dir);
        Arc::new(store) as Arc<dyn Storage>
    })
    .await;
}

#[tokio::test]
async fn traversal_keys_cannot_exist_so_nothing_outside_the_root_is_touched() {
    let fixture = fixture();
    for hostile in [
        "../secret.txt",
        "..",
        "a/../../secret.txt",
        "/etc/passwd",
        "./secret.txt",
        "a/./b",
        "a//b",
        ".hidden",
        "a/.hidden",
        "..\\secret.txt",
        "C:\\secret.txt",
        "%2e%2e/secret.txt",
        "a%2f..%2fsecret.txt",
        "secret.txt\0",
        "a/..%00/b",
    ] {
        assert!(
            matches!(StorageKey::parse(hostile), Err(StorageError::InvalidKey)),
            "{hostile:?} must not be a key"
        );
    }
    // Valid keys that merely look odd stay inside the root.
    for odd in ["a/b..c", "...x", "a...", "x/y.z/.."] {
        let parsed = StorageKey::parse(odd);
        if let Ok(k) = parsed {
            fixture
                .store
                .put(&k, Bytes::from_static(b"x"), options(10))
                .await
                .expect("put");
        }
    }
    assert!(
        files_below(&fixture.root)
            .iter()
            .all(|p| p.starts_with(&fixture.root))
    );
    assert_outside_untouched(&fixture);
}

#[tokio::test]
async fn symlinks_planted_below_the_root_cannot_be_followed_out() {
    let fixture = fixture();
    let secret = fixture.outer.join("secret.txt");
    // Someone with write access to the volume plants links that point outside.
    symlink(&fixture.outer, fixture.root.join("blobs/escape")).expect("dir link");
    symlink(&secret, fixture.root.join("blobs/link")).expect("file link");
    symlink(&fixture.outer, fixture.root.join("meta/escape")).expect("meta dir link");

    let store = &fixture.store;
    assert!(
        matches!(
            store.open(&key("escape/secret.txt"), 100).await,
            Err(StorageError::NotFound)
        ),
        "through a directory link"
    );
    assert!(
        matches!(
            store.open(&key("link"), 100).await,
            Err(StorageError::NotFound)
        ),
        "an escaping file link"
    );
    assert!(matches!(
        store.stat(&key("link")).await,
        Err(StorageError::NotFound)
    ));
    assert!(
        store
            .put(
                &key("escape/new.txt"),
                Bytes::from_static(b"pwned"),
                options(10)
            )
            .await
            .is_err(),
        "write through a directory link"
    );
    assert!(
        store
            .put(&key("link"), Bytes::from_static(b"pwned"), options(10))
            .await
            .is_ok(),
        "a put replaces the link itself, never its target"
    );
    assert_eq!(
        fs::read_to_string(&secret).expect("secret"),
        "top secret",
        "the link target was written through"
    );
    assert!(
        store.delete(&key("escape/secret.txt")).await.is_err(),
        "delete through a directory link"
    );
    assert!(secret.exists(), "delete followed a link out of the root");
    assert!(
        !fixture.outer.join("new.txt").exists(),
        "a file was written outside the root"
    );
    assert!(!fixture.outer.join("new.txt.json").exists());
}

#[tokio::test]
async fn a_symlinked_metadata_file_is_not_read() {
    let fixture = fixture();
    let k = key("doc");
    fixture
        .store
        .put(
            &k,
            Bytes::from_static(b"top secret"),
            options(100).content_type("text/plain"),
        )
        .await
        .expect("put");
    let meta = fixture.root.join("meta/doc.json");
    fs::remove_file(&meta).expect("remove");
    // The link points at a file outside the root that happens to be 10 bytes long.
    symlink(fixture.outer.join("secret.txt"), &meta).expect("link");
    let info = fixture.store.stat(&k).await.expect("stat");
    assert_eq!(
        info.content_type, OCTET_STREAM,
        "metadata through a link is ignored"
    );
    assert_eq!(info.sha256, None);
}

#[tokio::test]
async fn replacing_a_storage_directory_with_a_link_later_changes_nothing() {
    let fixture = fixture();
    fixture
        .store
        .put(&key("first"), Bytes::from_static(b"1"), options(10))
        .await
        .expect("put");
    let moved = fixture.root.join("blobs-moved");
    fs::rename(fixture.root.join("blobs"), &moved).expect("move");
    symlink(&fixture.outer, fixture.root.join("blobs")).expect("link");
    // The adapter holds the original directory: it neither follows the new link
    // nor writes outside.
    fixture
        .store
        .put(&key("second"), Bytes::from_static(b"2"), options(10))
        .await
        .expect("put");
    assert!(moved.join("second").exists());
    assert_outside_untouched(&fixture);
}

#[test]
fn a_storage_directory_that_is_a_link_is_refused_at_startup() {
    let scratch = tempfile::tempdir().expect("tempdir");
    let root = scratch.path().join("root");
    fs::create_dir_all(&root).expect("root");
    let outer = scratch.path().join("elsewhere");
    fs::create_dir_all(&outer).expect("elsewhere");
    symlink(&outer, root.join("blobs")).expect("link");
    assert!(DiskStorage::open(&root).is_err());
    assert_eq!(fs::read_dir(&outer).expect("read_dir").count(), 0);
}

#[tokio::test]
async fn files_and_directories_are_private() {
    let fixture = fixture();
    fixture
        .store
        .put(
            &key("deep/er/file.bin"),
            Bytes::from_static(b"x"),
            options(10),
        )
        .await
        .expect("put");
    for file in files_below(&fixture.root) {
        let mode = fs::metadata(&file).expect("metadata").permissions().mode();
        assert_eq!(
            mode & 0o077,
            0,
            "{} is accessible to others: {mode:o}",
            file.display()
        );
    }
    for dir in [
        &fixture.root,
        &fixture.root.join("blobs"),
        &fixture.root.join("meta"),
        &fixture.root.join("tmp"),
        &fixture.root.join("blobs/deep/er"),
    ] {
        let mode = fs::metadata(dir).expect("metadata").mode() & 0o777;
        assert_eq!(mode & 0o027, 0, "{} is too open: {mode:o}", dir.display());
    }
}

#[tokio::test]
async fn failed_puts_leave_no_temporary_files() {
    let fixture = fixture();
    let _ = fixture
        .store
        .put(&key("ok"), Bytes::from_static(b"x"), options(10))
        .await;
    let _ = fixture
        .store
        .put(&key("toolarge"), Bytes::from(vec![0; 100]), options(10))
        .await;
    let _ = fixture
        .store
        .put(
            &key("badtype"),
            Bytes::from_static(b"x"),
            options(10).content_type("nope"),
        )
        .await;
    // A key whose directory is a file fails after the temp files exist.
    let _ = fixture
        .store
        .put(&key("ok/child"), Bytes::from_static(b"x"), options(10))
        .await;
    let left = fs::read_dir(fixture.root.join("tmp")).expect("tmp").count();
    assert_eq!(left, 0, "temporary files remain");
    assert!(matches!(
        fixture.store.stat(&key("toolarge")).await,
        Err(StorageError::NotFound)
    ));
    assert!(matches!(
        fixture.store.stat(&key("badtype")).await,
        Err(StorageError::NotFound)
    ));
    assert_eq!(
        fixture
            .store
            .open(&key("ok"), 10)
            .await
            .expect("open")
            .body
            .as_ref(),
        b"x"
    );
}

#[tokio::test]
async fn missing_or_stale_metadata_falls_back_to_octet_stream() {
    let fixture = fixture();
    let k = key("blob");
    fixture
        .store
        .put(
            &k,
            Bytes::from_static(b"<html>"),
            options(100).content_type("text/plain").filename("a.txt"),
        )
        .await
        .expect("put");
    let meta = fixture.root.join("meta/blob.json");
    // Metadata whose size disagrees with the file (a stale or tampered record).
    fs::write(
        &meta,
        r#"{"content_type":"text/html","filename":"x.html","sha256":"00","size":999}"#,
    )
    .expect("write");
    let info = fixture.store.stat(&k).await.expect("stat");
    assert_eq!(
        (
            info.content_type.as_str(),
            info.filename.as_deref(),
            info.sha256.as_deref()
        ),
        (OCTET_STREAM, None, None)
    );
    fs::write(&meta, "not json at all").expect("write");
    assert_eq!(
        fixture.store.stat(&k).await.expect("stat").content_type,
        OCTET_STREAM
    );
    fs::remove_file(&meta).expect("remove");
    assert_eq!(
        fixture.store.stat(&k).await.expect("stat").content_type,
        OCTET_STREAM
    );
    // Oversized metadata is not read into memory.
    fs::write(&meta, vec![b' '; 1 << 20]).expect("write");
    assert_eq!(
        fixture.store.stat(&k).await.expect("stat").content_type,
        OCTET_STREAM
    );
}

#[tokio::test]
async fn metadata_cannot_inject_headers_through_a_filename_or_type() {
    let fixture = fixture();
    let k = key("blob");
    fixture
        .store
        .put(&k, Bytes::from_static(b"x"), options(100))
        .await
        .expect("put");
    fs::write(
        fixture.root.join("meta/blob.json"),
        // The digest of "x": the record is trusted (it matches the content),
        // so only sanitizing stands between it and the response headers.
        r#"{"content_type":"text/html\r\nSet-Cookie: a=b","filename":"a\r\nSet-Cookie: x=y.txt","sha256":"2d711642b726b04401627ca9fbac32f5c8530fb1903cc4db02258717921a4881","size":1}"#,
    )
    .expect("write");
    let info = fixture.store.stat(&k).await.expect("stat");
    assert_eq!(info.content_type, OCTET_STREAM);
    let name = info.filename.expect("sanitized name");
    assert!(!name.contains(['\r', '\n']), "{name:?}");
}

#[tokio::test]
async fn prune_temp_removes_only_old_temporary_files() {
    let fixture = fixture();
    let tmp = fixture.root.join("tmp");
    let old = tmp.join("old");
    let young = tmp.join("young");
    fs::write(&old, "x").expect("old");
    fs::write(&young, "x").expect("young");
    fs::File::options()
        .write(true)
        .open(&old)
        .expect("open")
        .set_modified(SystemTime::now() - Duration::from_secs(7200))
        .expect("mtime");
    assert!(
        fixture
            .store
            .prune_temp(Duration::from_secs(10))
            .await
            .is_err(),
        "under a minute could delete uploads in progress"
    );
    assert_eq!(
        fixture
            .store
            .prune_temp(Duration::from_secs(3600))
            .await
            .expect("prune"),
        1
    );
    assert!(!old.exists() && young.exists());
    assert_eq!(
        fixture
            .store
            .prune_temp(Duration::from_secs(3600))
            .await
            .expect("prune"),
        0
    );
}

#[tokio::test]
async fn a_key_below_an_existing_blob_is_not_found_and_does_not_clobber_it() {
    let fixture = fixture();
    fixture
        .store
        .put(&key("file"), Bytes::from_static(b"x"), options(10))
        .await
        .expect("put");
    assert!(matches!(
        fixture.store.open(&key("file/child"), 10).await,
        Err(StorageError::NotFound)
    ));
    assert!(
        fixture
            .store
            .put(&key("file/child"), Bytes::from_static(b"y"), options(10))
            .await
            .is_err()
    );
    assert_eq!(
        fixture
            .store
            .open(&key("file"), 10)
            .await
            .expect("open")
            .body
            .as_ref(),
        b"x"
    );
}

#[tokio::test]
async fn directories_are_not_blobs() {
    let fixture = fixture();
    fixture
        .store
        .put(&key("dir/file"), Bytes::from_static(b"x"), options(10))
        .await
        .expect("put");
    assert!(matches!(
        fixture.store.open(&key("dir"), 10).await,
        Err(StorageError::NotFound)
    ));
    assert!(matches!(
        fixture.store.stat(&key("dir")).await,
        Err(StorageError::NotFound)
    ));
}

#[tokio::test]
async fn a_large_body_round_trips() {
    let fixture = fixture();
    let big: Vec<u8> = (0..8 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
    let k = key("big/blob");
    fixture
        .store
        .put(&k, Bytes::from(big.clone()), options(16 << 20))
        .await
        .expect("put");
    let blob = fixture.store.open(&k, 16 << 20).await.expect("open");
    assert!(blob.body == big, "8 MiB content differs");
    assert_eq!(blob.info.size, big.len() as u64);
}

#[test]
fn opening_validates_the_root() {
    assert!(DiskStorage::open("").is_err());
    let scratch = tempfile::tempdir().expect("tempdir");
    let file = scratch.path().join("file");
    fs::write(&file, "x").expect("file");
    assert!(
        DiskStorage::open(&file).is_err(),
        "a file is not a storage root"
    );
    let mut config = forge_config::StorageConfig::default();
    assert!(
        DiskStorage::from_config(&config).is_err(),
        "no root configured"
    );
    config.root = Some(scratch.path().join("configured"));
    assert!(DiskStorage::from_config(&config).is_ok());
}
