//! Cached values that no longer decode are counted and logged once per key,
//! by a keyed hash of the key, never the key or the value (forge-rust#85.5,
//! Forge-Go's `cache.DecodeFailures`). Its own binary: the count is global.

use std::{
    io,
    sync::{Arc, Mutex},
    time::Duration,
};

use forge_cache::{Cache, MemoryCache, MemoryOptions, decode_failures, fetch_json, get_json};
use serde::{Deserialize, Serialize};
use tracing_subscriber::fmt::MakeWriter;

#[derive(Clone, Default)]
struct LogBuffer(Arc<Mutex<Vec<u8>>>);

impl io::Write for LogBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().expect("lock").extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for LogBuffer {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct Price {
    cents: u64,
}

#[tokio::test]
async fn undecodable_values_are_counted_and_logged_once_per_key_without_key_or_value() {
    let logs = LogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(logs.clone())
        .with_ansi(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let cache = MemoryCache::new(MemoryOptions::default()).expect("cache");
    let ttl = Some(Duration::from_secs(60));
    let (alice, bob) = ("price:alice@example.com", "price:bob@example.com");
    for key in [alice, bob] {
        cache
            .set(key, b"{\"secret-value-789\": true}", ttl)
            .await
            .expect("set");
    }
    assert_eq!(decode_failures(), 0);

    for _ in 0..3 {
        assert!(
            get_json::<Price, _>(&cache, alice)
                .await
                .expect("get")
                .is_none()
        );
    }
    assert_eq!(decode_failures(), 3, "every failure counts");
    // fetch_json reads the entry twice but counts it once, and replaces it.
    let price = fetch_json(&cache, bob, ttl, || async {
        Ok::<_, std::convert::Infallible>(Price { cents: 120 })
    })
    .await
    .expect("recomputed");
    assert_eq!(price.cents, 120);
    assert_eq!(decode_failures(), 4);
    assert!(
        get_json::<Price, _>(&cache, bob)
            .await
            .expect("get")
            .is_some()
    );
    assert_eq!(decode_failures(), 4, "the replaced entry decodes");

    let output = String::from_utf8(logs.0.lock().expect("lock").clone()).expect("utf8");
    let warnings: Vec<&str> = output
        .lines()
        .filter(|line| line.contains("cached value does not decode"))
        .collect();
    assert_eq!(warnings.len(), 2, "once per key: {output}");
    assert!(
        warnings
            .iter()
            .all(|line| line.contains("WARN") && line.contains("key_hmac="))
    );
    assert_ne!(warnings[0], warnings[1], "each key has its own key_hmac");
    for secret in ["alice", "bob", "example.com", "secret-value-789"] {
        assert!(!output.contains(secret), "{secret} leaked: {output}");
    }
}
