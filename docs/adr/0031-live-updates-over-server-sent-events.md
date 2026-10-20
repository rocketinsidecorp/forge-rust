# ADR 0031: Live updates: Turbo Streams over Server-Sent Events

- Status: Accepted
- Date: 2026-10-20
- Refines: [ADR 0022](0022-server-rendered-views.md) (Turbo), [ADR 0024](0024-scaffold-generator.md) (scaffold)
- Parity: Forge-Go ADR 0029 ("Broadcasts over Server-Sent Events", "PostgreSQL LISTEN/NOTIFY")

## Context

Server-rendered pages (ADR 0022) answer form submissions with redirects or
Turbo Streams, but nothing pushed a change to the other pages that show it.
Rails answers with `turbo_stream_from` and Action Cable broadcasts. Forge-Go
ships the same feature over Server-Sent Events, with signed channel names,
fail-closed tenant channels, bounded brokers and PostgreSQL `LISTEN/NOTIFY`
fanout; the two frameworks must expose the same commands, names and
configuration (owner decision, 2026-10-20).

`forge-http` buffered whole responses and had no write timeout, so a
long-lived response was impossible and a client that stopped reading could
hold a connection slot forever. The process shutdown signal existed only
inside `serve`, after composition, and workers are told to stop after the
HTTP server drained, which is too late for open streams.

## Decision

### Transport

Server-Sent Events, not WebSockets, for Forge-Go's reasons: Turbo supports
them natively (`<turbo-stream-source>`), they are plain HTTP through every
proxy and middleware layer, need no upgrade handshake or origin protocol, and
the `turbo_stream_from` case is server to client only.

`forge-http` gains `Response::stream(status, state, step)`: an unfold body
whose chunks are written with chunked transfer encoding by the connection's
own task (no spawned task), and `Response::next_chunk` for in-process
readers. Every connection's socket is wrapped in a stalled-write guard: a
write that makes **no progress** for the request timeout
(`FORGE_REQUEST_TIMEOUT_SECS`), but never less than 30 seconds, fails and ends
the connection. Progress resets the clock, so slow but live clients are
unaffected, and the 30 second floor keeps an application that lowers its
request timeout (a fast JSON API, say) from cutting off downloads and streams
to clients on slow links; this replaces Forge-Go's per-write deadline. Forge-Rust serves
HTTP/1.1; the event stream sets no hop-by-hop header, so an HTTP/2 proxy in
front passes it unchanged.

### Crate `forge-broadcast` (facade feature `broadcast`)

| Item | Role |
| ---- | ---- |
| `Channel` | `global(name)` or `for_tenant(tenant, name)`; name `[a-z0-9][a-z0-9_.:-]{0,95}`; key `global/NAME` or `TENANT/NAME` |
| `Signer` | HMAC-SHA256 signed, expiring channel names; `turbo_stream_from` renders the source element |
| `MemoryBroker` | in-process fanout with bounded queues |
| `postgres::PgBroker` | `LISTEN/NOTIFY` fanout across replicas (feature `postgres`) |
| `EventStreams` | the `GET /streams?stream=TOKEN` endpoint |
| `StreamAccess` | the application's decision about who may read a channel |
| `publish` | renders `TurboStreamAction`s and publishes them |

It depends on `forge-http`, `forge-views`, `forge-tenancy`, `forge-security`,
`forge-config` and the RustCrypto `hmac`/`sha2` and `subtle` crates already in
the workspace. No new third-party dependency.

### Signed stream names

The token format is Forge-Go's byte for byte (a test pins tokens Forge-Go
printed): `v1.<base64url(JSON {"t","n","e"})>.<base64url(HMAC-SHA256)>`, the
MAC over the label `forge-broadcast-stream-v1.` and `v1.<claims>`. It is
verified in constant time (`subtle`) against every configured key (rotation),
expires (24 hours by default, 7 days at most), and is at most 512 bytes. A
forged, tampered or expired token gets the same `403 stream_invalid`.

`FORGE_STREAMS_KEY` has Forge-Go's name and format: base64 (standard or
URL-safe, padded or not) of at least 32 bytes, as `openssl rand -base64 32`
prints. `forge-config` validates it and never echoes it; it is optional at
load time because only applications that serve streams need it.
`Signer::from_config` refuses to start in `production` without it and uses a
random per-process key (with a warning) in development and test. Forge-Rust
has no `staging` environment; Forge-Go also requires the key there.

### Tenancy and authorization, fail closed

A token is a bearer capability for one channel, not an identity. After it
verifies, the application's `StreamAccess` authenticates the caller and
returns a `StreamViewer`. A tenant channel is served only to
`StreamViewer::member(&TenantContext)` of the same tenant; a `TenantContext`
cannot be built without an authenticated principal, an active membership and
a policy grant. Global channels accept principals and, only when the
application says so, anonymous viewers. Denials are `401` or
`403 stream_forbidden` without saying which check failed.

