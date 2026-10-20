# ADR 0028: Application cache

- Status: Accepted
- Date: 2026-10-19
- Refines: [ADR 0021](0021-selectable-database-backends.md) (store per backend)
- Ported from: Forge-Go ADR 0032 (same contracts and negative tests)

## Context

Rails 8 ships Solid Cache: the application cache lives in the database the
application already operates, trading a little latency for capacity, durability
across restarts and no extra moving part. Forge applications need the same
(fragments, query results, remote API responses, expensive computations) on
every database configuration, plus an in-process option for single instances
and tests. `forge-cache` is already named in the workspace map ("cache port and
adapters").

Caches are also a place where security bugs hide: values written under keys an
attacker influences (poisoning), unbounded growth, secrets in keys that end up
in logs or tables, thundering herds on a cold key, and tenant data shared
because a key forgot the tenant.

## Decision

### Crate and contract

`forge-cache` holds the port and its adapters behind features, like
`forge-ratelimit` (ADR 0027): `Cache` (`get`, `set`, `delete`, `flight`),
`Pruner`, `Options`, `MemoryCache`, `PostgresCache`, `MySqlCache`,
`SqliteCache`, the free functions `fetch`, `fetch_json`, `get_json`,
`set_json`, `key` and `tenant_key`, and a `testing` conformance suite. The
trait is object safe (`Arc<dyn Cache>`); read-through is a free function
because it is generic over the compute closure.

Values are bytes; JSON helpers give typed access. A value that no longer
decodes (the type changed in a deploy) is a **miss**, so a stale entry is
recomputed, never served or turned into an error. A miss is `Ok(None)`. `ttl`
is `Option<Duration>`: `None` is the store's default (one hour), zero or more
than a year is `InvalidTtl`, and there is no "never expires": everything is
bounded in time as well as in size.

### Stampede protection

`fetch` runs `compute` once per key per process through `Flight`: the first
caller computes on its own future, later callers wait on a `tokio::sync::watch`
channel. No task is spawned. If the first caller is dropped (cancelled) or its
computation panics, the channel closes without a result and **one waiter takes
over with its own closure**: waiters never inherit a leader's cancellation.
An error from `compute` is shared with the callers that waited for it, wrapped
in `FetchError::Compute`, and never cached. Every caller receives its own copy
of the bytes. Replicas do not coordinate: with `N` replicas a cold key can be
computed up to `N` times; a distributed lock would add failure modes (expiry,
owner death) for a benefit that rarely matters, and an advisory lock around
`fetch` is available to applications that need it.

A failing cache never fails `fetch`: a read error degrades to a miss and a
write error is logged (without key or value) while the computed value is
returned. Errors that are the caller's bug (`InvalidKey`, `InvalidTtl`,
`ValueTooLarge`) are returned.

### Bounds

| Bound | Memory | Database stores |
| --- | --- | --- |
| Entries (`FORGE_CACHE_MAX_ENTRIES`, default 10,000) | every write, LRU | when pruning: the **oldest writes** go first (FIFO, like Solid Cache, which does not track reads) |
| Bytes (`FORGE_CACHE_MAX_BYTES`, default 64 MiB) | keys, values and a per-entry overhead | `max_entries x max_value_bytes` |
| Value (`FORGE_CACHE_MAX_VALUE_BYTES`, default 1 MiB, at most 8 MiB) | yes | yes, and a `CHECK` in the schema |
| Key | 1 KiB, no NUL | same |
| Age | per-entry ttl | per-entry ttl on the database clock |

`MemoryCache` starts no task: expired entries are dropped when read and four
cold entries are checked on every write. The database stores prune with
`Pruner::prune`: up to `MAX_PRUNE_ROUNDS` (20) batches of `prune_batch`
(default 500) expired entries, then, if more than `max_entries` remain, up to
as many batches of the oldest. They also prune inline after a successful
write, **after every `prune_batch` writes or every `prune_interval` (default
one minute), whichever comes first**, ignoring errors (`forge_db::PruneGate`).
A time-only trigger was rejected: eviction capped at one batch per interval let
a stream of new keys grow the table to thousands of rows over `max_entries`.
The database bound is *soft but tight*: the table stays within about
`prune_batch` entries per writing process (plus writes in flight) of
`max_entries`. Each prune counts the table, a scan; with millions of rows,
raise `prune_batch`.

### Keys are hashed; keys and values are never logged

Database rows are addressed by `SHA-256(key)` (32 bytes). Keys often name users,
sessions or e-mail addresses; they are never stored, indexed or logged, and
errors never contain them. Keys cannot be listed or deleted by pattern; use a
namespace and let ttl and eviction clean up (the Solid Cache trade-off).

`key(namespace, parts)` builds keys unambiguously: the namespace and every
part are length-prefixed, so neither a user-controlled part nor a dynamic
namespace can reach into another namespace.

### Tenancy

The cache is not tenant-aware, so tenant data must carry the tenant in its key.
Because Forge makes tenant context explicit and fail-closed, `tenant_key`
takes a `&TenantContext` (never a bare id): the key is derived from the
*authorized* tenant, so a handler cannot forget it or pass another tenant's id.
This is the only reason `forge-cache` depends on `forge-tenancy`; the namespace
`forge.tenant` is reserved for these keys.

### Time

PostgreSQL and MySQL/MariaDB compute expiry on the database clock, so replica
clock skew cannot extend or shorten a ttl. SQLite uses the application clock
(one host).

### Migrations

`forge_cache_entries` is version `20261019000100` per backend with `down`
scripts, merged into the application's migrator with
`forge_db::combine_migrations` (see ADR 0027).

## Consequences

- A bounded, shared, restart-surviving cache with no new infrastructure, and an
  in-memory cache for one instance.
- Nothing in the cache is authoritative: callers must treat a miss, a stale
  decode and a store outage the same way.
- Large objects belong in `forge-storage`, not the cache (8 MiB ceiling).
- A cache is only as trustworthy as its writers: whoever can write the table can
  poison every replica. The table is application-privileged like any other.
- No tag/pattern invalidation and no hit statistics; both would need read
  tracking or key storage, which this design deliberately avoids.
- Differences from Forge-Go: cancellation is future drop, so "leader cancelled"
  is a dropped future (tested by aborting the leader task); a panicking leader
  releases waiters who then compute for themselves instead of receiving a
  synthetic error; `FORGE_CACHE_DEFAULT_TTL_SECS` is in seconds (the Rust
  configuration convention); `tenant_key` is Rust-specific.
