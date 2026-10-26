use std::{
    collections::BTreeSet,
    error::Error as StdError,
    future::Future,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use hmac::{Hmac, KeyInit, Mac};
use serde::{Serialize, de::DeserializeOwned};
use sha2::Sha256;
use thiserror::Error;
use tracing::warn;

use crate::{Cache, CacheError, check_key, flight};

/// Why [`fetch`] failed. A failing *cache* never fails a fetch (the value is
/// computed without it); these are the caller's mistakes and the caller's own
/// computation failing.
#[derive(Clone, Debug, Error)]
#[non_exhaustive]
pub enum FetchError {
    /// See [`CacheError::InvalidKey`].
    #[error("cache key must be 1 to 1024 bytes without NUL")]
    InvalidKey,
    /// See [`CacheError::InvalidTtl`].
    #[error("cache ttl must be between 1ms and one year")]
    InvalidTtl,
    /// See [`CacheError::ValueTooLarge`].
    #[error("cache value too large")]
    ValueTooLarge,
    /// The computation failed; nothing was cached. Concurrent callers that
    /// shared the computation all receive the same error.
    #[error("cache computation failed")]
    Compute(#[source] Arc<dyn StdError + Send + Sync>),
}

/// What a computation published to its waiters.
pub(crate) type Outcome = Result<Arc<Vec<u8>>, FetchError>;

/// Read-through: the cached value of `key`, or on a miss the result of
/// `compute`, stored for `ttl` (`None` selects the default).
///
/// Concurrent misses of one key in the same process share one `compute` call
/// (see [`Flight`](crate::Flight)); every caller receives its own copy of the
/// bytes. An error from `compute` is returned and nothing is stored. Failures
/// of the cache itself never fail the fetch: an unreadable cache is a miss and
/// a failed write is logged (without key or value) and the computed value is
/// returned. Errors that are the caller's bug (an invalid key or ttl, a value
/// over the bound) are returned.
///
/// If the caller computing the value is dropped, a waiting caller takes over
/// and computes with its own closure instead of inheriting the cancellation.
pub async fn fetch<C, F, Fut, E>(
    cache: &C,
    key: &str,
    ttl: Option<Duration>,
    compute: F,
) -> Result<Vec<u8>, FetchError>
where
    C: Cache + ?Sized,
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<Vec<u8>, E>>,
    E: StdError + Send + Sync + 'static,
{
    check_key(key).map_err(|_| FetchError::InvalidKey)?;
    match cache.get(key).await {
        Ok(Some(value)) => return Ok(value),
        Ok(None) => {}
        Err(error) => warn!(%error, "cache read failed; computing without the cache"),
    }
    let mut compute = Some(compute);
    loop {
        match cache.flight().join(key) {
            flight::Role::Wait(slot) => match flight::wait(slot).await {
                Some(Ok(value)) => return Ok(value.as_ref().clone()),
                Some(Err(error)) => return Err(error),
                // The leader went away without a result: try again, possibly
                // as the new leader.
                None => {}
            },
            flight::Role::Lead(lead) => {
                // A caller that finished just before us may have filled the cache.
                if let Ok(Some(value)) = cache.get(key).await {
                    lead.finish(Ok(Arc::new(value.clone())));
                    return Ok(value);
                }
                let Some(compute) = compute.take() else {
                    // Only reachable if this caller already led and lost the
                    // closure; it cannot happen, but never loop forever.
                    return Err(FetchError::Compute(Arc::new(CacheError::Configuration(
                        "cache computation was already consumed",
                    ))));
                };
                let outcome = match compute().await {
                    Ok(value) => match cache.set(key, &value, ttl).await {
                        Ok(()) => Ok(value),
                        Err(CacheError::InvalidTtl) => Err(FetchError::InvalidTtl),
                        Err(CacheError::ValueTooLarge) => Err(FetchError::ValueTooLarge),
                        Err(error) => {
                            warn!(%error, "cache write failed; returning the computed value");
                            Ok(value)
                        }
                    },
                    Err(error) => Err(FetchError::Compute(Arc::new(error))),
                };
                return match outcome {
                    Ok(value) => {
                        lead.finish(Ok(Arc::new(value.clone())));
                        Ok(value)
                    }
                    Err(error) => {
                        lead.finish(Err(error.clone()));
                        Err(error)
                    }
                };
            }
        }
    }
}

/// The value under `key` decoded as JSON. A value that no longer decodes (for
/// example the type changed in a deploy, or an entry written by something
/// else) is a **miss**, so a stale entry is recomputed, never served or
/// turned into an error. It is not silent: it is counted and logged once per
/// key (see [`decode_failures`]).
pub async fn get_json<T, C>(cache: &C, key: &str) -> Result<Option<T>, CacheError>
where
    T: DeserializeOwned,
    C: Cache + ?Sized,
{
    read_json(cache, key, true).await
}

/// [`get_json`]; `report: false` leaves a decode failure unrecorded, for a
/// caller that reads the entry again and records it then.
async fn read_json<T, C>(cache: &C, key: &str, report: bool) -> Result<Option<T>, CacheError>
where
    T: DeserializeOwned,
    C: Cache + ?Sized,
{
    let Some(raw) = cache.get(key).await? else {
        return Ok(None);
    };
    match serde_json::from_slice(&raw) {
        Ok(value) => Ok(Some(value)),
        Err(_) => {
            if report {
                record_decode_failure(key);
            }
            Ok(None)
        }
    }
}

/// Cached values [`get_json`] and [`fetch_json`] could not decode.
static DECODE_FAILURES: AtomicU64 = AtomicU64::new(0);

/// Most keys whose first decode failure is logged, which bounds the memory
/// the log-once set holds (Forge-Go's `maxLoggedDecodeKeys`).
const MAX_LOGGED_DECODE_KEYS: usize = 1024;

/// The HMAC prefixes of the keys whose decode failure was already logged.
static LOGGED_DECODE_KEYS: Mutex<BTreeSet<[u8; 8]>> = Mutex::new(BTreeSet::new());

/// How many cached values [`get_json`] and [`fetch_json`] found that no
/// longer decoded into the requested type since the process started; each
/// was treated as a miss. The first failure of each key (up to 1024 keys per
/// process) is also logged at warn level as `cached value does not decode`
/// with `key_hmac`: an HMAC-SHA256 prefix of the key under a random
/// per-process secret, never the key or the value, so a low-entropy key such
/// as an email address cannot be recovered by hashing guesses (the same key
/// logs a different `key_hmac` in each process). A growing count after a
/// deploy means a type changed without a new key version: put a version in
/// the key (`forge_cache::key("price", &["v2", sku])`). Forge-Go's
/// `cache.DecodeFailures`.
#[must_use]
pub fn decode_failures() -> u64 {
    DECODE_FAILURES.load(Ordering::Relaxed)
}

/// Counts a decode failure of `key` and logs the first one of each key.
fn record_decode_failure(key: &str) {
    DECODE_FAILURES.fetch_add(1, Ordering::Relaxed);
    if let Some(key_hmac) = first_failure(key) {
        warn!(
            %key_hmac,
            "cached value does not decode; treated as a miss (logged once per key)"
        );
    }
}

/// The hex HMAC prefix of `key` when its failure is the first one to log,
/// else `None` (already logged, the set is full, or no secret).
fn first_failure(key: &str) -> Option<String> {
    let prefix = key_hmac(key)?;
    let mut logged = LOGGED_DECODE_KEYS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if logged.contains(&prefix) || logged.len() >= MAX_LOGGED_DECODE_KEYS {
        return None;
    }
    logged.insert(prefix);
    Some(prefix.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// The first 8 bytes of HMAC-SHA256(`key`) under [`key_secret`], or `None`
/// without a secret (the system's random source failed): such a key is
/// counted, never named.
fn key_hmac(key: &str) -> Option<[u8; 8]> {
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(key_secret()?).ok()?;
    mac.update(key.as_bytes());
    let digest = mac.finalize().into_bytes();
    let mut prefix = [0; 8];
    prefix.copy_from_slice(digest.get(..8)?);
    Some(prefix)
}

/// 32 random bytes drawn once per process.
fn key_secret() -> Option<&'static [u8; 32]> {
    static SECRET: OnceLock<Option<[u8; 32]>> = OnceLock::new();
    SECRET
        .get_or_init(|| {
            let mut secret = [0; 32];
            getrandom::fill(&mut secret).ok().map(|()| secret)
        })
        .as_ref()
}

/// Stores `value` as JSON under `key` for `ttl`.
pub async fn set_json<T, C>(
    cache: &C,
    key: &str,
    value: &T,
    ttl: Option<Duration>,
) -> Result<(), CacheError>
where
    T: Serialize + ?Sized,
    C: Cache + ?Sized,
{
    let raw = serde_json::to_vec(value).map_err(|error| CacheError::Store(Box::new(error)))?;
    cache.set(key, &raw, ttl).await
}

/// [`fetch`] for a typed value: the result of `compute` is stored as JSON and a
/// cached value that no longer decodes is recomputed.
pub async fn fetch_json<T, C, F, Fut, E>(
    cache: &C,
    key: &str,
    ttl: Option<Duration>,
    compute: F,
) -> Result<T, FetchError>
where
    T: Serialize + DeserializeOwned,
    C: Cache + ?Sized,
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<T, E>>,
    E: StdError + Send + Sync + 'static,
{
    // A value that does not decode is recorded below, where `fetch` reads it
    // again: once per call, not twice.
    if let Ok(Some(value)) = read_json::<T, C>(cache, key, false).await {
        return Ok(value);
    }
    let mut compute = Some(compute);
    let raw = fetch(cache, key, ttl, || {
        let step = compute.take();
        async move {
            let Some(step) = step else {
                return Err(JsonStep::Consumed);
            };
            let value = step().await.map_err(JsonStep::Compute)?;
            serde_json::to_vec(&value).map_err(JsonStep::Encode)
        }
    })
    .await?;
    if let Ok(value) = serde_json::from_slice(&raw) {
        return Ok(value);
    }
    // `fetch` returned a cached entry that does not decode (the type changed in
    // a deploy): compute afresh and replace it.
    record_decode_failure(key);
    let Some(compute) = compute.take() else {
        return Err(FetchError::Compute(Arc::new(CacheError::Configuration(
            "cache computed value does not round-trip through JSON",
        ))));
    };
    let value = compute()
        .await
        .map_err(|error| FetchError::Compute(Arc::new(error)))?;
    match set_json(cache, key, &value, ttl).await {
        Ok(()) => {}
        Err(CacheError::InvalidTtl) => return Err(FetchError::InvalidTtl),
        Err(CacheError::ValueTooLarge) => return Err(FetchError::ValueTooLarge),
        Err(error) => warn!(%error, "cache write failed; returning the computed value"),
    }
    Ok(value)
}

/// The ways the JSON computation of [`fetch_json`] fails.
#[derive(Debug, Error)]
enum JsonStep<E: StdError + 'static> {
    #[error(transparent)]
    Compute(E),
    #[error("could not encode the computed value")]
    Encode(#[source] serde_json::Error),
    #[error("the computation was already consumed")]
    Consumed,
}

#[cfg(test)]
mod tests {
    use super::{first_failure, key_hmac};

    #[test]
    fn a_key_is_named_by_a_keyed_hash_prefix_once() {
        let key = "user:alice@example.com";
        let hmac = key_hmac(key).expect("a random secret");
        assert_eq!(key_hmac(key), Some(hmac), "stable within the process");
        assert_ne!(key_hmac("user:bob@example.com"), Some(hmac));
        let logged = first_failure(key).expect("the first failure is logged");
        assert_eq!(logged.len(), 16);
        assert!(logged.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert!(!logged.contains("alice"));
        assert_eq!(first_failure(key), None, "logged once per key");
        // Not the unkeyed SHA-256 of the key, which a guess could reproduce.
        use sha2::{Digest, Sha256};
        assert_ne!(Sha256::digest(key.as_bytes())[..8], hmac);
    }
}