Generated applications reuse the resource's own access policy: a scaffold
registers its channel with its `TenantAccess`/`GlobalAccess` and read
permission; channels no resource registered are refused. `GlobalAccess`
returns the principal it admitted (`Option<PrincipalId>`), and the generated
reader serves global channels to `StreamViewer::principal` when there is one,
so signed-in viewers are limited per person, never by the address they share
behind a proxy; only anonymous viewers of public resources are counted by
address. (It returned `()` before; the scaffold refuses an older access
module with the change to make.) Because tenant
access in Forge-Rust is derived from the route (`/tenants/{tenant_id}/...`),
tenant channels are served at `/tenants/{tenant_id}/streams` and global ones
at `/streams` (Forge-Go: one `/streams` with the tenant in the request
context). A token for tenant A presented under tenant B's path is refused.

### Limits and backpressure

- `MemoryBroker` starts no task. Publishing does a non-blocking `try_send` to
  each subscriber's bounded queue (64 messages); a subscriber whose queue is
  full is **dropped** (`SlowConsumer`): its stream ends after the queued
  messages and the browser reconnects. Publishers never wait.
- 10 000 subscribers per process, 1 000 per channel, 1 MiB per message.
- Per account: 16 open streams per principal (anonymous viewers by peer
  address, IPv4-mapped IPv6 as IPv4, IPv6 by `/64`) and 1 000 per tenant:
  `429 stream_limit` with `Retry-After: 5`. A full broker is
  `503 stream_unavailable`. The client-address function is replaceable
  (`EventStreams::client_address`) so a trusted-proxy resolver can be used
  once Forge-Rust has one (Forge-Go ADR 0037); forwarded headers are never
  read today.
- Events are `retry: 3000`, `: connected`, `: ping` every 20 seconds, and
  `data:` lines: payloads are split on `\n`, `\r\n` and `\r`, so they cannot
  end an event or inject fields. The SSE framing of a message is computed
  once and shared by every subscriber.
- Streams end after 30 minutes; the browser reconnects, which re-runs every
  check, so a revoked session or permission stops receiving.
- `Cache-Control: no-store`, `X-Accel-Buffering: no`.
- Delivery is at most once; there is no replay (Last-Event-ID would need a
  retained, tenant-partitioned log). Pages re-render on navigation.

### Lifecycle

`App::serve` creates the shutdown signal before composition.
`Composition::shutdown_started` hands adapters a child of it;
`Composition::broadcast(Fanout::InProcess | Fanout::Postgres)` builds the
broker on it. When shutdown begins the broker closes, every open stream ends
and the HTTP server drains at once instead of waiting for the grace period
(a test runs a real server and proves it).

### PostgreSQL fanout

