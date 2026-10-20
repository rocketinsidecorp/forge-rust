# Observability

Forge applications produce traces, metrics and trace-correlated logs with
the `forge-telemetry` crate: the part of OpenTelemetry an application needs,
implemented over the HTTP and TLS stack Forge already ships, speaking the
OpenTelemetry wire protocol (OTLP/HTTP with JSON encoding) and the
Prometheus text format ([ADR 0033](adr/0033-telemetry.md)). It is the port of
Forge-Go's `telemetry` package: the same variables, metric names, labels,
limits and wire format, so one OpenTelemetry Collector, Prometheus and
Grafana setup serves both frameworks.

Telemetry is **on by default and exports nothing by default**. Out of the box
every request has a trace ID that appears in its log lines; incoming trace
headers are ignored unless you trust them; metrics are kept in memory. You
choose an export path:

| Goal | Setting |
| ---- | ------- |
| Prometheus scrapes the process | `FORGE_TELEMETRY_METRICS_ADDR=127.0.0.1:9464` (a separate listener serving `GET /metrics`) |
| Traces and metrics to an OpenTelemetry Collector | `FORGE_TELEMETRY_EXPORTER=otlp`, `FORGE_TELEMETRY_OTLP_ENDPOINT=http://collector:4318` |
| Logs to an OpenTelemetry Collector, with trace IDs | `FORGE_TELEMETRY_LOGS_EXPORTER=otlp` (and `FORGE_TELEMETRY_OTLP_ENDPOINT`) |
| See spans while developing | `FORGE_TELEMETRY_EXPORTER=log` |
| No telemetry at all | `FORGE_TELEMETRY_ENABLED=false` |

## Quick start

```bash
# 1. Metrics for Prometheus on a private port
FORGE_TELEMETRY_METRICS_ADDR=127.0.0.1:9464 cargo run
curl -s http://127.0.0.1:9464/metrics | grep http_server_request_duration

# 2. Spans in the development log
FORGE_TELEMETRY_EXPORTER=log cargo run

# 3. Traces and metrics to a local OpenTelemetry Collector (config below)
docker run --rm -p 4318:4318 \
  -v "$PWD/otel-collector.yaml:/etc/otelcol/config.yaml:ro" \
  otel/opentelemetry-collector:0.111.0
FORGE_TELEMETRY_EXPORTER=otlp \
FORGE_TELEMETRY_OTLP_ENDPOINT=http://127.0.0.1:4318 \
FORGE_TELEMETRY_SERVICE_NAME=billing \
cargo run

# 4. Logs too (OTLP/HTTP /v1/logs); standard output keeps the same lines
FORGE_TELEMETRY_LOGS_EXPORTER=otlp \
FORGE_TELEMETRY_OTLP_ENDPOINT=http://127.0.0.1:4318 \
cargo run
```

Pin the collector image to a current release in real deployments. The same
variables work for the release binary (`./target/release/<app>`) and the
container (`docker run -e FORGE_TELEMETRY_...`).

## Configuration

All variables are validated at startup; errors name the variable and never
echo the endpoint or a header value. Durations use Go's syntax (`10s`,
`1m30s`, `500ms`), as in Forge-Go.

