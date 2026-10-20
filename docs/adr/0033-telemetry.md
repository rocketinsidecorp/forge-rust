# ADR 0033: Forge-owned telemetry over OpenTelemetry protocols

- Status: Accepted
- Date: 2026-10-20
- Parity: Forge-Go ADR 0034 ("Dependency-free telemetry speaking OpenTelemetry protocols") and ADR 0040 ("OTLP log export, Go runtime metrics and connection pool metrics"); part 1 of [forge-rust#13](https://github.com/rocketinsidecorp/forge-rust/issues/13)

## Context

Phase 5 asks for traces with W3C trace-context propagation, bounded
metrics with an export path, logs correlated with traces, and database and
background-work instrumentation. Forge-Go ships them in a dependency-free
`telemetry` package configured by `FORGE_TELEMETRY_*`; the parity rule
(owner decision, 2026-10-20) requires the same variables, names and
behavior here. Forge-Rust had only `tracing` to standard output.

The obvious Rust route is the OpenTelemetry SDK: `opentelemetry`,
`opentelemetry_sdk`, `opentelemetry-otlp` (with `tonic`/`prost` or
`reqwest`), `opentelemetry-semantic-conventions`, `tracing-opentelemetry`,
and for Prometheus `opentelemetry-prometheus` with the `prometheus` crate.
That is several dozen new packages (gRPC and protobuf stacks, a second HTTP
client, `futures` executors, `async-trait`-style exporter plumbing), several
of them still pre-1.0 with frequent breaking releases, and SDK tasks that
spawn their own background workers outside Forge's lifecycle. It would also
make the metric names, labels and limits follow the SDK's choices rather
than Forge-Go's, which the shared dashboards depend on.

What an application needs is narrow and is already specified by Forge-Go:
start spans, propagate context, record a few instruments, render Prometheus
text, and post three JSON documents to a collector. The HTTP client, TLS and
JSON pieces are already in the locked graph (hyper, rustls with ring and
the webpki roots for SQLx, serde_json).

## Decision

Forge implements the needed subset of OpenTelemetry in a framework crate,
`forge-telemetry`, and interoperates through the standard protocols. It is a
line-by-line port of Forge-Go's design.

### Configuration

`TelemetryConfig` in `forge-config` reads the sixteen `FORGE_TELEMETRY_*`
variables with Forge-Go's defaults and rules (Go duration syntax and boolean
spellings included): telemetry on, exporter `none`; either `otlp` exporter
requires an `http(s)` endpoint without credentials, query or fragment;
`production` requires `https` unless plaintext is allowed and refuses the
`log` exporter; the metrics listener differs from `FORGE_BIND` and is
loopback in production unless confirmed public. Header values are
`SecretString`; errors never echo the endpoint or a header.

### Tracing and propagation

- Strict W3C `traceparent`/`tracestate` parsing; incoming context is ignored
  unless `FORGE_TELEMETRY_TRUST_TRACEPARENT=true`.
- A minimal tracer: kind, attributes (64, values cut at 1024 bytes), status,
  identifiers from the OS CSPRNG, parent-based `TraceIdRatioBased`
  sampling. Spans are recorded only when sampled and an exporter is
  configured; otherwise they still carry identifiers.
- `tracing` spans are **not** converted into OpenTelemetry spans: library
  spans carry raw values (the request path, SQL) and would break the
  privacy-by-construction rule. Forge's spans are explicit; application code
  uses `Telemetry::tracer`.

### HTTP instrumentation (adapter ring)

