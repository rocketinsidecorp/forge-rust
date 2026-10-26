# Benchmarks

Forge-Rust has two kinds of benchmarks, and they answer different questions:

- **Micro-benchmarks** (`cargo bench -p forge-bench`) time one hot path in
  isolation: nanoseconds, heap bytes and heap allocations per operation.
- **The end-to-end benchmark** (`scripts/bench.sh`) builds a generated
  application, runs it on PostgreSQL and measures requests per second and
  latency percentiles of four requests through the whole stack.

The suite has the **same scenario names, variants and output format** as
Forge-Go's ([Rails parity](rails-parity.md)), so the two can be read side by
side. Forge-Go's catalog is in its
[BENCHMARKS.md](https://github.com/rocketinsidecorp/forge-go/blob/main/docs/BENCHMARKS.md);
this document says what each scenario times in Rust, and which two scenarios
have no Rust equivalent yet.

Contents: [Run them](#run-them) · [Scenarios](#scenarios) ·
[Methodology](#methodology) · [Caveats](#caveats) · [Results](#results) ·
[Output format](#output-format-shared-with-forge-go)

## Run them

```sh
# every micro-benchmark: a fixed number of iterations per scenario, the median of 3 runs
cargo bench -p forge-bench

# a subset: the scenarios whose name ("scenario/variant") contains the text
cargo bench -p forge-bench -- router_match
cargo bench -p forge-bench -- middleware_chain http_telemetry

# each scenario once, nothing measured (what CI and `cargo test` do)
cargo bench -p forge-bench -- --quick

# end to end: needs cargo, psql, openssl and a PostgreSQL server whose user may
# create roles and databases. The URL is psql's argument, which ps shows: on a
# shared machine leave the password out of it and use PGPASSWORD or ~/.pgpass.
export FORGE_TEST_DATABASE_URL='postgres://postgres:postgres@127.0.0.1:5432/postgres'
scripts/bench.sh                                  # 16 workers, 10 s per scenario, 3 runs
BENCH_DURATION=3s BENCH_RUNS=1 scripts/bench.sh   # a quick check
BENCH_CONCURRENCY=64 scripts/bench.sh             # another concurrency
scripts/bench.sh -format json                     # arguments after the script go to the load generator

# the load generator alone, against an application you already run
cargo run --release -p forge-loadgen -- -h
```

`scripts/bench.sh` builds the `forge` CLI and the load generator, runs
`forge new --forge-path` against your checkout and
`forge generate scaffold post title:string body:text --tenant-scoped`, applies
the wiring the generator prints (and sets the scaffold's `PAGE_SIZE` to
`BENCH_ROWS`), builds the application in release mode, creates two
non-superuser roles and a database of their own on the server (row-level
security applies, as in production), runs `migrate` and `setup`, starts the
binary, measures it, and then stops it, drops the database and the roles and
deletes everything it built. The first run compiles the CLI and the
application (several minutes); it needs about 1 GB of temporary files (in
`$TMPDIR`) while it runs. The settings are environment variables:
`BENCH_CONCURRENCY` (16), `BENCH_DURATION` (10s), `BENCH_WARMUP` (2s),
`BENCH_RUNS` (3), `BENCH_ROWS` (50) and `BENCH_PORT` (18080), plus
`BENCH_TARGET_DIR`, a Cargo target directory to reuse and keep (the default is
a fresh one that is deleted). `FORGE_TEST_POSTGRES_URL`, Forge-Go's name for
the server URL, is accepted in place of `FORGE_TEST_DATABASE_URL`.

CI runs `cargo bench -p forge-bench --bench micro --locked -- --quick` and then
`scripts/bench.sh` with one-second windows in a non-gating job (`benchmarks`),
so every benchmark keeps compiling and running. It does not compare or store
numbers: shared runners are too noisy for that.

## Scenarios

### Micro-benchmarks

| Scenario | Variants | What is timed in Forge-Rust | Benchmark |
| --- | --- | --- | --- |
| `router_match` | `static`, `param`, `wildcard` x 1, 10, 100, 1000 routes | `Router::dispatch` for a request matching the middle route of the table, to a handler that answers `204`; `static` is `/static/rN/item`, `param` `/param/rN/{id}`, `wildcard` the catch-all `/files/rN/{path...}` requested as `/files/rN/docs/2026/report.txt` | `router_match/<kind>/<routes>` |
| `middleware_chain` | `passthrough`, `context` x 0, 1, 5, 10 layers | the server's request path for one `GET /users/{id}` (telemetry started and finished around `Router::dispatch`, see below) with N layers added by `Router::layer`; `context` layers also insert a value into the request extensions | `middleware_chain/<kind>/<n>` |
| `json_render` | `object`, `list_50` | `Response::json` of one record and of a page of 50 | `json_render/<variant>` |
| `html_render` | `layout_partial`, `layout_partial_csrf` | a page of 50 rows in a layout with two partials, auto-escaping text that contains markup (Askama, the generated applications' template engine); the second variant goes through `CsrfProtection` and `FlashLayer` and a router, with the CSRF meta tag, a form token and flash messages | `html_render/<variant>` |
| `csrf_verify` | `match`, `header_post`, `form_post` | the constant-time comparison of two tokens; a POST through `CsrfProtection` with the token in the `x-csrf-token` header; the same with the token in a form-encoded body | `csrf_verify/<variant>` |
| `session_cookie_verify` | none | what the generated `SessionTenantAccess` does for a GET to a tenant page: cookie parsing, token decode, SHA-256 digest, session lookup, session check, membership and RBAC authorization, giving the `TenantContext`; the stores are in memory | `session_cookie_verify` |
| `password_verify` | none | **Argon2id** (version 19, 19456 KiB, 2 passes, 1 lane, 32-byte output), one correct password | `password_verify` |
| `uuid_v7` | `new`, `string` | generating an identifier (`forge::core::Id`); formatting it | `uuid_v7/<variant>` |
| `i18n_lookup` | `simple`, `interpolation`, `plural_one`, `plural_other`, `fallback` | `Translator::translate`: a plain key; one `%{name}` argument; the `one` and `other` plural forms; a key answered by the parent locale (`pt-BR` to `pt`) | `i18n_lookup/<variant>` |
| `attribute_encryption` | `seal`, `open`, `value_seal`, `value_open` | AES-256-GCM of one 64-byte attribute bound to table, column and record, with `Cipher::encrypt`/`decrypt` and with the `Attribute::seal`/`open` helper a model's column uses | `attribute_encryption/<variant>` |
| `rate_limit_decision` | `allowed`, `denied`, `distinct_keys` | one `MemoryLimiter::allow`; `distinct_keys` inserts a key and evicts another on every call (the worst case) | `rate_limit_decision/<variant>` |
| `http_telemetry` | `baseline`, `disabled`, `enabled_no_export`, `enabled_exporting` | the cost of telemetry on the same request path as `middleware_chain/passthrough/0`, see below | `http_telemetry/<variant>` |

One scenario of Forge-Go's catalog is **not run** because Forge-Rust has
nothing to time (it is not reported, not estimated): `show_json` (end to
end), see [End-to-end scenarios](#end-to-end-scenarios).

Notes on the choices:

- `password_verify` is **deliberately slow**: the work factor is the defense.
  Forge-Rust's password hasher is **Argon2id** (the OWASP minimum profile:
  m=19456 KiB, t=2, p=1), where Forge-Go uses PBKDF2-HMAC-SHA256 with 600000
  iterations. Both are tens of milliseconds, and a memory-hard function costs
  an attacker memory as well as time, but **the two figures are not a
  comparison of the frameworks**: they are different algorithms. It is
  reported so that nobody mistakes its cost for a regression and nobody
  "optimizes" it. Every other figure is smaller by three orders of magnitude or
  more. Its `B/op` is the 19 MiB block memory of Argon2.
- Sessions are opaque server-side tokens, not signed or encrypted cookies, so
  there is no signature to verify; `session_cookie_verify` times what the
  access check does in process. Those functions live in the generated
  application (`src/infrastructure/security`, `src/adapters/http/access.rs`),
  not in a framework crate the benchmark can import, so the benchmark restates
  them step for step with the framework's public types (`SessionBearerToken`,
  `Session::authenticate`, `RbacPolicy`, `TenantContext::authorize`) and
  in-memory `async_trait` stores. The database lookups of a real session store
  are in the end-to-end scenarios (`index_html`, `create_form`).
- `rate_limit_decision` times the in-process limiter through its `Limiter`
  trait, which is `async_trait`: every call boxes its future, one allocation
  Forge-Go's `Memory.Allow` does not make. The PostgreSQL, MySQL and SQLite
  limiters cost one database round trip, which the micro-benchmark would only
  measure as the driver's.
- `middleware_chain` and `http_telemetry`: `forge-http`'s request handler
  (`Server::handle_request`) is private and sits on hyper, so the benchmark
  restates the steps that are not network work: build the request, start the
  request's telemetry (`ServerInstrumentation::start`), `Router::dispatch`
  through the layers and the route table, finish the telemetry with the route
  and status. Parsing, the body limit, the request timeout, panic catching and
  the log span are not included. `middleware_chain` uses telemetry in its
  default mode (enabled, nothing exported), like Forge-Go's chain, so its
  0-layer row is the base cost of a request with telemetry, not of the router
  alone (`router_match` and `http_telemetry/baseline` are the router alone).
- `http_telemetry`: `baseline` is `Router::dispatch` with no telemetry code;
  `disabled` is the server path with `FORGE_TELEMETRY_ENABLED=false`, which
  creates no instrumentation at all, so it is `baseline` and the two match by
  construction (Forge-Go reports both too); `enabled_no_export` is
  instrumented, with spans recorded and sampled and no exporter;
  `enabled_exporting` adds the `log` span sink on a Tokio runtime, so every
  finished span is queued, batched and handed to the exporter task, which
  writes one tracing event per span (no subscriber is installed, so nothing is
  printed). Forge-Go's counterpart hands spans to a discarding exporter; an OTLP
  export over the network is not measured here either. The exporter runs on
  another thread, whose allocations are in `B/op` and `allocs/op` (they are
  process-wide).
- `json_render`, `html_render`, `csrf_verify`, `router_match`: a `Request` is
  moved into `Router::dispatch`, so each iteration builds one from clones of
  prebuilt parts and drops the response; the request is a fixed one (the
  request id is not generated per iteration). That cost is inside the figures.
  The reference rows `request_build/get` and `request_build/post_form`, which
  are **not** part of Forge-Go's catalog and are printed apart, time the
  building alone, so it can be subtracted. The Rust-only reference rows
  `router_match/escaped/<routes>` route `/param/rN/a%20b`, the one request
  shape whose invalid-path check decodes a segment; Forge-Go has no such case.
- `csrf_verify/match`: `forge-views` compares tokens with
  `subtle::ConstantTimeEq` in a private function; the benchmark runs the same
  expression on two `CsrfToken`s.
- `uuid_v7/new`: a UUIDv7 from the clock and a read of the operating system's
  random source for every call (the `uuid` crate does not buffer), which is
  why it is slower than Forge-Go's `uuid.New`. The IDs of a database row are
  generated once per row, outside the request hot path.
- Router times grow only slightly with the number of routes because the table
  is a segment trie with a hash map per node.

### End-to-end scenarios

Each request is sent over a keep-alive connection by a closed loop of a fixed
number of workers. The sign-in happens once, before measuring.

| Scenario | Request | Success |
| --- | --- | --- |
| `health` | `GET /health/ready` (readiness: pings the database) | `200` |
| `welcome` | `GET /` (the generated welcome page, no database) | `200` |
| `index_html` | `GET /posts` with the session and tenant cookies: a scaffold index of 50 rows from PostgreSQL, rendered with layout, i18n and CSRF tags | `200` |
| `show_json` | not run: see below | |
| `create_form` | `POST /posts` with the session and tenant cookies and the CSRF token in the form: validates, inserts one row, redirects | `303` |

The application is a tenant-scoped scaffold, so every request that touches the
table runs the session lookup, the membership and permission check, the tenant
context and PostgreSQL row-level security. A response with another status
counts as an error and its latency is not recorded.

Differences from Forge-Go's scenarios, none of them a hidden change:

- **`show_json` is not run yet.** It is a `GET` of one row as JSON with a
  bearer token. Generated applications can now serve it (`forge new
  --api-auth` and the scaffold's `/v1/<plural>` API,
  [ADR 0045](adr/0045-json-resources-and-bearer-api-authentication.md)), but
  the load generator does not sign in for a token yet: it knows the name and
  refuses it. Once it does, the scenario joins the report with Forge-Go's
  request.
- `health` is `GET /health/ready`. In Forge-Go `/health` is the readiness
  probe that pings the database; in Forge-Rust `/health` and `/health/live`
  never touch the database and `/health/ready` is the probe that does. The
  scenario means "the readiness probe", so it is the same work.
- `index_html` takes the scaffold's page size, not a `limit` parameter (the
  generated query refuses unknown parameters): the script sets the scaffold's
  `PAGE_SIZE` constant to `BENCH_ROWS` (50) in the benchmark application's copy,
  and the load generator checks that the page lists exactly that many rows
  before it measures.
- The sign-in and the form use Forge-Rust's names: the hidden field is
  `csrf_token` (Forge-Go's `authenticity_token`), the scaffold lives under
  `/posts` for the tenant of the session, and the load generator takes the
  tenant's id (`-tenant-id`, which `scripts/bench.sh` reads from the output
  of `setup`) for the tenant cookie when sign-in sets none.
- Forge-Rust separates the migration and runtime database credentials
  ([ADR 0011](adr/0011-postgresql-runtime-migration-roles-and-rls-context.md)), so the rig
  creates two non-superuser roles (the owner that migrates, and a runtime role
  that can only read and write the tables) where Forge-Go's creates one.

The load generator is a **Rust port** of Forge-Go's `internal/loadgen`
(`crates/forge-loadgen`, standard library only), not Forge-Go's program run
with `go run`: that program signs in through Forge-Go's form fields, calls
`/auth/login` for a bearer token and requests `/posts`, none of which a
Forge-Rust application answers. The flags, the environment variable
(`BENCH_PASSWORD`), the closed loop, the interleaved rounds, the percentile
rule and the output are the same; its tests run against a stand-in application
in `cargo test`.

## Methodology

**What is captured.** `scripts/bench.sh` prints, before the table: the
Forge-Rust commit (`+dirty` if the tree has changes), the Rust version, the
kernel, the number of CPUs and the CPU model, the PostgreSQL version and the
UTC date. Add the host description by hand (container, virtual machine,
laptop). `cargo bench -p forge-bench` prints the operating system, the
architecture, the CPU, the toolchain and the profile before its table.

**Warm-up.** Micro-benchmarks: every scenario runs a tenth of its iterations
unmeasured first. The number of iterations is **fixed per scenario** (chosen
so that a run takes a fraction of a second to a few seconds; `password_verify`
runs 5), where Go's `testing` package calibrates the count to about a second.
End to end: every scenario is first driven for `BENCH_WARMUP` (2 s),
unmeasured, so connections, pools and caches are warm.

**Runs.** Micro-benchmarks run three times and the tables report the
**median** of the three (the run with the median time, with its own heap
figures). End to end, each scenario is measured for `BENCH_DURATION` (10 s) in
each of `BENCH_RUNS` (3) rounds; the rounds interleave the scenarios
(health, welcome, index, create, health, ...) so slow drift of the host affects
all of them alike. The table reports the median of each figure over the runs,
and the minimum and maximum requests per second so that the spread is visible.

**Heap figures.** The bench binary installs a counting global allocator
(`forge_bench::alloc`) that forwards to the system allocator and counts
allocation calls and the bytes they request; a `realloc` counts as one
allocation of its new size, frees are not subtracted. `B/op` and `allocs/op`
are those counts divided by the iterations, rounded to the nearest integer, so
an allocation that happens once per run is `0`. The counts are process-wide
(all threads), like Go's. The counting allocator is the only `unsafe` in the
repository; it is isolated, documented and tested, and the dependency policy
records it ([dependency policy](dependency-policy.md#benchmark-dependency-decision)).
Without it the harness prints `n/a`.

**Harness.** `criterion` is not in the dependency graph, so `forge-bench` is a
small harness on stable Rust: a `benches/` target with `harness = false`,
`std::hint::black_box` around the operation under test and its input, a fixed
number of iterations and the median of three runs. Futures are polled on the
benchmark thread with a no-op waker (`Router::dispatch` and the in-memory
stores never suspend), so no scheduler is in the figures.

**Concurrency.** Fixed: 16 workers, one connection each, a closed loop (a
worker sends its next request when the previous one is answered). Latency is
measured by the client from sending the request to reading the whole response.
Percentiles are nearest-rank over every successful request of the run.

**What is measured.** Micro-benchmarks: the Rust function and its heap
allocations, in a release build (`bench` profile: the workspace's thin LTO and
one codegen unit). End to end: the compiled application binary (a release
build, plain HTTP, the logging the application is generated with), the HTTP
server, the whole handler chain, the generated session, authorization and
handlers, the views, the SQLx pool and PostgreSQL itself.

**What is not measured.** TLS and anything a reverse proxy does; a network
(loopback only); sign-in and password hashing (done once, before the clock
starts; see `password_verify`); the observability stack (telemetry is on in its
default mode, nothing is exported); other databases (PostgreSQL only); mail,
jobs, storage and live streams; start-up and memory use; cold caches.

## Caveats

- **One machine.** The load generator, the application and PostgreSQL share the
  same CPUs and the same kernel, so each takes CPU from the others. The
  figures are **throughput of the whole rig**, not of the application alone,
  and the latency includes queueing for CPU.
- **Closed loop.** With a fixed number of workers, throughput and latency are
  tied (requests per second is about 16 divided by the mean latency), and a
  stall delays the requests that would have been sent during it instead of
  being counted against them (coordinated omission). Read the percentiles as
  "latency at this concurrency", not as an open-loop service-level figure.
- **Shared and noisy hosts.** Other work on the machine changes the numbers from
  run to run; see the min-max column and repeat before drawing a conclusion
  from a difference of less than about 10 percent. Compare runs from the same
  host only, and never numbers from different hardware.
- **Loopback and no TLS** make every request cheaper than it is in production.
- **Development settings.** The binary runs with the defaults of
  `FORGE_ENV` (development), because production requires TLS or a trusted
  proxy.
- **`create_form` grows the table** by one row per request (tens of thousands
  per run); `index_html` always lists the newest 50, so its work does not grow.
  PostgreSQL runs with its default settings.
- **`health` is not "no work"**: it pings the database.
- **The runtime role has broader table privileges than the generated compose
  setup grants.** The migrations narrow a few tables (audit events are
  insert-only, for example) for a role that is literally named `app_runtime`;
  the rig's own role names get the schema's default privileges instead (read
  and write on every table). Row-level security, the lack of superuser and
  `BYPASSRLS`, and the tenant predicates are unchanged, so the work per request
  is the same; the difference is in what the role *may* do, not in what the
  benchmark does.
- Micro-benchmark figures are for a prebuilt request and a response that is
  dropped, so they are lower bounds of what a server spends on the same work.
- `middleware_chain` and `http_telemetry` do not include hyper, the body limit,
  the request timeout or the log span, because the server's request handler is
  private (see [Scenarios](#scenarios)). Their ordering and differences are
  meaningful; their absolute values are lower than a served request's.
- The `enabled_exporting` figure includes cross-thread hand-off to the exporter
  task, so it varies more than the others on a busy host.

## Results

Indicative only: see the [caveats](#caveats). Do not compare them with figures from
another machine or another framework.

Measured on 2026-10-20 in the container this was developed in: **a container
on a shared host; indicative only**. Other work (compilation and test runs) shared the
four CPUs during both measurements: the load average was 2.8 when the
micro-benchmarks finished and 6.6 right after the end-to-end run. Treat the
figures as the order of magnitude, and the min-max column as the spread.

| | |
| --- | --- |
| Date | 2026-10-20 |
| CPU | 4 x Intel Xeon Processor @ 2.10 GHz (virtual CPUs; `model name` as the guest reports it) |
| Kernel | Linux 6.18.44 (container) |
| Rust | rustc 1.98.1 (48a229cea 2026-09-01), release / `bench` profile (thin LTO, 1 codegen unit) |
| PostgreSQL | 16.15 (Ubuntu 16.15-0ubuntu0.24.04.1), default settings, on the same host at 127.0.0.1 |
| Forge-Rust | 2ce127e with the benchmark suite of this change in the working tree (`scripts/bench.sh` printed `+dirty`) |

### Micro-benchmarks

`cargo bench -p forge-bench`; the median of three runs. `password_verify` is
deliberately slow (about 18.5 ms: Argon2id, 19456 KiB, 2 passes; its `B/op` is
Argon2's 19 MiB block memory).

| Scenario | Variant | ns/op | B/op | allocs/op |
| --- | --- | ---: | ---: | ---: |
| `router_match` | `static/1` | 443 | 1639 | 3 |
| `router_match` | `static/10` | 441 | 1639 | 3 |
| `router_match` | `static/100` | 478 | 1640 | 3 |
| `router_match` | `static/1000` | 421 | 1641 | 3 |
| `router_match` | `param/1` | 532 | 1922 | 7 |
| `router_match` | `param/10` | 573 | 1922 | 7 |
| `router_match` | `param/100` | 545 | 1923 | 7 |
| `router_match` | `param/1000` | 528 | 1924 | 7 |
| `middleware_chain` | `passthrough/0` | 1662 | 1919 | 7 |
| `middleware_chain` | `passthrough/1` | 1671 | 1919 | 7 |
| `middleware_chain` | `passthrough/5` | 1714 | 1919 | 7 |
| `middleware_chain` | `passthrough/10` | 1876 | 1919 | 7 |
| `middleware_chain` | `context/0` | 1639 | 1919 | 7 |
| `middleware_chain` | `context/1` | 1748 | 2107 | 10 |
| `middleware_chain` | `context/5` | 2175 | 2419 | 15 |
| `middleware_chain` | `context/10` | 2629 | 3003 | 21 |
| `http_telemetry` | `baseline` | 505 | 1919 | 7 |
| `http_telemetry` | `disabled` | 490 | 1919 | 7 |
| `http_telemetry` | `enabled_no_export` | 1587 | 1919 | 7 |
| `http_telemetry` | `enabled_exporting` | 6392 | 2827 | 21 |
| `json_render` | `object` | 1151 | 1176 | 11 |
| `json_render` | `list_50` | 39441 | 38920 | 311 |
| `html_render` | `layout_partial` | 17215 | 30398 | 10 |
| `html_render` | `layout_partial_csrf` | 20303 | 28004 | 30 |
| `csrf_verify` | `match` | 118 | 0 | 0 |
| `csrf_verify` | `header_post` | 1257 | 2706 | 12 |
| `csrf_verify` | `form_post` | 2249 | 4482 | 25 |
| `session_cookie_verify` | `-` | 635 | 180 | 7 |
| `password_verify` | `-` | 18477865 | 19922944 | 1 |
| `uuid_v7` | `new` | 440 | 0 | 0 |
| `uuid_v7` | `string` | 45.9 | 36 | 1 |
| `i18n_lookup` | `simple` | 229 | 78 | 3 |
| `i18n_lookup` | `interpolation` | 281 | 308 | 5 |
| `i18n_lookup` | `plural_one` | 369 | 314 | 5 |
| `i18n_lookup` | `plural_other` | 369 | 315 | 5 |
| `i18n_lookup` | `fallback` | 312 | 93 | 5 |
| `attribute_encryption` | `seal` | 836 | 517 | 6 |
| `attribute_encryption` | `open` | 323 | 257 | 3 |
| `attribute_encryption` | `value_seal` | 875 | 581 | 7 |
| `attribute_encryption` | `value_open` | 361 | 321 | 4 |
| `rate_limit_decision` | `allowed` | 172 | 56 | 1 |
| `rate_limit_decision` | `denied` | 169 | 56 | 1 |
| `rate_limit_decision` | `distinct_keys` | 403 | 341 | 4 |

`router_match/wildcard/*` (the router's catch-all segment) was added after
this run and has no figures here yet: `cargo bench -p forge-bench --
router_match/wildcard` measures it.

Reference rows that are not part of Forge-Go's catalog: the cost of building
the request that the dispatching scenarios above include in their figures.

| Scenario | Variant | ns/op | B/op | allocs/op |
| --- | --- | ---: | ---: | ---: |
| `request_build` | `get` | 88.0 | 0 | 0 |
| `request_build` | `post_form` | 231 | 344 | 2 |

How to read them, from the table only: `router_match` varies by less than the
noise from 1 to 1000 routes (443 ns for `static/1`, 421 ns for
`static/1000`), so the segment trie does not slow down with the table. `middleware_chain/*/0`
(about 1.6 us) already includes telemetry in its default mode; ten layers that
only call the next one add about 0.2 us, which is within the noise of this host,
and ten that insert an extension value add about 1.0 us, 1.1 KB and 14
allocations (about 100 ns and 1.4 allocations each). Telemetry costs about
1.1 us per request when enabled and nothing is exported
(`http_telemetry/enabled_no_export` against `disabled`, the same 7
allocations), and about 5.9 us when finished spans are also queued and handed to
an exporter task (`enabled_exporting`, 21 allocations; that figure includes
cross-thread hand-off). `uuid_v7/new` is 440 ns because the `uuid` crate reads
the operating system's random source on every call; formatting is 46 ns.
Rendering a page of 50 rows with Askama is about 17 us and 10 allocations
(`html_render/layout_partial`), 20 us with the CSRF and flash layers.

### End to end

`scripts/bench.sh` with its defaults (a generated application on PostgreSQL, the
load generator and the database on the same four CPUs).

```text
concurrency 16, 10s per run, 2s warm-up, 3 runs, index of 50 rows; figures are medians over the runs
```

| Scenario | Requests/s | Requests/s (min-max) | p50 ms | p95 ms | p99 ms | Errors |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `health` | 17273 | 13919-17495 | 0.81 | 1.79 | 3.29 | 0 |
| `welcome` | 44464 | 40919-49395 | 0.32 | 0.66 | 1.25 | 0 |
| `index_html` | 3361 | 2704-3605 | 4.44 | 8.38 | 11.2 | 0 |
| `create_form` | 2143 | 1306-2891 | 5.76 | 11.4 | 19.8 | 0 |

`show_json` is not run (see [End-to-end scenarios](#end-to-end-scenarios)). The
spread between runs is wide on this host (`create_form` ranged from 1306 to
2891 requests/s) because other work ran on the same CPUs. `index_html` renders
50 rows per request and `create_form` inserts one row and redirects; `health`
(a database ping) and `welcome` (no database) show what the rig does for
requests without a table. Where the time goes was not profiled.


## Output format (shared with Forge-Go)

Forge-Rust reports the same scenarios with the same columns as Forge-Go.

**Micro-benchmarks** are listed one row per scenario and variant, with the
median of three runs:

```text
| Scenario | Variant | ns/op | B/op | allocs/op |
```

`ns/op` is wall-clock time per operation; `B/op` and `allocs/op` are heap
bytes and allocations per operation, or `n/a` where the counting allocator is
not installed. Variants are named exactly as in Forge-Go's catalog
(`static/100`, `context/5`, `list_50`); a scenario without variants has `-` in
the Variant column. Rows that Forge-Go's catalog does not have (`request_build`)
are printed after the shared table, under their own heading.

**End to end** is a markdown table with these columns, preceded by one line of
settings (concurrency, time per run, warm-up, runs, index rows):

```text
| Scenario | Requests/s | Requests/s (min-max) | p50 ms | p95 ms | p99 ms | Errors |
```

The scenario names are `health`, `welcome`, `index_html` and `create_form`
(and `show_json` once Forge-Rust can run it). Figures are medians over the
runs; `Errors` is the total of requests with an unexpected status or a
transport failure. `-format json` prints the same figures as a JSON document
(`concurrency`, `duration_seconds`, `warmup_seconds`, `runs`, `index_rows` and,
per scenario, `scenario`, `runs[]`, `median` and `errors`, each with
`requests_per_second`, `p50_ms`, `p95_ms` and `p99_ms`), with the keys in the
order Forge-Go writes them. The scenarios send the same requests to a generated
application of the same shape (tenant-scoped `post` scaffold with `title` and
`body`, PostgreSQL, non-superuser roles), apart from the differences listed
under [End-to-end scenarios](#end-to-end-scenarios).
