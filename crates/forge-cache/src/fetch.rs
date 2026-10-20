use std::{error::Error as StdError, future::Future, sync::Arc, time::Duration};

use serde::{Serialize, de::DeserializeOwned};
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
/// example the type changed in a deploy) is a **miss**, so a stale entry is
/// recomputed, never served or turned into an error.
pub async fn get_json<T, C>(cache: &C, key: &str) -> Result<Option<T>, CacheError>
where
    T: DeserializeOwned,
    C: Cache + ?Sized,
{
    Ok(cache
        .get(key)
        .await?
        .and_then(|raw| serde_json::from_slice(&raw).ok()))
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
    if let Ok(Some(value)) = get_json::<T, C>(cache, key).await {
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