| Variable | Default | Notes |
| -------- | ------- | ----- |
| `FORGE_TELEMETRY_ENABLED` | `true` | `false` creates no span, instrument or task; nothing else here is checked |
| `FORGE_TELEMETRY_SERVICE_NAME` | the application name | `service.name` resource attribute |
| `FORGE_TELEMETRY_SERVICE_VERSION` | the application version | `service.version` |
| `FORGE_TELEMETRY_EXPORTER` | `none` | `none`, `otlp`, or `log` (refused in `production`) |
| `FORGE_TELEMETRY_LOGS_EXPORTER` | `none` | `none` or `otlp`. Independent of `FORGE_TELEMETRY_EXPORTER`; `otlp` sends every log line to the collector as well as to standard output, with the `FORGE_TELEMETRY_OTLP_*` settings below |
| `FORGE_TELEMETRY_LOGS_LEVEL` | `info` | Lowest level exported: `debug`, `info`, `warn` or `error`. Standard output keeps `FORGE_LOG` |
| `FORGE_TELEMETRY_OTLP_ENDPOINT` | (empty) | Collector base URL; Forge posts to `/v1/traces`, `/v1/metrics` and `/v1/logs`. Required by either `otlp` exporter. `production` requires `https` unless `FORGE_TELEMETRY_OTLP_ALLOW_PLAINTEXT=true`. No credentials, query or fragment |
| `FORGE_TELEMETRY_OTLP_HEADERS` | (empty) | `name=value,name2=value2`, values percent-encoded (the `OTEL_EXPORTER_OTLP_HEADERS` format), e.g. `authorization=Bearer%20TOKEN`. Values are secret: redacted everywhere, sent marked sensitive |
| `FORGE_TELEMETRY_OTLP_TIMEOUT` | `10s` | Per export, retries included; at most `1m` |
| `FORGE_TELEMETRY_OTLP_COMPRESSION` | `none` | `none` or `gzip` |
| `FORGE_TELEMETRY_OTLP_ALLOW_PLAINTEXT` | `false` | Permit `http://` endpoints in `production` (sidecar or private network) |
| `FORGE_TELEMETRY_SAMPLE_RATIO` | `1` | Fraction of new traces sampled, `0` to `1` |
| `FORGE_TELEMETRY_TRUST_TRACEPARENT` | `false` | Continue traces from incoming `traceparent`/`tracestate`. Enable only behind a proxy or gateway that sets or sanitizes them |
| `FORGE_TELEMETRY_METRICS_ADDR` | (empty: off) | `ip:port` or `localhost:port` of the Prometheus listener; must differ from `FORGE_BIND` |
| `FORGE_TELEMETRY_METRICS_PUBLIC` | `false` | Confirms a non-loopback `FORGE_TELEMETRY_METRICS_ADDR` (such as `0.0.0.0:9464` in a container). Without it `production` refuses to start and other environments log a warning |
| `FORGE_TELEMETRY_METRICS_INTERVAL` | `1m` | OTLP metric push interval, at least `1s` |

## What is recorded

### Traces

| Source | Span | Attributes |
| ------ | ---- | ---------- |
| HTTP server (every request) | `GET /users/{id}` (server; `GET` when no route matched) | `http.request.method`, `http.route`, `http.response.status_code`, `url.scheme`, `network.protocol.version`, `error.type` for 5xx |
| Job attempts (`forge::jobs`) | `process <queue>` (consumer, new trace) | `messaging.system`, `messaging.operation.type`, `messaging.destination.name`, `messaging.message.id` (job id), `forge.job.kind`, `forge.job.attempt`, `forge.job.outcome` |
| Outbox deliveries (`forge::events`) | `process outbox` (consumer, new trace) | `messaging.message.id` (event id), `forge.event.type`, `forge.event.version`, `forge.outbox.attempt`, `forge.outbox.outcome` |

Span limits: 64 attributes, string values cut at 1024 bytes. Errors are
recorded as `error.type` only (a status code or an outcome such as
`dead`), never as a message.

Custom spans, from a composition hook or an adapter:

```rust
use forge::telemetry::SpanKind;

let telemetry = composition.telemetry().cloned(); // None when disabled
// In a handler: parent the span on the request's server span.
if let Some(telemetry) = &telemetry {
    let span = telemetry
        .tracer("billing")
        .start("charge card", SpanKind::Internal, request.trace_context());
    // ... on failure: span.record_error_type("declined");
    span.end();
}
```

To call another service inside the trace, write the request's context into
the outgoing headers: `request.trace_context().map(|context|
context.inject(&mut headers))`. Do it only for services that may see your
trace IDs.

### Metrics

| Metric (Prometheus name) | Type | Labels |
| ------------------------ | ---- | ------ |
| `http.server.request.duration` (`http_server_request_duration_seconds`) | histogram | `http_request_method`, `http_route`, `http_response_status_class` |
| `http.server.active_requests` (`http_server_active_requests`) | gauge | `http_request_method` |
| `forge.queue.job.duration` (`forge_queue_job_duration_seconds`) | histogram | `messaging_destination_name`, `forge_job_kind`, `forge_job_outcome` |
| `forge.outbox.dispatch.duration` (`forge_outbox_dispatch_duration_seconds`) | histogram | `forge_event_type`, `forge_outbox_outcome` |
| `db.client.connection.count` (`db_client_connection_count`) | gauge | `db_system_name`, `db_client_connection_pool_name`, `db_client_connection_state` (`idle`, `used`) |
| `db.client.connection.max` (`db_client_connection_max`) | gauge | `db_system_name`, `db_client_connection_pool_name` |
| `process.memory.usage` (`process_memory_usage_bytes`) | gauge | |
| `process.memory.virtual` (`process_memory_virtual_bytes`) | gauge | |
| `process.cpu.time` (`process_cpu_time_seconds_total`) | counter | |
| `process.thread.count` (`process_thread_count`) | gauge | |
| `process.unix.file_descriptor.count` (`process_unix_file_descriptor_count`) | gauge | |
| `tokio.worker.count` (`tokio_worker_count`) | gauge | |
| `tokio.task.count` (`tokio_task_count`) | gauge | |
| `tokio.global_queue.depth` (`tokio_global_queue_depth`) | gauge | |
| `forge.telemetry.spans.dropped` (`forge_telemetry_spans_dropped_total`) | counter | |
| `forge.telemetry.logs.dropped` (`forge_telemetry_logs_dropped_total`) | counter | |
| `forge.telemetry.export.errors` (`forge_telemetry_export_errors_total`) | counter | `signal` (`traces`, `metrics`, `logs`) |
| `forge.telemetry.metric.overflows` (`forge_telemetry_metric_overflows_total`) | counter | |

