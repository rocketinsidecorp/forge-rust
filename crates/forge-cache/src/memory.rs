use std::{
    collections::{BTreeMap, HashMap},
    sync::{Mutex, MutexGuard, PoisonError},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use forge_config::CacheConfig;

use crate::{Cache, CacheError, Flight, MAX_KEY_LENGTH, Options, check_key};

/// Bounds of a [`MemoryCache`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MemoryOptions {
    options: Options,
    max_bytes: u64,
}

const DEFAULT_MAX_BYTES: u64 = 64 << 20;
/// Approximate bookkeeping of one entry, so many tiny entries cannot defeat
/// `max_bytes`.
const ENTRY_OVERHEAD: u64 = 96;
/// Entries at the cold end checked for expiry on every write.
const COLD_CHECKS: usize = 4;

impl Default for MemoryOptions {
    fn default() -> Self {
        Self {
            options: Options::default(),
            max_bytes: DEFAULT_MAX_BYTES,
        }
    }
}

impl MemoryOptions {
    /// The options the `FORGE_CACHE_*` variables select.
    #[must_use]
    pub fn from_config(config: &CacheConfig) -> Self {
        Self {
            options: Options::from_config(config),
            max_bytes: config.max_bytes,
        }
    }

    /// The shared bounds (entries, value size, default ttl).
    #[must_use]
    pub const fn with_options(mut self, options: Options) -> Self {
        self.options = options;
        self
    }

    /// Bounds the memory of keys and values together (default 64 MiB). It must
    /// hold at least one maximum-size value.
    #[must_use]
    pub const fn with_max_bytes(mut self, max_bytes: u64) -> Self {
        self.max_bytes = max_bytes;
        self
    }
}

type Clock = Box<dyn Fn() -> Instant + Send + Sync>;

/// A [`Cache`] in process memory with LRU eviction and per-entry expiry,
/// bounded by entries and by bytes.
///
/// It starts no tasks: expired entries are dropped when read and when writes
/// need room (a few cold entries are checked on every write), and the least
/// recently used entries leave first. It is local to one process: replicas do
/// not share it and it is empty after a restart.
pub struct MemoryCache {
    options: MemoryOptions,
    clock: Clock,
    flight: Flight,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    entries: HashMap<String, Entry>,
    /// Entries by recency: the smallest tick is the least recently used.
    recency: BTreeMap<u64, String>,
    tick: u64,
    bytes: u64,
}

struct Entry {
    value: Vec<u8>,
    expires_at: Instant,
    tick: u64,
}

fn cost(key: &str, value: &[u8]) -> u64 {
    (key.len() + value.len()) as u64 + ENTRY_OVERHEAD
}

impl std::fmt::Debug for MemoryCache {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MemoryCache")
            .field("options", &self.options)
            .finish_non_exhaustive()
    }
}

impl MemoryCache {
    /// An empty cache.
    pub fn new(options: MemoryOptions) -> Result<Self, CacheError> {
        Self::with_clock(options, Box::new(Instant::now))
    }

    pub(crate) fn with_clock(options: MemoryOptions, clock: Clock) -> Result<Self, CacheError> {
        let base = options.options.validated()?;
        let floor = base.max_value_bytes() as u64 + ENTRY_OVERHEAD + MAX_KEY_LENGTH as u64;
        if options.max_bytes < floor {
            return Err(CacheError::Configuration(
                "cache max bytes must hold at least one maximum-size value",
            ));
        }
        Ok(Self {
            options: MemoryOptions {
                options: base,
                ..options
            },
            clock,
            flight: Flight::default(),
            state: Mutex::new(State::default()),
        })
    }