`forge_http::Server::with_telemetry` instruments every request outside every
layer, as Forge-Go's dedicated instrument slot: a server span named after
the matched route template, `http.server.request.duration` (method,
template, status class) and `http.server.active_requests`. The router
reports the template through a private slot in the request extensions
(Forge-Go's `web.RouteInfo`), so layers cannot forge it and unmatched
requests have an empty route. A panicking handler is recorded as a 5xx and
the active gauge returns to zero. Handlers read `Request::trace_context` to
parent spans or propagate the trace.

### Log correlation and export

The request's `http.request` log span gains `trace_id` and `span_id` fields,
so every line logged inside it is correlated on standard output. With
`FORGE_TELEMETRY_LOGS_EXPORTER=otlp` a second `tracing-subscriber` layer of
the global subscriber (Forge-Go's tee handler) turns events into OTLP log
records: the message as body, the event's and enclosing spans' fields as
attributes formatted as the standard-output formatter formats them (so
`SecretString` exports `[REDACTED]`), the nearest `trace_id`/`span_id` as
the record's identifiers, Go's severity numbers and limits. The global
`FORGE_LOG` filter applies to both layers; the level threshold is checked
inside the layer, never in `enabled`, which would filter standard output
too. The exporter therefore never sends more than standard output shows.

### Export and backpressure

- OTLP/HTTP with the JSON encoding to `/v1/traces`, `/v1/metrics` and
  `/v1/logs`: one connection per attempt over hyper's client, driven inline
  by the export future (no detached connection task), TLS through
  `tokio-rustls` with the ring provider and the webpki roots, five attempts
  with backoff and jitter on network errors and 429/502/503/504,
  `Retry-After`, the export timeout as a hard deadline, optional gzip,
  header values marked sensitive, no redirects, errors that carry only the
  signal path and the status or transport cause.
- Spans and log records go through bounded queues (2048) with one exporter
  task each; a full queue drops and counts, producers never wait. Metrics
  are pushed every `FORGE_TELEMETRY_METRICS_INTERVAL` by one task.
  `Telemetry::new` only builds the queues, so the log layer can be installed
  before the runtime exists; `Telemetry::start` spawns the tasks inside the
  runtime and keeps their handles; `Telemetry::shutdown` flushes all three
  signals concurrently before a deadline and aborts and counts what is left.
- `App` shuts telemetry down last, after the server drained, workers stopped
  and the pool closed, within what remains of `FORGE_SHUTDOWN_GRACE_SECS`
  since shutdown began.
- Export failures are logged on the `forge_telemetry` target, which the log
  layer never exports: a failing collector cannot feed itself.

### Metrics

A registry with Forge-Go's limits: label names fixed at registration (at
most 6), 1000 series per instrument then an `_overflow` series counted by
`forge.telemetry.metric.overflows`, label values cut at 128 bytes, counters,
float counters, gauges, explicit-bucket histograms, and observers run at
collection for state cheaper to read than to push. Prometheus text 0.0.4
with Go's name conversion is served on a **separate listener**
(`FORGE_TELEMETRY_METRICS_ADDR`, a `forge_http::Server` with only
`GET`/`HEAD /metrics`), never on the application router.

The catalogue is Go's where Forge-Rust has the instrumented component:
HTTP server, job attempts (`forge.queue.job.duration`, Go's outcomes),
outbox deliveries (`forge.outbox.dispatch.duration`), pool gauges
(`db.client.connection.count`/`.max`) and telemetry health. A test holds the
shared list against Go's catalogue and checks that every deviation is
explained in [observability.md](../observability.md#differences-from-forge-go).
In place of `go.*`, Forge-Rust records the OpenTelemetry process conventions
it can read cheaply from `/proc/self` on Linux and the stable Tokio runtime
counters (workers, live tasks, global queue depth); no counting allocator is
installed.

### Rings

`forge-telemetry` is an infrastructure/adapter crate. Domain and application
code never depend on it; generated applications reach it only through
`forge::telemetry` in bootstrap and adapters. `forge-http`, `forge-jobs` and
`forge-events` take its instruments as explicit, optional values (no global
provider); `forge` (the composition root) builds it from configuration.

### Dependencies

Admitted: `tokio-rustls` 0.26 (rustls organization; default features off,
`ring` and `tls12`), `flate2` 1.1 with the pure-Rust `miniz_oxide` backend
(brings `miniz_oxide`, `adler2`, `crc32fast`, `simd-adler32`), and hyper's
`client` feature (brings `want`, `try-lock`). `rustls`, `webpki-roots`,
`httpdate`, `getrandom`, `tracing-subscriber` were already in the graph. See
[dependency-policy.md](../dependency-policy.md#telemetry-dependency-decision).

## Consequences

- Applications gain traces, metrics, runtime and pool metrics and
  correlated logs with a handful of new packages, and any OpenTelemetry
  Collector, Prometheus, Grafana or OTLP-capable vendor can consume them.
  Dashboards and alerts written for Forge-Go's names work unchanged for the
  shared metrics.
- Forge maintains the encoding and propagation code and must follow OTLP
  and W3C changes itself; the OTLP JSON mapping is stable in OTLP 1.x.
- The per-request cost with telemetry on and nothing exported is two random
  identifiers, a span context, a histogram and gauge update and two extra
  log-span fields; `FORGE_TELEMETRY_ENABLED=false` removes all of it.
- Not implemented (as in Forge-Go): OTLP gRPC/protobuf, baggage, exemplars,
  exponential histograms, delta temporality, span events and links. Not yet
  ported: an instrumented outgoing HTTP client, per-query database spans
  and `db.client.operation.duration` (SQLx has no query hook), pool wait
  counters (SQLx exposes none), the mailer metric, and the local
  observability stack (part 2 of forge-rust#13).
