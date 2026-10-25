//! The conformance suite every [`Cache`] must pass (feature `testing`).
//!
//! Run it against a real database for the shared stores; the factory receives
//! the [`Options`] each case needs and returns a [`Subject`] on fresh storage.
//! The suite panics with a descriptive message on the first violated
//! guarantee. Times are short so expiry can be observed with real clocks,
//! including the database clocks of the shared stores.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{
    any::Any,
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use crate::{Cache, CacheError, FetchError, MAX_KEY_LENGTH, Options, Pruner, fetch};

/// A cache on fresh storage plus whatever must stay alive with it (such as a
/// test database).
pub struct Subject {
    /// The cache under test.
    pub cache: Arc<dyn Cache>,
    /// Its pruner, when it keeps persistent entries.
    pub pruner: Option<Arc<dyn Pruner>>,
    /// Kept alive until the case ends.
    pub guard: Box<dyn Any + Send + Sync>,
}

const SHORT_TTL: Duration = Duration::from_millis(400);
const MINUTE: Option<Duration> = Some(Duration::from_secs(60));
const HOUR: Option<Duration> = Some(Duration::from_secs(3600));

/// A computation that cannot fail, for [`fetch`].
type Never = std::convert::Infallible;

async fn set(subject: &Subject, key: &str, value: &[u8], ttl: Option<Duration>) {
    subject
        .cache
        .set(key, value, ttl)
        .await
        .unwrap_or_else(|error| panic!("set: {error}"));
}

async fn get(subject: &Subject, key: &str) -> Option<Vec<u8>> {
    subject
        .cache
        .get(key)
        .await
        .unwrap_or_else(|error| panic!("get: {error}"))
}

/// Runs every case, each on a fresh [`Subject`].
pub async fn run<F, Fut>(new_subject: F)
where
    F: Fn(Options) -> Fut,
    Fut: Future<Output = Subject>,
{
    let defaults = Options::default;
    set_get_delete(new_subject(defaults()).await).await;
    values_are_copied(new_subject(defaults()).await).await;
    binary_and_empty_values(new_subject(defaults()).await).await;
    overwrite_replaces_value_and_ttl(new_subject(defaults()).await).await;
    values_expire(new_subject(defaults()).await).await;
    default_ttl_applies(new_subject(defaults().with_default_ttl(SHORT_TTL)).await).await;
    rejects_invalid_input(new_subject(defaults().with_max_value_bytes(1024)).await).await;
    keys_are_exact(new_subject(defaults()).await).await;
    fetch_computes_once_and_caches(new_subject(defaults()).await).await;
    fetch_collapses_concurrent_misses(new_subject(defaults()).await).await;
    fetch_does_not_cache_errors(new_subject(defaults()).await).await;
    fetch_waiter_survives_a_cancelled_leader(new_subject(defaults()).await).await;
    enforces_the_entry_bound(new_subject(defaults().with_max_entries(5)).await).await;
    prunes_expired_entries(new_subject(defaults()).await).await;
}

async fn set_get_delete(subject: Subject) {
    assert_eq!(get(&subject, "k").await, None, "a miss is not an error");
    set(&subject, "k", b"v1", MINUTE).await;
    assert_eq!(get(&subject, "k").await.as_deref(), Some(&b"v1"[..]));
    set(&subject, "k", b"v2", MINUTE).await;
    assert_eq!(get(&subject, "k").await.as_deref(), Some(&b"v2"[..]));
    subject.cache.delete("k").await.expect("delete");
    assert_eq!(get(&subject, "k").await, None);
    subject
        .cache
        .delete("never-existed")
        .await
        .expect("deleting a missing key is not an error");
}

async fn values_are_copied(subject: Subject) {
    let mut input = b"abc".to_vec();
    set(&subject, "k", &input, MINUTE).await;
    input[0] = b'X';
    let mut out = get(&subject, "k").await.expect("hit");
    assert_eq!(out, b"abc", "mutating the input changed the cache");
    out[0] = b'Y';
    assert_eq!(
        get(&subject, "k").await.expect("hit"),
        b"abc",
        "mutating a returned value changed the cache"
    );
}

async fn binary_and_empty_values(subject: Subject) {
    let binary = [0, 1, 2, 0xff, 0xfe, 0, b'\n', b'\'', b'"', b'\\', 0];
    set(&subject, "bin", &binary, MINUTE).await;
    assert_eq!(get(&subject, "bin").await.expect("hit"), binary);
    set(&subject, "empty", &[], MINUTE).await;
    assert_eq!(
        get(&subject, "empty").await,
        Some(Vec::new()),
        "an empty value is a hit"
    );
    let big = vec![0xab_u8; 512 << 10];
    set(&subject, "big", &big, MINUTE).await;
    assert!(
        get(&subject, "big").await.expect("hit") == big,
        "512 KiB round trip"
    );
    let unusual = "ключ/é/ \t'\";--";
    set(&subject, unusual, b"u", MINUTE).await;
    assert_eq!(get(&subject, unusual).await.as_deref(), Some(&b"u"[..]));
}

