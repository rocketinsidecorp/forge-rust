# ADR 0027: Rate limiting

- Status: Accepted
- Date: 2026-10-19
- Refined by: [ADR 0030](0030-framework-migration-ownership.md) (migration ownership and rollback scope)
- Refines: [ADR 0021](0021-selectable-database-backends.md) (store per backend)
- Ported from: Forge-Go ADR 0031 (same contracts and negative tests)

## Context

Rails 8's `rate_limit to: 10, within: 3.minutes, only: :create` protects a route
with one line. Forge needs the same for every route that is not password login
(comment forms, password-reset requests, API endpoints, uploads), on every
supported database and across replicas, and it has to be honest about the two
ways rate limiting fails in practice: the store grows without bound under a
flood of distinct keys, and a client's "identity" is whatever the application
believed, which behind a reverse proxy is usually spoofable. Forge has no
trusted-proxy policy, so the default must not pretend to have one.

## Decision

### Crate

`forge-ratelimit` is a new crate. The workspace map in
[architecture](../architecture.md) names no rate limiter; it is not part of
`forge-security` (authorization contracts, no HTTP or database), `forge-http`
(transport) or `forge-db` (connections): it has an HTTP consumer
(`RateLimit`), database stores and its own dependency boundary (`sha2`,
`sqlx` only for the backend features). Splitting it satisfies the map's rule:
stable responsibility, independent feature boundary, an actual consumer. The
facade exposes it as `forge::ratelimit` behind the `ratelimit` feature.

| Item | Role |
| --- | --- |
| `Limit`, `Decision`, `Limiter`, `Pruner`, `key`, `check_key` | the contract; `Limit::new` validates (`N` in `1..=2^30`, window `1ms..=30d`) so no store sees an invalid limit |
| `MemoryLimiter` | bounded, process-local, no background task |
| `PostgresLimiter`, `MySqlLimiter`, `SqliteLimiter` | shared stores (features `postgres`, `mysql`, `sqlite`) with embedded reversible migrations |
| `RateLimit`, `by_remote_addr`, `join` | HTTP middleware, handler wrapper and key helpers |
| `testing` (feature) | the conformance suite every `Limiter` passes |

The crate sits in the adapter/infrastructure ring: applications' domain and
application code never import it. Application ports that need throttling
define their own narrow trait and bootstrap wires a `RateLimit`.

### Fixed windows

`Limiter::allow(key, limit)` counts one hit. The window of a key starts with
its first hit and lasts `limit.per()`; at most `limit.n()` hits are allowed in
it. Every call counts as a hit, denied or not, so a client cannot probe for
free. A fixed window is one atomic statement on every database
(`INSERT ... ON CONFLICT` / `ON DUPLICATE KEY UPDATE`, the reset decided on the
database clock), has no read-modify-write race and is what Rails does. Its
known cost is the boundary burst: a client aligned to a window edge gets up to
`2N` in `per`. A sliding window would need two rows or a log per key and SQL
that is hard to make atomic on MySQL; the burst is accepted and documented.

### Keys never contain identifiers

`key(scope, parts)` is SHA-256 over `"forge.ratelimit.v1"` and the
length-prefixed (8-byte big-endian) scope and parts, in hex: 64 ASCII
characters. The middleware passes the scope, the window length and the key
function's result through it, so stores see fixed-length, opaque keys: no
addresses or e-mail addresses at rest, no delimiter confusion, and the same
client under two windows does not share a counter. Columns are ASCII, at most
128 characters, with a `CHECK`.

### Memory limiter: bounded, no background work

`MemoryLimiter` holds at most `max_keys` buckets
(`FORGE_RATE_LIMIT_MAX_KEYS`, default 100,000). Buckets are also ordered by
window end in a `BTreeMap`; expired buckets are removed during later calls, so
expiry costs `O(log n)` and there is **no sweeper task** to own, stop or leak
(ADR 0006). When full of live buckets, `EvictSoonest` (default) drops the
bucket closest to expiring (new clients are never refused; an attacker needs
more than `max_keys` distinct keys in one window to push a victim's counter
out) and `Deny` refuses new keys until space frees (no counter is lost, but a
flood of distinct keys locks out new clients). Counters saturate at `N + 1`.

