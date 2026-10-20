//! Behavior of the cache contracts that do not depend on a store: stampede
//! protection, degradation when the cache fails, JSON helpers and tenant keys.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{
    io,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use forge_auth::{Session, UnixTimestamp};
use forge_cache::{
    Cache, CacheError, FetchError, Flight, MemoryCache, MemoryOptions, fetch, fetch_json, get_json,
    key, set_json, tenant_key,
    testing::{Subject, run},
};
use forge_security::{Permission, PrincipalId, RbacPolicy, Role};
use forge_tenancy::{Membership, TenantContext, TenantId};
use serde::{Deserialize, Serialize};
use tracing_subscriber::fmt::MakeWriter;

type Never = std::convert::Infallible;
const MINUTE: Option<Duration> = Some(Duration::from_secs(60));

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn memory_cache_conforms_to_the_cache_contract() {
    run(|options| async move {
        let cache =
            MemoryCache::new(MemoryOptions::default().with_options(options)).expect("cache");
        Subject {
            cache: Arc::new(cache),
            pruner: None,
            guard: Box::new(()),
        }
    })
    .await;
}

/// A cache whose store always fails, like a database that is down.
struct Broken {
    flight: Flight,
    set_error: fn() -> CacheError,
}

fn outage() -> CacheError {
    CacheError::Store("db password=hunter2 down".into())
}

#[async_trait]
impl Cache for Broken {
    async fn get(&self, _key: &str) -> Result<Option<Vec<u8>>, CacheError> {
        Err(outage())
    }

    async fn set(
        &self,
        _key: &str,
        _value: &[u8],
        _ttl: Option<Duration>,
    ) -> Result<(), CacheError> {
        Err((self.set_error)())
    }

    async fn delete(&self, _key: &str) -> Result<(), CacheError> {
        Err(outage())
    }

    fn flight(&self) -> &Flight {
        &self.flight
    }
}

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
    type Writer = LogBuffer;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[tokio::test]
async fn fetch_degrades_when_the_store_fails_and_never_logs_keys_or_values() {
    let logs = LogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(logs.clone())
        .with_ansi(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let broken = Broken {
        flight: Flight::default(),
        set_error: outage,
    };

    let value = fetch(&broken, "session:alice-token-123", MINUTE, || async {
        Ok::<_, Never>(b"secret-value-456".to_vec())
    })
    .await
    .expect("the computed value despite the failing store");
    assert_eq!(value, b"secret-value-456");

    let output = String::from_utf8(logs.0.lock().expect("lock").clone()).expect("utf8");
    assert!(output.contains("cache read failed"), "{output}");
    assert!(output.contains("cache write failed"), "{output}");
    assert!(
        !output.contains("alice-token-123"),
        "a key leaked: {output}"
    );
    assert!(
        !output.contains("secret-value-456"),
        "a value leaked: {output}"
    );
}

#[tokio::test]
async fn fetch_reports_caller_bugs_instead_of_swallowing_them() {
    let too_large = Broken {
        flight: Flight::default(),
        set_error: || CacheError::ValueTooLarge,
    };
    let result = fetch(&too_large, "k", MINUTE, || async {
        Ok::<_, Never>(b"v".to_vec())
    })
    .await;
    assert!(
        matches!(result, Err(FetchError::ValueTooLarge)),
        "{result:?}"
    );
    let bad_ttl = Broken {
        flight: Flight::default(),
        set_error: || CacheError::InvalidTtl,
    };
    let result = fetch(&bad_ttl, "k", MINUTE, || async {
        Ok::<_, Never>(b"v".to_vec())
    })
    .await;
    assert!(matches!(result, Err(FetchError::InvalidTtl)), "{result:?}");
}

#[tokio::test]
async fn concurrent_misses_share_one_error_and_it_is_not_cached() {
    #[derive(Debug)]
    struct Down;
    impl std::fmt::Display for Down {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("origin down")
        }
    }
    impl std::error::Error for Down {}

    let cache = Arc::new(MemoryCache::new(MemoryOptions::default()).expect("cache"));
    let calls = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(tokio::sync::Notify::new());
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let cache = Arc::clone(&cache);
        let calls = Arc::clone(&calls);
        let release = Arc::clone(&release);
        tasks.spawn(async move {
            fetch(cache.as_ref(), "k", MINUTE, || async move {
                calls.fetch_add(1, Ordering::SeqCst);
                release.notified().await;
                Err::<Vec<u8>, _>(Down)
            })
            .await
        });
    }
    tokio::time::sleep(Duration::from_millis(150)).await;
    release.notify_one();
    while let Some(result) = tasks.join_next().await {
        let error = result
            .expect("task")
            .expect_err("every caller gets the error");
        assert!(matches!(error, FetchError::Compute(_)));
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(cache.get("k").await.expect("get"), None);
}

#[tokio::test]
async fn a_panicking_computation_releases_its_waiters_and_leaks_no_call() {
    let cache = Arc::new(MemoryCache::new(MemoryOptions::default()).expect("cache"));
    let started = Arc::new(tokio::sync::Notify::new());
    let leader = {
        let cache = Arc::clone(&cache);
        let started = Arc::clone(&started);
        tokio::spawn(async move {
            fetch(cache.as_ref(), "k", MINUTE, || async move {
                started.notify_one();
                tokio::time::sleep(Duration::from_millis(150)).await;
                panic!("boom");
                #[allow(unreachable_code)]
                Ok::<_, Never>(Vec::new())
            })
            .await
        })
    };
    started.notified().await;
    let waiter = {
        let cache = Arc::clone(&cache);
        tokio::spawn(async move {
            fetch(cache.as_ref(), "k", MINUTE, || async {
                Ok::<_, Never>(b"x".to_vec())
            })
            .await
        })
    };
    assert!(leader.await.expect_err("panicked").is_panic());
    let value = tokio::time::timeout(Duration::from_secs(5), waiter)
        .await
        .expect("a panicking leader left its waiters blocked")
        .expect("task")
        .expect("fetch");
    assert_eq!(value, b"x");
    // Nothing is in flight any more: a fresh fetch is served from the cache.
    let again = fetch(cache.as_ref(), "k", MINUTE, || async {
        Ok::<_, Never>(b"y".to_vec())
    })
    .await
    .expect("fetch");
    assert_eq!(again, b"x");
}

