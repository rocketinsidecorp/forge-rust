//! The conformance suite every [`Storage`] must pass (feature `testing`).
//!
//! The factory returns a store on fresh storage; the suite panics with a
//! descriptive message on the first violated guarantee. Unsafe keys need no
//! test here: a [`StorageKey`] cannot be built from one.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{future::Future, sync::Arc};

use bytes::Bytes;

use crate::{OCTET_STREAM, PutOptions, Storage, StorageError, StorageKey};

fn key(text: &str) -> StorageKey {
    StorageKey::parse(text).expect("valid key")
}

fn options(max: u64) -> PutOptions {
    PutOptions::new(max).expect("options")
}

async fn read(store: &dyn Storage, k: &StorageKey) -> (Bytes, crate::Info) {
    let blob = store
        .open(k, 1 << 30)
        .await
        .unwrap_or_else(|e| panic!("open {k}: {e}"));
    (blob.body, blob.info)
}

/// Runs every case, each on a fresh store.
pub async fn run<F, Fut>(new_store: F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = Arc<dyn Storage>>,
{
    round_trip(new_store().await).await;
    stat_describes_without_reading(new_store().await).await;
    nested_keys(new_store().await).await;
    overwrite_replaces_content_and_metadata(new_store().await).await;
    delete_is_idempotent(new_store().await).await;
    missing_key_is_not_found(new_store().await).await;
    size_limit_is_enforced_and_stores_nothing(new_store().await).await;
    size_limit_boundary(new_store().await).await;
    open_refuses_blobs_over_its_limit(new_store().await).await;
    rejects_malformed_content_types(new_store().await).await;
    binary_content(new_store().await).await;
    concurrent_overwrites_never_tear(new_store().await).await;
    concurrent_distinct_keys(new_store().await).await;
}

async fn round_trip(store: Arc<dyn Storage>) {
    let k = key("docs/report.pdf");
    let put = options(100)
        .content_type("Application/PDF; charset=x")
        .filename("../Quarterly \"report\".pdf");
    let info = store
        .put(&k, Bytes::from_static(b"hello"), put)
        .await
        .expect("put");
    assert_eq!(info.size, 5);
    assert_eq!(
        info.content_type, "application/pdf",
        "normalized, parameters dropped"
    );
    assert_eq!(
        info.filename.as_deref(),
        Some("Quarterly report.pdf"),
        "sanitized"
    );
    let (body, opened) = read(store.as_ref(), &k).await;
    assert_eq!(body.as_ref(), b"hello");
    assert_eq!(opened.content_type, "application/pdf");
    assert_eq!(opened.filename.as_deref(), Some("Quarterly report.pdf"));
    assert_eq!(opened.size, 5);
    let digest = opened.sha256.expect("the adapter records the digest");
    assert_eq!(digest.len(), 64);
    assert!(digest.bytes().all(|b| b.is_ascii_hexdigit()));
}

async fn stat_describes_without_reading(store: Arc<dyn Storage>) {
    let k = key("a.txt");
    store
        .put(
            &k,
            Bytes::from_static(b"abc"),
            options(10).content_type("text/plain"),
        )
        .await
        .expect("put");
    let info = store.stat(&k).await.expect("stat");
    assert_eq!((info.size, info.content_type.as_str()), (3, "text/plain"));
}

async fn nested_keys(store: Arc<dyn Storage>) {
    let keys = ["a/b/c/d.txt", "a/b/other", "a/x", "v1.2/file-name_3.tar.gz"];
    for text in keys {
        store
            .put(&key(text), Bytes::from(text.to_owned()), options(100))
            .await
            .unwrap_or_else(|e| panic!("put {text}: {e}"));
    }
    for text in keys {
        let (body, _) = read(store.as_ref(), &key(text)).await;
        assert_eq!(body.as_ref(), text.as_bytes(), "{text}");
    }
}

async fn overwrite_replaces_content_and_metadata(store: Arc<dyn Storage>) {
    let k = key("k");
    store
        .put(
            &k,
            Bytes::from_static(b"one"),
            options(10).content_type("text/plain").filename("one.txt"),
        )
        .await
        .expect("put");
    store
        .put(
            &k,
            Bytes::from_static(b"second"),
            options(10).content_type("text/csv"),
        )
        .await
        .expect("overwrite");
    let (body, info) = read(store.as_ref(), &k).await;
    assert_eq!(body.as_ref(), b"second");
    assert_eq!(info.content_type, "text/csv");
    assert_eq!(
        info.filename, None,
        "the old file name does not survive an overwrite"
    );
}

async fn delete_is_idempotent(store: Arc<dyn Storage>) {
    let k = key("gone/soon.txt");
    store
        .put(&k, Bytes::from_static(b"x"), options(10))
        .await
        .expect("put");
    store.delete(&k).await.expect("delete");
    assert!(matches!(
        store.open(&k, 100).await,
        Err(StorageError::NotFound)
    ));
    store
        .delete(&k)
        .await
        .expect("deleting a missing key is not an error");
    store
        .delete(&key("never/existed"))
        .await
        .expect("nor is a key that never existed");
}

async fn missing_key_is_not_found(store: Arc<dyn Storage>) {
    for text in ["nope", "a/b/c"] {
        assert!(
            matches!(
                store.open(&key(text), 10).await,
                Err(StorageError::NotFound)
            ),
            "{text}"
        );
        assert!(
            matches!(store.stat(&key(text)).await, Err(StorageError::NotFound)),
            "{text}"
        );
    }
}

async fn size_limit_is_enforced_and_stores_nothing(store: Arc<dyn Storage>) {
    let k = key("big");
    let result = store.put(&k, Bytes::from(vec![1; 100]), options(10)).await;
    assert!(matches!(result, Err(StorageError::TooLarge)), "{result:?}");
    assert!(
        matches!(store.stat(&k).await, Err(StorageError::NotFound)),
        "an oversized put stored something"
    );
    // And it does not replace an existing blob.
    store
        .put(&k, Bytes::from_static(b"keep"), options(10))
        .await
        .expect("put");
    assert!(
        store
            .put(&k, Bytes::from(vec![1; 100]), options(10))
            .await
            .is_err()
    );
    assert_eq!(read(store.as_ref(), &k).await.0.as_ref(), b"keep");
}

async fn size_limit_boundary(store: Arc<dyn Storage>) {
    let k = key("edge");
    store
        .put(&k, Bytes::from(vec![7; 10]), options(10))
        .await
        .expect("exactly the limit");
    assert!(matches!(
        store.put(&k, Bytes::from(vec![7; 11]), options(10)).await,
        Err(StorageError::TooLarge)
    ));
}

async fn open_refuses_blobs_over_its_limit(store: Arc<dyn Storage>) {
    let k = key("served");
    store
        .put(&k, Bytes::from(vec![1; 50]), options(100))
        .await
        .expect("put");
    assert!(matches!(
        store.open(&k, 49).await,
        Err(StorageError::TooLarge)
    ));
    assert_eq!(store.open(&k, 50).await.expect("open").body.len(), 50);
}

async fn rejects_malformed_content_types(store: Arc<dyn Storage>) {
    for bad in ["nope", "a/", "text/html\r\nX: y"] {
        let result = store
            .put(
                &key("k"),
                Bytes::from_static(b"x"),
                options(10).content_type(bad),
            )
            .await;
        assert!(
            matches!(result, Err(StorageError::UnsupportedType)),
            "{bad:?}: {result:?}"
        );
    }
    assert!(matches!(
        store.stat(&key("k")).await,
        Err(StorageError::NotFound)
    ));
    let info = store
        .put(&key("k"), Bytes::from_static(b"x"), options(10))
        .await
        .expect("put");
    assert_eq!(
        info.content_type, OCTET_STREAM,
        "no content type means octet-stream"
    );
}

async fn binary_content(store: Arc<dyn Storage>) {
    let k = key("bin");
    store
        .put(&k, Bytes::new(), options(10))
        .await
        .expect("an empty body is a blob");
    assert_eq!(read(store.as_ref(), &k).await.0.len(), 0);
    let binary: Vec<u8> = (0..=255).collect();
    store
        .put(&k, Bytes::from(binary.clone()), options(1000))
        .await
        .expect("put");
    assert_eq!(read(store.as_ref(), &k).await.0.as_ref(), binary.as_slice());
}

async fn concurrent_overwrites_never_tear(store: Arc<dyn Storage>) {
    let k = key("race");
    let versions: Vec<Vec<u8>> = (0..4_u8).map(|v| vec![v; 64 * 1024]).collect();
    store
        .put(&k, Bytes::from(versions[0].clone()), options(1 << 20))
        .await
        .expect("put");
    let mut tasks = tokio::task::JoinSet::new();
    for version in versions.clone() {
        let (store, k) = (Arc::clone(&store), k.clone());
        tasks.spawn(async move {
            for _ in 0..20 {
                store
                    .put(
                        &k,
                        Bytes::from(version.clone()),
                        PutOptions::new(1 << 20).expect("options"),
                    )
                    .await
                    .expect("put");
            }
        });
    }
    for _ in 0..4 {
        let (store, k) = (Arc::clone(&store), k.clone());
        tasks.spawn(async move {
            for _ in 0..40 {
                let blob = store.open(&k, 1 << 20).await.expect("open");
                let first = blob.body[0];
                assert_eq!(blob.body.len(), 64 * 1024, "a torn write: wrong length");
                assert!(
                    blob.body.iter().all(|b| *b == first),
                    "a torn write: mixed content"
                );
            }
        });
    }
    while let Some(result) = tasks.join_next().await {
        result.expect("task");
    }
}

async fn concurrent_distinct_keys(store: Arc<dyn Storage>) {
    let mut tasks = tokio::task::JoinSet::new();
    for i in 0..16 {
        let store = Arc::clone(&store);
        tasks.spawn(async move {
            let k = key(&format!("many/{i}"));
            store
                .put(&k, Bytes::from(format!("value-{i}")), options(100))
                .await
                .expect("put");
            let blob = store.open(&k, 100).await.expect("open");
            assert_eq!(blob.body.as_ref(), format!("value-{i}").as_bytes());
        });
    }
    while let Some(result) = tasks.join_next().await {
        result.expect("task");
    }
}