`PgBroker` wraps a `MemoryBroker`: publishing delivers locally at once and
sends `NOTIFY` on the `forge_broadcast` channel (a validated lowercase
identifier); each broker skips notifications carrying its own origin id.
The envelope is Forge-Go's: chunks of 4 500 bytes base64 encoded
(`v1 ORIGIN ID INDEX COUNT KEY DATA`, under PostgreSQL's 8 000 byte limit),
sent in one transaction, reassembled with bounded buffers (64 partial
messages, the 256 KiB message limit, 30 seconds). `publish_in` queues a
message in the caller's transaction (delivered everywhere only on commit).
The listener is a future (`PgBroker::listen`) that `Composition::broadcast`
registers as a lifecycle worker; it holds one pool connection through
`sqlx::PgListener`, reconnects with 1-30 second backoff and logs the gap,
ignores malformed notifications, and returns when the broker closes.

`NOTIFY` is database-wide and not tenant-aware: the channel key, which
carries the tenant, separates tenants exactly as in memory, and whoever can
`NOTIFY` on the database can publish (the database role is the trust
boundary). MySQL, MariaDB and SQLite have no `LISTEN/NOTIFY`:
`Fanout::Postgres` is refused there with `UnsupportedBackend` rather than
emulated (CLAUDE.md, ADR 0021), and those applications use the in-process
broker, which reaches one process only.

### Scaffold

`forge generate scaffold ... --broadcast` is opt-in, as in Forge-Go. The
pages publish after the change is stored (create appends the row and removes
the empty-list row, because Forge-Rust lists are oldest first; update
replaces the row; delete removes it); a publish failure is logged and never
fails the request. On PostgreSQL publishing needs a pool connection for
`NOTIFY`, so under pool pressure it could outlast the request timeout and
turn a stored write into a `503`: the generated `Streams::publish` therefore
waits at most `PUBLISH_TIMEOUT` (2 seconds), then logs `live_update_failed`
and returns (this replica's pages already received the update; other
replicas see it on their next page load). Publishing with `publish_in` inside
the write's own transaction would be atomic, but the generated repositories
own their transactions behind application ports that expose no SQLx type to
the HTTP adapter, and MySQL, MariaDB and SQLite have no `NOTIFY`;
hand-written PostgreSQL code that holds the transaction should prefer
`publish_in`. Fragments are rendered from a `_row.html` partial without
a request, so no CSRF token or per-user data enters a broadcast. The first
broadcasting scaffold writes `src/adapters/http/streams.rs`; the printed
wiring uses `Fanout::Postgres` on PostgreSQL and `Fanout::InProcess`
elsewhere. `forge new` enables the `broadcast` feature for database HTML
applications; older applications are refused with instructions. The plural
`streams` is reserved.

### Accepted residuals and capacity edges

Recorded after the independent review of the pull request:

- **`sqlx::PgListener` internals.** The listener buffers notifications in an
  unbounded `futures` mpsc channel, and its `Drop` spawns a detached task
  that runs `UNLISTEN *` and returns the connection to the pool. Both are
  third-party code that Forge's own rule against unbounded queues and
  detached tasks cannot reach without replacing the driver; they are
  accepted. The buffer only takes notifications that arrive while the
  listener's connection executes a statement, and `PgBroker` issues none
  after `LISTEN` (it only awaits `try_recv`, which reads the socket
  directly), so it holds at most what arrives during one `LISTEN` round
  trip. The detached task runs one statement on a connection the pool
  already counts, and ends with it or with the runtime. The listener's
  whole lifetime (connect, reconnect with backoff, drop) belongs to the
  `PgBroker::listen` lifecycle worker that `Composition::broadcast`
  registers, which returns when shutdown begins. Revisit if sqlx offers a
  bounded listener.
- **Detecting vanished clients.** A browser that disappears without closing
  its connection (network loss, a suspended laptop) is noticed only when a
  write fails. The 20 second heartbeat guarantees a write at least that
  often, but the kernel accepts writes until its buffer is full, so the
  stream ends when TCP gives up retransmitting (minutes, operating-system
  dependent), when the stalled-write guard trips (at least 30 seconds
  without progress once the buffer is full), or at the 30 minute maximum
  age, whichever comes first. Until then it holds a connection slot and one
  of its principal's 16 streams; if the same person opens pages meanwhile
  and reaches 16, the new page gets `429 stream_limit`, on which
  `EventSource` gives up, and updates live again once reloaded.
- **Per-process limits.** Every limit (10 000 subscribers, 1 000 per channel,
  1 000 per tenant, 16 per principal) is counted per process: with N
  replicas a tenant may hold up to N x 1 000 streams, and ten tenants at
  their limit fill one process. Open streams also hold connection slots
  shared with ordinary requests, so size `FORGE_MAX_CONNECTIONS` for both.
  A shared, cross-replica counter is not provided.
- **Browser connection limit.** Over HTTP/1.1 browsers open at most 6
  connections per origin, and each open page holds one for its stream:
  with five or more tabs of the application open, further page loads in that
  browser wait. Forge-Rust serves HTTP/1.1, so terminate HTTP/2 (or HTTP/3)
  at the reverse proxy in front of it, which multiplexes every stream of a
  browser over one connection; the stream response sets no hop-by-hop
  header and passes through unchanged.
- **Key rotation.** `Signer::new(key, previous)` verifies tokens signed with
  previous keys, but generated applications build the signer from
  `FORGE_STREAMS_KEY` alone, like Forge-Go's generated `htmlstreams.NewSigner`
  (whose library also has `SignerOptions.PreviousKeys`). Changing the key
  makes every open page's token invalid: its next reconnect gets `403` and
  `EventSource` stops, so the page updates live again once reloaded. The
  procedure is in [deployment](../deployment.md#rotating-the-stream-signing-key).
  A `FORGE_STREAMS_PREVIOUS_KEYS` setting is a parity follow-up for both
  frameworks ([parity](../rails-parity.md#parity-follow-ups)), not added
  here alone.

## Consequences

- Open pages show other people's changes without polling, across replicas on
  PostgreSQL, with tenant isolation enforced by the channel key, the signed
  name, the route's authorized tenant and the handler's own check.
- Each open stream holds one connection slot (`FORGE_MAX_CONNECTIONS`) until
  it ends; capacity is bounded by the subscriber and per-account limits.
  Behind a proxy, streams need buffering disabled and an idle timeout above
  the heartbeat.
- Stream URLs carry the token and therefore appear in access logs for its
  lifetime; it grants reading one channel to an otherwise authorized caller.
- Not provided: WebSockets, replay of missed messages, presence, per-user
  channels, a `staging` environment, trusted-proxy client addresses.