    /// Entries held, expired ones included until they are dropped.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().entries.len()
    }

    /// Whether no entry is held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl State {
    fn remove(&mut self, key: &str) {
        if let Some(entry) = self.entries.remove(key) {
            self.recency.remove(&entry.tick);
            self.bytes -= cost(key, &entry.value);
        }
    }

    fn touch(&mut self, key: &str) {
        self.tick += 1;
        let tick = self.tick;
        if let Some(entry) = self.entries.get_mut(key) {
            self.recency.remove(&entry.tick);
            entry.tick = tick;
            self.recency.insert(tick, key.to_owned());
        }
    }

    /// Drops expired entries from the cold end, then least recently used ones,
    /// until the bounds hold again.
    fn make_room(&mut self, now: Instant, max_entries: usize, max_bytes: u64) {
        // A few cold entries are checked on every write, so expired entries do
        // not wait for capacity pressure.
        let cold: Vec<String> = self.recency.values().take(COLD_CHECKS).cloned().collect();
        for key in cold {
            if self
                .entries
                .get(&key)
                .is_some_and(|entry| now >= entry.expires_at)
            {
                self.remove(&key);
            }
        }
        while self.entries.len() > max_entries || self.bytes > max_bytes {
            let Some(oldest) = self.recency.values().next().cloned() else {
                break;
            };
            self.remove(&oldest);
        }
    }
}

#[async_trait]
impl Cache for MemoryCache {
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, CacheError> {
        check_key(key)?;
        let now = (self.clock)();
        let mut state = self.lock();
        let Some(entry) = state.entries.get(key) else {
            return Ok(None);
        };
        if now >= entry.expires_at {
            state.remove(key);
            return Ok(None);
        }
        let value = entry.value.clone();
        state.touch(key);
        Ok(Some(value))
    }