#[tokio::test]
async fn a_waiter_stops_waiting_when_it_is_dropped_and_the_leader_is_unaffected() {
    let cache = Arc::new(MemoryCache::new(MemoryOptions::default()).expect("cache"));
    let release = Arc::new(tokio::sync::Notify::new());
    let running = Arc::new(tokio::sync::Notify::new());
    let leader = {
        let cache = Arc::clone(&cache);
        let release = Arc::clone(&release);
        let running = Arc::clone(&running);
        tokio::spawn(async move {
            fetch(cache.as_ref(), "k", MINUTE, || async move {
                running.notify_one();
                release.notified().await;
                Ok::<_, Never>(b"leader".to_vec())
            })
            .await
        })
    };
    running.notified().await;
    let waiting = fetch(cache.as_ref(), "k", MINUTE, || async {
        Ok::<_, Never>(Vec::new())
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(50), waiting)
            .await
            .is_err(),
        "the waiter's own deadline ends its wait"
    );
    release.notify_one();
    assert_eq!(leader.await.expect("task").expect("fetch"), b"leader");
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct Profile {
    name: String,
    age: u32,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct ProfileV2 {
    full_name: String,
}

#[tokio::test]
async fn json_helpers_round_trip_and_treat_undecodable_entries_as_misses() {
    let cache = MemoryCache::new(MemoryOptions::default()).expect("cache");
    let ana = Profile {
        name: "Ana".into(),
        age: 31,
    };
    set_json(&cache, "p", &ana, MINUTE).await.expect("set");
    assert_eq!(
        get_json::<Profile, _>(&cache, "p").await.expect("get"),
        Some(ana)
    );

    // The type changed in a deploy: the stale entry is a miss, never an error.
    assert_eq!(
        get_json::<ProfileV2, _>(&cache, "p").await.expect("get"),
        None
    );
    let fresh = fetch_json(&cache, "p", MINUTE, || async {
        Ok::<_, Never>(ProfileV2 {
            full_name: "Ana Maria".into(),
        })
    })
    .await
    .expect("fetch_json recomputes");
    assert_eq!(fresh.full_name, "Ana Maria");
    assert_eq!(
        get_json::<ProfileV2, _>(&cache, "p")
            .await
            .expect("get")
            .map(|p| p.full_name),
        Some("Ana Maria".to_owned()),
        "the stale entry was replaced"
    );

    cache
        .set("garbage", b"not json", MINUTE)
        .await
        .expect("set");
    assert_eq!(
        get_json::<Profile, _>(&cache, "garbage")
            .await
            .expect("get"),
        None
    );
}

#[tokio::test]
async fn fetch_json_computes_once_and_propagates_errors() {
    #[derive(Debug)]
    struct Down;
    impl std::fmt::Display for Down {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("origin down")
        }
    }
    impl std::error::Error for Down {}

    let cache = MemoryCache::new(MemoryOptions::default()).expect("cache");
    let calls = AtomicUsize::new(0);
    for _ in 0..3 {
        let value: u32 = fetch_json(&cache, "n", MINUTE, || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok::<_, Never>(42)
        })
        .await
        .expect("fetch_json");
        assert_eq!(value, 42);
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let error = fetch_json::<u32, _, _, _, _>(&cache, "other", MINUTE, || async { Err(Down) })
        .await
        .expect_err("compute error");
    assert!(matches!(error, FetchError::Compute(_)));
    assert_eq!(cache.get("other").await.expect("get"), None);
}

fn tenant_context(tenant: TenantId) -> TenantContext {
    let principal = PrincipalId::new();
    let role = Role::new("member").expect("role");
    let permission = Permission::new("reports:read").expect("permission");
    let membership = Membership::active(tenant, principal, vec![role.clone()]);
    let mut policy = RbacPolicy::new();
    policy.allow(role, permission.clone());
    let authenticated = Session::new(
        principal,
        UnixTimestamp::from_secs(10),
        UnixTimestamp::from_secs(20),
    )
    .expect("session")
    .authenticate(UnixTimestamp::from_secs(11))
    .expect("authenticate");
    TenantContext::authorize(authenticated, &membership, &policy, &permission).expect("context")
}

#[test]
fn tenant_keys_never_collide_across_tenants_or_with_plain_keys() {
    let a = tenant_context(TenantId::new());
    let b = tenant_context(TenantId::new());
    let a_again = tenant_context(a.tenant_id());
    assert_eq!(
        tenant_key(&a, "invoices", &["7"]),
        tenant_key(&a_again, "invoices", &["7"]),
        "the key depends on the tenant, not on the principal"
    );
    assert_ne!(
        tenant_key(&a, "invoices", &["7"]),
        tenant_key(&b, "invoices", &["7"])
    );
    assert_ne!(
        tenant_key(&a, "invoices", &["7"]),
        tenant_key(&a, "invoices", &["8"])
    );
    assert_ne!(tenant_key(&a, "invoices", &[]), key("invoices", &[]));
    // A user-controlled part cannot reach into another tenant's keys.
    let other = b.tenant_id().to_string();
    assert_ne!(
        tenant_key(&a, "invoices", &[&other, "7"]),
        tenant_key(&b, "invoices", &["7"]),
    );
}