Job outcomes are Forge-Go's: `succeeded`, `retry`, `dead`, `released`
(interrupted by shutdown), `lease_lost`, `store_failed`; attempts that
panicked or were aborted are counted too. Outbox outcomes: `delivered`,
`retry`, `dead_lettered`, `lease_lost`, `error`.

**Connection pools.** The application's database pool (PostgreSQL, MySQL or
MariaDB, SQLite) is read when metrics are collected (a scrape or an OTLP
push), never per query: connections in use and idle against the limit, pool
name `default`, `db.system.name` `postgresql`, `mysql` or `sqlite`. When the
pool closes at shutdown its counts drop to zero.

**Process and runtime.** Read on collection. On Linux,
`process.memory.usage` is the resident set (`VmRSS`), `process.memory.virtual`
`VmSize`, `process.cpu.time` user plus system time, plus thread and open file
descriptor counts (from `/proc/self`; other systems record none of these).
`tokio.task.count` is the number of live tasks: one that only grows is a task
leak (the counterpart of Forge-Go's goroutine count).

Duration buckets (seconds) follow the OpenTelemetry advice: 0.005, 0.01,
0.025, 0.05, 0.075, 0.1, 0.25, 0.5, 0.75, 1, 2.5, 5, 7.5, 10. Counters and
histograms are cumulative. Application metrics use the same registry and
limits:

```rust
if let Some(telemetry) = composition.telemetry() {
    let orders = telemetry
        .metrics()
        .counter("shop.orders.created", "Orders created.", "{order}", &["channel"])
        .map_err(ComposeError::other)?;
    orders.add(1, &["web"]);
}
```

### Logs

Every line logged inside a request carries `trace_id` and `span_id`: they
are fields of the request's `http.request` log span, so the standard output
formatter prints them (`trace_id=4bf9…` in text, inside `"span"` in JSON).
Job attempts and outbox deliveries do the same. Search your log store for a
trace ID from a trace, or the reverse.

With `FORGE_TELEMETRY_LOGS_EXPORTER=otlp` the application's `tracing`
subscriber writes every line to standard output **and** queues it for the
collector's `/v1/logs` (a second layer of the same subscriber, Forge-Go's
tee). Each OTLP log record has:

| Field | Source |
| ----- | ------ |
| body | the message, cut at 4096 bytes |
| severity number and text | the level: TRACE 1, DEBUG 5, INFO 9, WARN 13, ERROR 17 |
| attributes | the event's fields and those of its enclosing spans (the nearest wins), at most 64 (the rest counted in `droppedAttributesCount`), string values cut at 1024 bytes |
| `traceId`, `spanId` | the `trace_id`/`span_id` fields of the nearest enclosing span; fields of the record, not repeated as attributes |
| resource | `service.name`, `service.version`, `deployment.environment.name`, as for traces and metrics |
| time, observed time | when the event happened; when Forge queued it |

The exporter sees only what `FORGE_LOG` lets through, and values are
formatted the way the standard-output formatter formats them (`Debug` for
values logged with `?` or `%`), so a `SecretString` exports as `[REDACTED]`,
exactly as on standard output. **The exporter never sends more than standard
output shows**: keep personal data and secrets out of log calls as you would
for standard output. Lines of the `tracing-log` bridge's `log.*` metadata are
not exported, as the formatter does not print them.

## Cardinality rules

Every label value comes from a bounded set chosen by code, never from
request input:

- HTTP routes are the matched template (`/users/{id}`), never the raw path.
  Requests no route matched (404, 405, 400, and requests a layer answered
  before routing) have an empty route label.