    async fn set(&self, key: &str, value: &[u8], ttl: Option<Duration>) -> Result<(), CacheError> {
        let ttl = self.options.options.check_set(key, value, ttl)?;
        let now = (self.clock)();
        let expires_at = now
            .checked_add(ttl)
            .ok_or(CacheError::Configuration("cache ttl overflows the clock"))?;
        let mut state = self.lock();
        state.remove(key);
        state.tick += 1;
        let tick = state.tick;
        state.bytes += cost(key, value);
        state.recency.insert(tick, key.to_owned());
        state.entries.insert(
            key.to_owned(),
            Entry {
                value: value.to_vec(),
                expires_at,
                tick,
            },
        );
        state.make_room(
            now,
            self.options.options.max_entries(),
            self.options.max_bytes,
        );
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<(), CacheError> {
        check_key(key)?;
        self.lock().remove(key);
        Ok(())
    }

    fn flight(&self) -> &Flight {
        &self.flight
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };

    use super::*;

    #[derive(Clone)]
    pub(crate) struct ManualClock {
        base: Instant,
        offset_nanos: Arc<AtomicU64>,
    }

    impl ManualClock {
        pub(crate) fn new() -> Self {
            Self {
                base: Instant::now(),
                offset_nanos: Arc::new(AtomicU64::new(0)),
            }
        }

        pub(crate) fn advance(&self, by: Duration) {
            let nanos = u64::try_from(by.as_nanos()).unwrap_or(u64::MAX);
            self.offset_nanos.fetch_add(nanos, Ordering::SeqCst);
        }

        pub(crate) fn boxed(&self) -> Clock {
            let clock = self.clone();
            Box::new(move || {
                clock.base + Duration::from_nanos(clock.offset_nanos.load(Ordering::SeqCst))
            })
        }
    }

    fn cache(options: MemoryOptions) -> (MemoryCache, ManualClock) {
        let clock = ManualClock::new();
        (
            MemoryCache::with_clock(options, clock.boxed()).expect("valid options"),
            clock,
        )
    }

    fn hour() -> Option<Duration> {
        Some(Duration::from_secs(3600))
    }

    #[tokio::test]
    async fn evicts_the_least_recently_used_entry() {
        let (cache, _clock) =
            cache(MemoryOptions::default().with_options(Options::default().with_max_entries(3)));
        for key in ["a", "b", "c"] {
            cache.set(key, b"v", hour()).await.expect("set");
        }
        // Reading "a" makes "b" the least recently used.
        assert!(cache.get("a").await.expect("get").is_some());
        cache.set("d", b"v", hour()).await.expect("set");
        assert_eq!(cache.len(), 3);
        assert!(
            cache.get("b").await.expect("get").is_none(),
            "b was evicted"
        );
        for key in ["a", "c", "d"] {
            assert!(cache.get(key).await.expect("get").is_some(), "{key}");
        }
    }

    #[tokio::test]
    async fn is_bounded_by_bytes_not_only_by_entries() {
        let options = MemoryOptions::default()
            .with_options(Options::default().with_max_value_bytes(1000))
            .with_max_bytes(1000 + ENTRY_OVERHEAD + MAX_KEY_LENGTH as u64 + 2000);
        let (cache, _clock) = cache(options);
        let value = [0_u8; 1000];
        for i in 0..50 {
            cache
                .set(&format!("k{i}"), &value, hour())
                .await
                .expect("set");
        }
        let held = cache.lock().bytes;
        assert!(held <= cache.options.max_bytes, "{held} bytes held");
        assert!(cache.len() < 50);
        // Many tiny entries cannot defeat the byte bound either.
        let (tiny, _clock) = self::cache(
            MemoryOptions::default()
                .with_options(Options::default().with_max_value_bytes(1))
                .with_max_bytes(1 + ENTRY_OVERHEAD + MAX_KEY_LENGTH as u64 + 1000),
        );
        for i in 0..100 {
            tiny.set(&format!("t{i:03}"), b"x", hour())
                .await
                .expect("set");
        }
        assert!(tiny.len() < 100);
    }

    #[tokio::test]
    async fn expired_entries_leave_without_reads_and_never_serve() {
        let (cache, clock) = cache(MemoryOptions::default());
        for i in 0..10 {
            cache
                .set(&format!("old{i}"), b"v", Some(Duration::from_secs(1)))
                .await
                .expect("set");
        }
        clock.advance(Duration::from_secs(2));
        // Writes check a few cold entries each, so expired ones drain.
        for i in 0..5 {
            cache
                .set(&format!("new{i}"), b"v", hour())
                .await
                .expect("set");
        }
        assert_eq!(
            cache.len(),
            5,
            "the ten expired entries drained, four per write"
        );
        assert!(cache.get("old9").await.expect("get").is_none());
        assert!(cache.get("new0").await.expect("get").is_some());
    }

    #[tokio::test]
    async fn ttl_follows_the_injected_clock() {
        let (cache, clock) = cache(MemoryOptions::default());
        cache
            .set("k", b"v", Some(Duration::from_secs(10)))
            .await
            .expect("set");
        clock.advance(Duration::from_secs(9));
        assert!(cache.get("k").await.expect("get").is_some());
        clock.advance(Duration::from_secs(1));
        assert!(cache.get("k").await.expect("get").is_none());
        assert_eq!(cache.len(), 0, "an expired read drops the entry");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_use_stays_bounded() {
        let cache = Arc::new(
            MemoryCache::new(
                MemoryOptions::default().with_options(Options::default().with_max_entries(50)),
            )
            .expect("cache"),
        );
        let mut tasks = tokio::task::JoinSet::new();
        for task in 0..8 {
            let cache = Arc::clone(&cache);
            tasks.spawn(async move {
                for i in 0..200 {
                    let key = format!("k{}", (task * 31 + i) % 80);
                    cache.set(&key, b"value", hour()).await.expect("set");
                    cache.get(&key).await.expect("get");
                    if i % 7 == 0 {
                        cache.delete(&key).await.expect("delete");
                    }
                }
            });
        }
        while let Some(result) = tasks.join_next().await {
            result.expect("task");
        }
        assert!(cache.len() <= 50);
    }

    #[test]
    fn options_are_validated() {
        assert!(matches!(
            MemoryCache::new(MemoryOptions::default().with_max_bytes(10)),
            Err(CacheError::Configuration(_))
        ));
        assert!(matches!(
            MemoryCache::new(
                MemoryOptions::default().with_options(Options::default().with_max_entries(0))
            ),
            Err(CacheError::Configuration(_))
        ));
        let config = CacheConfig::default();
        assert!(MemoryCache::new(MemoryOptions::from_config(&config)).is_ok());
    }
}