### Shared stores

One row per live bucket in `forge_rate_limits (bucket, hits, expires_at)`:

- PostgreSQL: one `INSERT ... ON CONFLICT DO UPDATE ... RETURNING` on the
  server clock.
- MySQL/MariaDB: `INSERT ... ON DUPLICATE KEY UPDATE`, then a read of the same
  row **in the same transaction** (the upsert holds the row lock until commit,
  so the count read is the caller's own, never a later hit's), `UTC_TIMESTAMP(6)`
  as the clock, deadlock retries (3 attempts).
- SQLite: one upsert with `RETURNING` under the single-writer lock; the
  application clock is the only clock (SQLite serves one host).

Expired rows are removed by `Pruner::prune`: at most `MAX_PRUNE_ROUNDS` (20)
batches of `prune_batch` (default 500) rows per call, `FOR UPDATE SKIP LOCKED`
on PostgreSQL so replicas do not block each other. `allow` also prunes inline,
**after every `prune_batch` hits or every `prune_interval` (default one minute),
whichever comes first**, after the hit has been decided and ignoring errors
(`forge_db::PruneGate`: two atomics, no lock, shared with the cache). A time-only trigger was rejected: it let a
stream of distinct keys pile up expired rows between prunes. No task is ever
started; a scheduler can call `prune` for an explicit cadence. Live buckets
are bounded by the distinct keys seen within one window; a database limiter has
no key cap, which is why it is the right store for attacker-choosable keys.

Migrations are versions `20261019000000` per backend with `down` scripts. They
share the application's `_sqlx_migrations` table, so applications merge them
with their own through `forge_db::combine_migrations`, which refuses duplicate
versions.

### HTTP

`RateLimit::new(limiter, scope, limit, key_fn)` is a `forge_http::Middleware`
(`Router::layer`, the whole application) and `RateLimit::wrap(handler)` wraps
one route (Rails' `only:`); `only_methods` counts only some methods. Over the
limit: `429`, `Retry-After` (whole seconds, rounded up), `Cache-Control:
no-store`, a JSON error (`code: rate_limited`, request id) or, when `Accept`
prefers HTML, a minimal static page.

- **Fail closed.** A limiter error answers `503 rate_limit_unavailable`; the
  cause is logged, never sent. `fail_open()` is an explicit per-route opt-out
  that logs a warning.
- **Scope is explicit.** `forge-http` layers run before routing, so the matched
  route pattern is unknown there; unlike Forge-Go's route-pattern default, the
  scope is a required argument. Reusing a scope shares a counter on purpose.
- **Empty key.** A key function that returns `""` is not exempt: all such
  requests share one bucket.

### Client identity is the application's decision

`by_remote_addr` is the only built-in: the TCP peer address (IPv4 whole,
IPv4-mapped IPv6 unmapped, IPv6 reduced to its `/64`, because one subscriber
controls a whole `/64`), **never** `X-Forwarded-For`, `X-Real-IP` or
`Forwarded`. Behind a reverse proxy the peer is the proxy and every client
shares one bucket: loud and safe, but useless as a per-client limit. The
documented options are keying by something the application authenticated
(`join(&[tenant, principal])`) or reading the one header the deployment's
proxy overwrites, only when the proxy is the sole network path. A first-party
trusted-proxy policy is a separate item.

## Consequences

- One line protects a route, on any backend, with bounded memory and no
  background tasks.
- The login throttle of ADR 0014/0013 is unaffected; new code uses this crate.
- The boundary burst and the shared-store round trip are accepted and
  documented.
- Behind a proxy the default key is useless until the application chooses a
  better one; the framework does not guess.
- Tenancy: a limit is not tenant-sensitive data, but keys that must not mix
  tenants include the tenant in the key function.
- MySQL/MariaDB were verified on MariaDB 10.11 in this change; the supported
  floor in ADR 0021 is MariaDB 11.4+/MySQL 8.4+ and CI should run it there.
- Differences from Forge-Go: cancellation is future drop (no `context`);
  explicit scope; configuration uses `_SECS` keys for durations (none here).