- Methods outside the standard set are recorded as `_OTHER`; statuses as
  classes (`2xx`...`5xx`).
- Queue names, job names and event types are the names the application
  registered.

As a backstop each instrument holds at most 1000 series. New combinations
beyond that are folded into one series whose labels are all `_overflow`,
and `forge_telemetry_metric_overflows_total` counts them: an alert on that
counter means a label is fed by unbounded data. Label values are cut at 128
bytes, and labels are declared when an instrument is created.

## What is never recorded

Request and response bodies, query strings, raw URL paths, headers, cookies,
client IP addresses, user agents, SQL text and arguments, job payloads,
event payloads and error messages, in spans and metrics. Span names are
route templates, queue names or fixed names. Application code that adds
attributes is responsible for keeping personal data out of them; log lines
are exported as standard output shows them.

## Export paths

### OpenTelemetry Collector

`otel-collector.yaml`, printing what it receives and forwarding nowhere:

```yaml
receivers:
  otlp:
    protocols:
      http:
        endpoint: 0.0.0.0:4318
processors:
  batch: {}
exporters:
  debug:
    verbosity: basic
service:
  pipelines:
    traces:
      receivers: [otlp]
      processors: [batch]
      exporters: [debug]
    metrics:
      receivers: [otlp]
      processors: [batch]
      exporters: [debug]
    logs:
      receivers: [otlp]
      processors: [batch]
      exporters: [debug]
```

Replace `debug` with your backend's exporter. Keeping a collector between
the application and a vendor confines vendor credentials and tail sampling
to the collector; when the application talks to a vendor directly, put the
API key in `FORGE_TELEMETRY_OTLP_HEADERS`.

The exporter sends batches of up to 512 spans at least every 5 seconds, log
records in batches of up to 512 at least every second, and metrics every
`FORGE_TELEMETRY_METRICS_INTERVAL`. It retries network errors and HTTP
429/502/503/504 up to 5 times with exponential backoff (500 ms doubling to
5 s) and jitter, honors `Retry-After` (seconds or a date, at most 30 s),
never follows redirects, and gives up within `FORGE_TELEMETRY_OTLP_TIMEOUT`.
`https` endpoints are verified against the Mozilla root store (webpki
roots) with rustls.

### Prometheus and Grafana

Set `FORGE_TELEMETRY_METRICS_ADDR` and scrape it:

```yaml
# prometheus.yml
scrape_configs:
  - job_name: forge
    scrape_interval: 15s
    static_configs:
      - targets: ["127.0.0.1:9464"]
```

Useful queries:

```promql
# p95 latency per route
histogram_quantile(0.95, sum by (le, http_route) (rate(http_server_request_duration_seconds_bucket[5m])))
# error ratio
sum(rate(http_server_request_duration_seconds_count{http_response_status_class="5xx"}[5m]))
  / sum(rate(http_server_request_duration_seconds_count[5m]))
# dead jobs
sum by (forge_job_kind) (rate(forge_queue_job_duration_seconds_count{forge_job_outcome="dead"}[15m]))
# pool saturation
sum(db_client_connection_count{db_client_connection_state="used"}) / sum(db_client_connection_max)
```

The metrics listener is separate from the application listener by design:
the application router never serves `/metrics`. It is plain HTTP without
authentication, answers only `GET`/`HEAD /metrics` (`405` with `Allow` for
other methods, `404` for other paths), and accepts at most 16 connections.
Outside containers bind it to `127.0.0.1`. In a container bind it to
`0.0.0.0:9464` so Prometheus can reach it **inside the cluster network**,
confirm that with `FORGE_TELEMETRY_METRICS_PUBLIC=true`, and do not publish
the port to the internet.

A preconfigured local stack (Collector, Prometheus, Tempo, Loki, Grafana with
a dashboard), as Forge-Go generates with `compose.observability.yaml`, is the
second part of rocketinsidecorp/forge-rust#13 and not generated yet.

## Propagation and sampling

By default incoming `traceparent` and `tracestate` headers are ignored and
every request starts a new trace: a client could otherwise pick the trace
IDs that appear in your logs, force recording of its requests past
`FORGE_TELEMETRY_SAMPLE_RATIO` (a sampled parent is always followed), and
push `tracestate` to your downstream services. Set
`FORGE_TELEMETRY_TRUST_TRACEPARENT=true` behind a proxy, gateway or mesh
that sets or sanitizes these headers. Trusted requests with a valid W3C
`traceparent` continue that trace and keep the validated `tracestate`.
Parsing is strict: lowercase hex, no all-zero identifiers, exact lengths,
repeated `traceparent` headers rejected.