async fn overwrite_replaces_value_and_ttl(subject: Subject) {
    set(&subject, "k", b"one", Some(SHORT_TTL)).await;
    tokio::time::sleep(SHORT_TTL / 2).await;
    set(&subject, "k", b"two", MINUTE).await;
    tokio::time::sleep(SHORT_TTL).await;
    assert_eq!(
        get(&subject, "k").await.as_deref(),
        Some(&b"two"[..]),
        "the overwrite carries the new value and the new ttl"
    );
}

/// Asserts that `key`, stored at `stored` with [`SHORT_TTL`], is readable
/// while that ttl runs. A store that took the whole ttl to answer (a loaded
/// CI runner) may rightly have expired the entry already, so the read only
/// counts when it finished inside the ttl.
async fn readable_before_short_ttl(subject: &Subject, key: &str, stored: Instant) {
    let found = get(subject, key).await.is_some();
    if stored.elapsed() < SHORT_TTL {
        assert!(found, "readable before it expires");
    }
}

async fn values_expire(subject: Subject) {
    let stored = Instant::now();
    set(&subject, "k", b"v", Some(SHORT_TTL)).await;
    readable_before_short_ttl(&subject, "k", stored).await;
    tokio::time::sleep(SHORT_TTL + Duration::from_millis(150)).await;
    assert!(
        get(&subject, "k").await.is_none(),
        "an expired entry was returned"
    );
}

async fn default_ttl_applies(subject: Subject) {
    let stored = Instant::now();
    set(&subject, "k", b"v", None).await;
    readable_before_short_ttl(&subject, "k", stored).await;
    tokio::time::sleep(SHORT_TTL + Duration::from_millis(150)).await;
    assert!(
        get(&subject, "k").await.is_none(),
        "no ttl uses the (short) default and expires"
    );
}

async fn rejects_invalid_input(subject: Subject) {
    let cache = &subject.cache;
    let too_long = "k".repeat(MAX_KEY_LENGTH + 1);
    for (name, key) in [
        ("empty key", ""),
        ("NUL in key", "a\0b"),
        ("too long", too_long.as_str()),
    ] {
        assert!(
            matches!(
                cache.set(key, b"v", MINUTE).await,
                Err(CacheError::InvalidKey)
            ),
            "{name}: set"
        );
        assert!(
            matches!(cache.get(key).await, Err(CacheError::InvalidKey)),
            "{name}: get"
        );
        assert!(
            matches!(cache.delete(key).await, Err(CacheError::InvalidKey)),
            "{name}: delete"
        );
    }
    cache
        .set(&"k".repeat(MAX_KEY_LENGTH), b"v", MINUTE)
        .await
        .expect("maximum-length key");
    assert!(matches!(
        cache.set("k", &[0; 1025], MINUTE).await,
        Err(CacheError::ValueTooLarge)
    ));
    cache
        .set("k", &[0; 1024], MINUTE)
        .await
        .expect("value at the limit");
    for ttl in [Duration::ZERO, Duration::from_secs(366 * 24 * 3600)] {
        assert!(
            matches!(
                cache.set("k", b"v", Some(ttl)).await,
                Err(CacheError::InvalidTtl)
            ),
            "ttl {ttl:?}"
        );
    }
}

async fn keys_are_exact(subject: Subject) {
    for (key, value) in [("a", "1"), ("A", "2"), ("a ", "3")] {
        set(&subject, key, value.as_bytes(), MINUTE).await;
    }
    for (key, value) in [("a", "1"), ("A", "2"), ("a ", "3")] {
        assert_eq!(
            get(&subject, key).await.as_deref(),
            Some(value.as_bytes()),
            "keys are case- and space-sensitive: {key:?}"
        );
    }
    assert_eq!(get(&subject, "a  ").await, None);
}

async fn fetch_computes_once_and_caches(subject: Subject) {
    let calls = Arc::new(AtomicUsize::new(0));
    for _ in 0..3 {
        let calls = Arc::clone(&calls);
        let value = fetch(subject.cache.as_ref(), "k", MINUTE, || async move {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok::<_, Never>(b"computed".to_vec())
        })
        .await
        .expect("fetch");
        assert_eq!(value, b"computed");
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1, "compute ran once");
    assert_eq!(get(&subject, "k").await.as_deref(), Some(&b"computed"[..]));
}

