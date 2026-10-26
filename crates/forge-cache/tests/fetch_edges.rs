//! `fetch` and `fetch_json` against caches that answer from a script: a
//! value that appears between the first read and the lead, and writes that
//! fail after an entry that no longer decodes was replaced.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use forge_cache::{
    Cache, CacheError, FetchError, Flight, MemoryCache, MemoryOptions, Options, fetch, fetch_json,
};

const MINUTE: Option<Duration> = Some(Duration::from_secs(60));
type Never = std::convert::Infallible;

/// Reads come from a queue (then miss); writes fail the same way every time.
struct Scripted {
    flight: Flight,
    reads: Mutex<VecDeque<Option<Vec<u8>>>>,
    write: fn() -> Result<(), CacheError>,
    writes: AtomicUsize,
}

impl Scripted {
    fn new(reads: Vec<Option<Vec<u8>>>, write: fn() -> Result<(), CacheError>) -> Self {
        Self {
            flight: Flight::default(),
            reads: Mutex::new(reads.into()),
            write,
            writes: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl Cache for Scripted {
    async fn get(&self, _: &str) -> Result<Option<Vec<u8>>, CacheError> {
        Ok(self.reads.lock().unwrap().pop_front().flatten())
    }

    async fn set(&self, _: &str, _: &[u8], _: Option<Duration>) -> Result<(), CacheError> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        (self.write)()
    }

    async fn delete(&self, _: &str) -> Result<(), CacheError> {
        Ok(())
    }

    fn flight(&self) -> &Flight {
        &self.flight
    }
}

#[tokio::test]
async fn a_value_that_appears_before_the_lead_is_returned_without_computing() {
    // The first read misses; by the time this caller leads, another one filled it.
    let cache = Scripted::new(vec![None, Some(b"filled".to_vec())], || Ok(()));
    let computed = AtomicUsize::new(0);
    let value = fetch(&cache, "k", MINUTE, || async {
        computed.fetch_add(1, Ordering::SeqCst);
        Ok::<_, Never>(b"computed".to_vec())
    })
    .await
    .expect("fetch");
    assert_eq!(value, b"filled");
    assert_eq!(computed.load(Ordering::SeqCst), 0);
    assert_eq!(cache.writes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn replacing_an_entry_that_no_longer_decodes_reports_what_the_write_says() {
    // The stale entry is served by `fetch`, does not decode, and is replaced.
    let stale = || vec![Some(b"not json".to_vec()), Some(b"not json".to_vec())];
    let invalid_ttl = Scripted::new(stale(), || Err(CacheError::InvalidTtl));
    let too_large = Scripted::new(stale(), || Err(CacheError::ValueTooLarge));
    let down = Scripted::new(stale(), || Err(CacheError::Store("down".into())));
    let recompute = || async { Ok::<_, Never>(7_u32) };

    assert!(matches!(
        fetch_json(&invalid_ttl, "k", MINUTE, recompute).await,
        Err(FetchError::InvalidTtl)
    ));
    assert!(matches!(
        fetch_json(&too_large, "k", MINUTE, recompute).await,
        Err(FetchError::ValueTooLarge)
    ));
    // A store failure never fails the caller: the computed value is returned.
    assert_eq!(
        fetch_json(&down, "k", MINUTE, recompute)
            .await
            .expect("degrades"),
        7
    );
    assert_eq!(down.writes.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn shared_and_default_cache_surfaces_behave_like_the_cache_they_wrap() {
    let memory = Arc::new(MemoryCache::new(MemoryOptions::default()).expect("cache"));
    assert!(memory.is_empty());
    assert!(format!("{memory:?}").contains("MemoryCache"));
    // Through the `Arc` implementation, which forwards the stampede guard too.
    let value = fetch(&memory, "k", MINUTE, || async {
        Ok::<_, Never>(b"v".to_vec())
    })
    .await
    .expect("fetch");
    assert_eq!(value, b"v");
    assert_eq!(memory.len(), 1);
    assert!(Options::default().default_ttl() > Duration::ZERO);
}
