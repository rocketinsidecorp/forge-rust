# Rate limiting

`forge-ratelimit` is Rails' `rate_limit` with safe defaults: a fixed-window
limiter, HTTP middleware that answers `429` with `Retry-After`, a bounded
in-memory limiter, and shared limiters for PostgreSQL, MySQL/MariaDB and
SQLite. The design is in [ADR 0027](adr/0027-rate-limiting.md).

## Enable it

```toml
# Cargo.toml of the application (the facade re-exports the crate as
# `forge::ratelimit`)
forge = { version = "0.1", features = ["ratelimit"] }
```

## Protect a route

Rails:

```ruby
rate_limit to: 10, within: 3.minutes, only: :create
```

Forge:

```rust
use std::{sync::Arc, time::Duration};
use forge::ratelimit::{Limit, MemoryLimiter, MemoryOptions, RateLimit, by_remote_addr};

let limiter = Arc::new(MemoryLimiter::new(MemoryOptions::from_config(&config.rate_limit))?);
let create = RateLimit::new(
    limiter,
    "comments.create",                              // scope: what the limit protects
    Limit::new(10, Duration::from_secs(180))?,      // 10 requests per 3 minutes
    by_remote_addr,
)?;

// one route:
router.route(Method::POST, "/comments", create.clone().wrap(comments_create))?;
// or the whole application, counting only POSTs:
router.layer(create.only_methods([Method::POST]));
```

Over the limit the client gets `429` and `Retry-After: <seconds>`:

```json
{"error":{"code":"rate_limited","message":"too many requests","request_id":"..."}}
```

Browsers (`Accept: text/html`) get a minimal static HTML page instead; both
carry `Cache-Control: no-store`. Place the layer **after** authentication when
it keys by principal, and before expensive work (password hashing, mail,
uploads): requests are counted before the handler runs, whatever its outcome.

### Scopes

The counter of a request is identified by (scope, window length, client key).
`forge-http` layers run before routing, so the scope is always explicit: give
two routes two scopes to count them separately, or the same scope to share one
counter on purpose.

### When the limiter fails

If the limiter returns an error (the database is down), the middleware fails
**closed** with `503 rate_limit_unavailable`; an attacker who can make the
limiter fail must not become unlimited. `RateLimit::fail_open()` lets requests
through (and logs a warning) for routes where availability matters more.

## Who is the client? Proxy caveats

The key function (`Fn(&Request) -> String`) result is hashed before it reaches
a store (no addresses or identifiers at rest).

`by_remote_addr` uses the **TCP peer address** only. IPv4 addresses are used
whole; IPv6 addresses are reduced to their `/64`, because one subscriber
controls the whole prefix and could otherwise rotate addresses at will. It
**never reads `X-Forwarded-For`, `X-Real-IP` or `Forwarded`**: those headers
are client-controlled unless a proxy overwrites them, and Forge has no
trusted-proxy policy yet.

| Deployment | `by_remote_addr` | What to do |
| --- | --- | --- |
| Direct (TLS in the application) | the client | nothing |
| Behind a proxy / load balancer | **the proxy**: every client shares one bucket | key by an authenticated value, or by a header your proxy sets |

Key by something the application has authenticated whenever possible:

```rust
let by_principal = |request: &Request| match request.extensions().get::<Principal>() {
    Some(principal) => forge::ratelimit::join(&[&tenant_of(request), &principal.id().to_string()]),
    None => String::new(), // anonymous clients share one bucket: limited, never exempt
};
```

If you must key by client address behind a proxy, write the key function
yourself, read the single header your proxy **overwrites** (not appends), and
only do so when the proxy is the only network path to the application
(firewall the container port). A key function that trusts a header the client
can set lets one client use unlimited keys and bypass the limit entirely.

## Stores

| Store | Type | Shared across replicas | Notes |
| --- | --- | --- | --- |
| Memory | `MemoryLimiter` | no | bounded by `max_keys`, no tasks; the limit is multiplied by the number of replicas |
| PostgreSQL | `postgres::PostgresLimiter` | yes | one atomic upsert per hit, database clock |
| MySQL, MariaDB | `mysql::MySqlLimiter` | yes | one short transaction per hit, database clock |
| SQLite | `sqlite::SqliteLimiter` | processes of one host | application clock |

Shared limiter setup (PostgreSQL shown; enable the `forge` feature of the
backend and use `mysql::`/`sqlite::` otherwise):

```rust
// Apply the table with the application's own migrations:
let migrations = forge::db::combine_migrations(&[&APP_MIGRATIONS, &forge::ratelimit::postgres::MIGRATIONS])?;
App::new("shop").postgres(migrations) /* ... */;

// At startup, after connecting:
let limiter = Arc::new(PostgresLimiter::new(db.clone(), StoreOptions::default())?);
```

`cargo run -- migrate` then creates `forge_rate_limits`. `cargo run -- rollback`
never drops it: the application rollback reverts only the application's own
migrations, and turning the shared limiter off later does not break `migrate`
([ADR 0030](adr/0030-framework-migration-ownership.md)). Dropping the table is
explicit: `database.rollback_framework(&migrations)`.
A new store should pass `forge_ratelimit::testing::run` (feature `testing`).

### Memory limits

`FORGE_RATE_LIMIT_MAX_KEYS` (default `100000`) bounds the in-memory buckets.
When it is full of live buckets, `WhenFull::EvictSoonest` (default) drops the
bucket closest to expiring so new clients are never refused;
`WhenFull::Deny` refuses new keys until space frees (no counter is lost, but a
flood of distinct keys locks out new clients). Choose `Deny` where a bypass is
worse than an outage.

**Never key an `EvictSoonest` limiter on a value the attacker can choose per
target** (the username of a login or reset attempt, an e-mail address, a
resource ID in the URL). Flooding the limiter with more than `max_keys`
distinct junk keys evicts the victim's counter and resets it, which defeats a
per-target limit. Use `Deny`, or better a database limiter (no key cap,
expired rows pruned), for such keys, and combine them with a per-client key.

### Pruning the database tables

Expired rows are removed in bounded batches (`StoreOptions::with_prune_batch`,
default 500; one `prune` call runs at most `MAX_PRUNE_ROUNDS` = 20 batches):
inline after every `prune_batch` hits or every `prune_interval` (default one
minute), whichever comes first, once the hit has been decided, so a stream of
new keys cannot pile up expired rows; and by `Pruner::prune`, which you can
call from a scheduled job. `with_prune_interval(None)` disables the inline
prune. No task is ever started. Live rows are bounded only by the distinct
keys seen within one window.

## Fixed windows: what to expect

The window of a key starts with its first hit and lasts `per`. A client that
spends its allowance at the end of one window and again at the start of the
next gets up to `2N` requests in `per`. For a hard ceiling, lower `N`. Denied
requests count too, so hammering a limited route does not shorten the wait.

## Testing

`forge_ratelimit::testing::run(|| async { Subject { .. } })` is the conformance
suite every store passes (limit and remaining, independent keys, window reset,
retry-after, 160 concurrent hits allowing exactly the limit, invalid keys,
pruning only expired buckets). `MemoryLimiter` takes no clock in its public
API; use short windows in tests.
