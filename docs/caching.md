# Caching

`forge-cache` is the equivalent of Rails' Solid Cache: a key/value cache with
expiry that lives in the database you already run, plus a bounded in-memory
cache. The design is in [ADR 0028](adr/0028-cache.md).

```toml
# Cargo.toml of the application (re-exported as `forge::cache`)
forge = { version = "0.1", features = ["cache"] }   # plus the backend you use
```

## The interface

```rust
#[async_trait]
pub trait Cache: Send + Sync {
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, CacheError>;
    async fn set(&self, key: &str, value: &[u8], ttl: Option<Duration>) -> Result<(), CacheError>;
    async fn delete(&self, key: &str) -> Result<(), CacheError>;
}
```

```rust
use forge::cache::{fetch_json, key};

let price: Price = fetch_json(
    cache.as_ref(),
    &key("price", &[sku]),
    Some(Duration::from_secs(600)),
    || async { catalog.price(sku).await },
)
.await?;
```

- `fetch` is read-through: on a miss it runs `compute` **once per key per
  process** however many requests wait for it (stampede protection), stores the
  result and returns it. Errors are returned (as `FetchError::Compute`) and
  never cached. If the request that is computing is cancelled (its future is
  dropped), a waiting caller takes over instead of inheriting the cancellation.
- A cache failure never fails `fetch`: an unreadable cache is a miss, a failed
  write is logged (without key or value) and the computed value is returned.
  Direct `get`/`set`/`delete` report store errors.
- A cached value that no longer decodes (`get_json`/`fetch_json`; for example the
  struct changed in a deploy) is a miss and is recomputed.
- `ttl` of `None` means the store's default; zero or more than a year is an
  error. Nothing lives forever.
- Values are copied. Keys are 1 to 1024 bytes without NUL; values at most
  `max_value_bytes` (default 1 MiB, hard limit 8 MiB). Put large objects in
  [storage](storage.md).
- Across replicas the stampede protection does not coordinate: `N` replicas can
  compute a cold key up to `N` times.

## Stores

| Store | Type | Shared across replicas | Survives restart |
| --- | --- | --- | --- |
| Memory (LRU + ttl) | `MemoryCache` | no | no |
| PostgreSQL | `postgres::PostgresCache` | yes | yes |
| MySQL, MariaDB | `mysql::MySqlCache` | yes | yes |
| SQLite | `sqlite::SqliteCache` | processes of one host | yes |

```rust
// In memory:
let cache: Arc<dyn Cache> = Arc::new(MemoryCache::new(MemoryOptions::from_config(&config.cache))?);

// PostgreSQL (enable the `postgres` feature; mysql::/sqlite:: are analogous).
// Apply the table with the application's own migrations:
let migrations = forge::db::combine_migrations(&[&APP_MIGRATIONS, &forge::cache::postgres::MIGRATIONS])?;
App::new("shop").postgres(migrations) /* ... */;
// At startup:
let store_options = StoreOptions::default().with_options(Options::from_config(&config.cache));
let cache: Arc<dyn Cache> = Arc::new(PostgresCache::new(db.clone(), store_options)?);
```

`cargo run -- migrate` creates `forge_cache_entries`. `cargo run -- rollback`
never drops it: the application rollback reverts only the application's own
migrations, and turning the shared cache off later does not break `migrate`
([ADR 0030](adr/0030-framework-migration-ownership.md)). Dropping the table is
explicit: `database.rollback_framework(&migrations)`.
Database rows are addressed by `SHA-256(key)`: keys are never stored, indexed or
logged (they often name users or sessions), and cannot be listed or deleted by
pattern. Namespace keys (`key("users", &[id, "profile"])`) and let ttl and
eviction clean up.

## Configuration

| Variable | Default | Meaning |
| --- | --- | --- |
| `FORGE_CACHE_MAX_ENTRIES` | `10000` | Entry bound of every cache |
| `FORGE_CACHE_MAX_BYTES` | `67108864` | Memory bound of the in-memory cache (keys, values and per-entry overhead) |
| `FORGE_CACHE_MAX_VALUE_BYTES` | `1048576` | Largest cached value, at most `8388608`; the byte bound must exceed it by 4096 |
| `FORGE_CACHE_DEFAULT_TTL` | `1h` | ttl used when a write passes none, a duration from `1ms` to `8760h` |

Invalid values stop the process at startup.

## Bounds and pruning

`MemoryCache` enforces `max_entries` and `max_bytes` on every write by evicting
the least recently used entry; it drops expired entries when read and checks a
few cold ones on each write, and starts no task.

The database stores are pruned in bounded batches: expired entries first, then
the **oldest writes** while more than `max_entries` remain (FIFO by write time,
like Solid Cache; reads are not tracked). One `prune()` call runs at most
`MAX_PRUNE_ROUNDS` (20) batches of `prune_batch` (default 500) of each kind.
Writes call it inline **after every `prune_batch` writes or every
`prune_interval` (default one minute), whichever comes first**
(`with_prune_interval(None)` disables it), so sustained writes of new keys
cannot outrun eviction. You can also call `Pruner::prune` from a scheduled job.

The entry bound on the database stores is **soft but tight**: each process
prunes after every `prune_batch` writes, and a prune evicts the whole excess
(up to 20 batches), so the table stays within about `prune_batch` entries per
writing process, plus writes in flight, of `max_entries`. Each prune counts the
table (a scan): with millions of rows, raise `prune_batch` and leave headroom
in `max_entries`.

## Security notes

- **Poisoning.** Whoever can write a key controls what every reader of that key
  sees. Never build a key from untrusted input without a namespace and `key`
  (the namespace and every part are length-prefixed, so neither a part nor a
  dynamic namespace such as `"tenant:"+id` can reach into another namespace),
  and never cache per-user data under a key a different user can compute or
  choose. Cache entries are untrusted bytes: decode them strictly (`get_json`
  does) and do not cache authorization decisions longer than the permission can
  change.
- **Secrets.** Do not put secrets or tokens in values you would not keep in the
  database in clear text; cache rows are plain application data. Keys are hashed
  at rest, and Forge never logs keys or values.
- **Tenancy.** The cache is not tenant-aware. Build tenant data keys with
  `tenant_key(&tenant_context, "invoices", &[id])`: it takes the authorized
  `TenantContext`, so a key cannot be built for a tenant the caller is not
  authorized for. A key without the tenant is shared by all tenants.
- **Access.** The table is as writable as the application's database role. Do not
  share the cache table with a less trusted application.
- **Size.** Values are bounded (1 MiB default) and entries are bounded, so a
  client that causes many distinct keys cannot grow the cache without limit; it
  can evict useful entries, which only costs recomputation.

## Testing

`forge_cache::testing::run(|options| async move { Subject { .. } })` (feature
`testing`) is the conformance suite every store passes: set/get/delete, copied
values, binary and empty values, overwrite, expiry, default ttl, invalid keys,
values and ttls, exact keys, fetch (computes once, collapses 24 concurrent
misses, does not cache errors, survives a cancelled leader), the entry bound and
pruning.