async fn fetch_collapses_concurrent_misses(subject: Subject) {
    const CALLERS: usize = 24;
    let cache = Arc::clone(&subject.cache);
    let calls = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(tokio::sync::Notify::new());
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..CALLERS {
        let cache = Arc::clone(&cache);
        let calls = Arc::clone(&calls);
        let release = Arc::clone(&release);
        tasks.spawn(async move {
            let mut value = fetch(cache.as_ref(), "hot", MINUTE, || async move {
                calls.fetch_add(1, Ordering::SeqCst);
                release.notified().await;
                Ok::<_, Never>(b"value".to_vec())
            })
            .await
            .expect("fetch");
            value[0] = b'V'; // every caller owns its copy
            value
        });
    }
    // Let every caller reach the flight, then release the one computation.
    tokio::time::sleep(Duration::from_millis(300)).await;
    release.notify_one();
    while let Some(result) = tasks.join_next().await {
        assert_eq!(result.expect("task"), b"Value");
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "compute ran once for {CALLERS} concurrent misses"
    );
    assert_eq!(
        get(&subject, "hot").await.as_deref(),
        Some(&b"value"[..]),
        "callers mutating their copies did not change the cache"
    );
}

async fn fetch_does_not_cache_errors(subject: Subject) {
    #[derive(Debug)]
    struct Marker;
    impl std::fmt::Display for Marker {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("origin down")
        }
    }
    impl std::error::Error for Marker {}
    let result = fetch(subject.cache.as_ref(), "k", MINUTE, || async {
        Err::<Vec<u8>, _>(Marker)
    })
    .await;
    match result {
        Err(FetchError::Compute(source)) => {
            assert!(
                source.downcast_ref::<Marker>().is_some(),
                "the compute error is returned"
            );
        }
        other => panic!("fetch = {other:?}, want the compute error"),
    }
    assert_eq!(
        get(&subject, "k").await,
        None,
        "a failed computation was cached"
    );
    let value = fetch(subject.cache.as_ref(), "k", MINUTE, || async {
        Ok::<_, Never>(b"ok".to_vec())
    })
    .await
    .expect("fetch after a failure computes afresh");
    assert_eq!(value, b"ok");
    let invalid = fetch(subject.cache.as_ref(), "", MINUTE, || async {
        Ok::<_, Never>(Vec::new())
    })
    .await;
    assert!(matches!(invalid, Err(FetchError::InvalidKey)));
}

async fn fetch_waiter_survives_a_cancelled_leader(subject: Subject) {
    let cache = Arc::clone(&subject.cache);
    let leader_running = Arc::new(tokio::sync::Notify::new());
    let leader = {
        let cache = Arc::clone(&cache);
        let running = Arc::clone(&leader_running);
        tokio::spawn(async move {
            fetch(cache.as_ref(), "k", MINUTE, || async move {
                running.notify_one();
                std::future::pending::<()>().await;
                Ok::<_, Never>(Vec::new())
            })
            .await
        })
    };
    leader_running.notified().await;
    let waiter = {
        let cache = Arc::clone(&cache);
        tokio::spawn(async move {
            fetch(cache.as_ref(), "k", MINUTE, || async {
                Ok::<_, Never>(b"waiter".to_vec())
            })
            .await
        })
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    leader.abort(); // the leader is cancelled: its future is dropped
    assert!(leader.await.expect_err("aborted").is_cancelled());
    let value = tokio::time::timeout(Duration::from_secs(5), waiter)
        .await
        .expect("the waiter must finish")
        .expect("task")
        .expect("the waiter computes for itself when the leader is cancelled");
    assert_eq!(value, b"waiter");
}

async fn enforces_the_entry_bound(subject: Subject) {
    const BOUND: usize = 5;
    const TOTAL: usize = 14;
    for i in 0..TOTAL {
        set(&subject, &format!("k{i:02}"), b"v", HOUR).await;
        // Distinct write times: eviction is oldest-write-first.
        tokio::time::sleep(Duration::from_millis(3)).await;
    }
    if let Some(pruner) = &subject.pruner {
        for _ in 0..TOTAL {
            if pruner.prune().await.expect("prune") == 0 {
                break;
            }
        }
    }
    let mut present = Vec::new();
    for i in 0..TOTAL {
        if get(&subject, &format!("k{i:02}")).await.is_some() {
            present.push(i);
        }
    }
    assert_eq!(
        present.len(),
        BOUND,
        "entries surviving a bound of {BOUND}: {present:?}"
    );
    assert_eq!(
        present,
        ((TOTAL - BOUND)..TOTAL).collect::<Vec<_>>(),
        "the most recent writes survive; the oldest go first"
    );
}

async fn prunes_expired_entries(subject: Subject) {
    let Some(pruner) = subject.pruner.clone() else {
        return;
    };
    for i in 0..3 {
        set(&subject, &format!("short-{i}"), b"v", Some(SHORT_TTL)).await;
    }
    set(&subject, "long", b"v", HOUR).await;
    tokio::time::sleep(SHORT_TTL + Duration::from_millis(150)).await;
    assert_eq!(
        pruner.prune().await.expect("prune"),
        3,
        "the 3 expired entries"
    );
    assert!(
        get(&subject, "long").await.is_some(),
        "prune removed a live entry"
    );
    assert_eq!(pruner.prune().await.expect("prune"), 0);
}