New traces are sampled with probability `FORGE_TELEMETRY_SAMPLE_RATIO`
(OpenTelemetry's `TraceIdRatioBased` algorithm, so services sharing a ratio
agree); child spans follow their parent. Unsampled spans still have IDs for
log correlation and propagation but record nothing; without a span exporter
nothing is recorded either.

## Lifecycle and backpressure

`App` builds the telemetry from the configuration before logging is
installed (so the log exporter can be part of the subscriber), starts its
exporter tasks inside the runtime, instruments the HTTP server, the
database pool and, with `App::jobs`/`App::events`, the job runtime and the
outbox dispatcher, and shuts it down **last**, after the server drained, the
workers stopped and the pool closed, so their spans are exported. The
dedicated `jobs work` process does the same (with the metrics listener when
configured); other commands (`migrate`, `setup`, ...) export nothing.

Ended spans and log records wait in bounded in-memory queues (2048 each).
One task per signal, owned by the `Telemetry` and joined at shutdown,
exports them; no task is detached. When a queue is full new items are
dropped and counted (`forge_telemetry_spans_dropped_total`,
`forge_telemetry_logs_dropped_total`) instead of slowing requests down; a
failed export drops its batch after the retries, counts it in the same
metric and in `forge_telemetry_export_errors_total{signal}`, and is
reported **on standard output only**: the telemetry's own diagnostics
(target `forge_telemetry`) are never exported, so a broken collector cannot
feed itself. On shutdown spans, log records and a final metric snapshot are
flushed concurrently within what remains of `FORGE_SHUTDOWN_GRACE_SECS`
since shutdown began; whatever is still queued at the deadline is dropped
and counted. Lines logged after that reach standard output only.

## Differences from Forge-Go

The variables, names, labels, limits and wire format are Forge-Go's. These
parts differ, because Forge-Rust has no equivalent yet or the Rust runtime
has no such counter:

- `http.client.request.duration`: Forge-Rust has no instrumented outgoing
  HTTP client; propagate with `SpanContext::inject`.
- `db.client.operation.duration` and per-query spans: SQLx offers no query
  hook, and wrapping every executor would change the `forge-db` contracts;
  tracked in rocketinsidecorp/forge-rust#13.
- `forge.db.pool.waits` and `forge.db.pool.wait.duration`: the SQLx pool
  exposes no acquisition-wait counters and adapters acquire through it
  directly. They are not emulated; tracked in rocketinsidecorp/forge-rust#13.
- `forge.scheduler.job.duration`: Forge-Rust's cron schedules enqueue
  durable jobs (ADR 0025) instead of running functions in process, so
  scheduled work is measured by `forge.queue.job.duration`.
- `forge.mail.send.duration`: Forge-Rust has no mailer yet.
- `go.goroutine.count`, `go.memory.heap.alloc`, `go.memory.heap.inuse`,
  `go.memory.total`, `go.memory.gc.goal`, `go.memory.allocated`,
  `go.gc.cycles`, `go.gc.pause.duration`, `go.processor.limit`: Rust has no
  garbage collector or goroutines; the `process.*` and `tokio.*` metrics
  above take their place.
- `FORGE_TELEMETRY_SERVICE_NAME` and `FORGE_TELEMETRY_SERVICE_VERSION`
  default to the name and version given to `App` rather than
  `unknown_service:<binary>` and empty.
- `FORGE_TELEMETRY_METRICS_ADDR` takes an IP address or `localhost` (not
  other host names), like `FORGE_BIND`; it must differ from `FORGE_BIND`.
- `FORGE_TELEMETRY_OTLP_COMPRESSION` and `FORGE_TELEMETRY_OTLP_TIMEOUT` are
  validated even when no OTLP exporter is selected.
- OTLP log records carry no trace flags (the sampled bit), which the span
  fields do not hold; `traceId` and `spanId` are set.
- The exporter opens one connection per export attempt instead of keeping a
  pool; at the batch intervals above that is a handful of connections per
  minute.

## Limitations

- Logs are exported from the `tracing` subscriber only; other output (a
  crash's stack trace, `println!`) needs your log shipper.
- OTLP/HTTP with JSON encoding only; no gRPC and no protobuf encoding.
- Metric temporality is cumulative; exemplars, exponential histograms,
  span events, span links and baggage are not implemented.
- Jobs and outbox deliveries start new traces linked to their origin by job
  or event id, not by stored context.
