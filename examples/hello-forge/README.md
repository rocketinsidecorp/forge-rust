# hello-forge

A [Forge](https://github.com/rocketinsidecorp/forge-rust) application organized
around hexagonal architecture boundaries:

```text
src/domain          business rules; no I/O, HTTP, database or vendor types
src/application     commands, queries, use cases and ports
src/adapters        HTTP and other protocol translation
src/infrastructure  concrete database, cache, queue and telemetry mechanisms
src/bootstrap       composition root and process lifecycle
```

`tests/architecture.rs` fails when an inner ring imports an outer one.

## Views

Pages are rendered on the server, as in Rails
([ADR 0022](https://github.com/rocketinsidecorp/forge-rust/blob/main/docs/adr/0022-server-rendered-views.md)):

```text
templates/layouts/application.html  layout: CSRF meta tag, stylesheet, import map
templates/layouts/_flash.html       one-shot notice/alert messages
templates/welcome/index.html        the page served at /
src/adapters/http/views/            view models checked against the templates at compile time
assets/                             CSS, JavaScript and the logo, digested and embedded by build.rs
```

Templates use [Askama](https://askama.rs): `{{ value }}` is HTML-escaped and a
template error is a build error. Turbo and Stimulus are vendored by Forge and
loaded through the import map; no Node toolchain is needed. Every response
carries a strict Content-Security-Policy (same-origin scripts and styles, the
import map allowed by hash, no inline scripts or `style` attributes).

Every form that changes state must carry the page's CSRF token:
`forge::views::csrf_field(&token)` renders the hidden `csrf_token` input and
Turbo sends the `<meta name="csrf-token">` value in `x-csrf-token`. Unsafe
requests without a matching token are refused with `403`. A `POST` form can
tunnel `PATCH`, `PUT` or `DELETE` in a `_method` field. Decode form bodies with
`forge::web::decode_form`, answer a successful submission with
`forge::views::see_other` (`303`) and attach a message with
`forge::views::Flash`. The CSRF and flash cookies use the `__Host-` prefix
and are `Secure`: browsers accept them on `https://` and on
`http://localhost`/`127.0.0.1`.

## Development

```bash
cargo run                     # serves on http://127.0.0.1:3000
curl http://127.0.0.1:3000/health/live
forge check && forge lint && forge test
```

Binary commands: `serve` (default), `healthcheck`, `version`, `help`.

Configuration uses `FORGE_*` environment variables; unknown `FORGE_*` keys are
rejected at startup. Run `cargo run -- help` for the full list.

Behind a reverse proxy (nginx, Traefik, a load balancer), list its networks in
`FORGE_HTTP_TRUSTED_PROXIES` so throttles, rate limits and live-update caps
count each client (`Request::client_ip`) instead of the proxy. Empty, the
default, trusts nobody; `/0`, IPv4 prefixes shorter than `/8`, IPv6 prefixes
shorter than `/16` and malformed entries stop startup. Only `X-Forwarded-For`
from a listed peer is read, from the right; keep the application port
reachable only through the proxy:

```bash
export FORGE_HTTP_TRUSTED_PROXIES=172.30.0.0/24,fd00:1::/64   # proxy CIDRs or addresses
cargo run
```

## Observability

The framework traces every request (a span named after the route template),
records request, job, connection pool, process and runtime metrics, and puts
`trace_id`/`span_id` on every log line of a request. Telemetry is on and
exports nothing until you choose a path; the variables and metric names are
Forge-Go's ([observability guide](https://github.com/rocketinsidecorp/forge-rust/blob/main/docs/observability.md)):

```bash
# Prometheus text on a separate, private listener (never on the app port)
FORGE_TELEMETRY_METRICS_ADDR=127.0.0.1:9464 cargo run
curl -s http://127.0.0.1:9464/metrics | grep http_server_request_duration_seconds_count
# Spans in the development log
FORGE_TELEMETRY_EXPORTER=log cargo run
# Traces, metrics and logs to an OpenTelemetry Collector (OTLP/HTTP)
FORGE_TELEMETRY_EXPORTER=otlp FORGE_TELEMETRY_LOGS_EXPORTER=otlp \
  FORGE_TELEMETRY_OTLP_ENDPOINT=http://127.0.0.1:4318 cargo run
# Off entirely
FORGE_TELEMETRY_ENABLED=false cargo run
```

Production requires an `https` collector endpoint; API keys go in
`FORGE_TELEMETRY_OTLP_HEADERS` (`authorization=Bearer%20TOKEN`), which is
redacted everywhere. Register application metrics and spans through
`composition.telemetry()` in `src/bootstrap`.
In a container, bind the metrics listener to `0.0.0.0:9464`, confirm it with
`FORGE_TELEMETRY_METRICS_PUBLIC=true`, and do not publish that port.

### Observability stack: Grafana, Prometheus, Tempo and Loki

`compose.observability.yaml` adds a preconfigured stack to the compose
project: an OpenTelemetry Collector, Prometheus (metrics), Tempo (traces),
Loki (logs) and Grafana with the data sources and a starter dashboard already
provisioned. It is opt-in: `docker compose up` does not start it. Everything
stays on your machine and every published port is bound to `127.0.0.1`.

```bash
# 1. Start it (Grafana refuses to start without an admin password you choose)
export GRAFANA_ADMIN_PASSWORD="$(openssl rand -base64 18)"   # or put it in .env
docker compose -f compose.yaml -f compose.observability.yaml up --build

# 2. Generate some data
curl http://127.0.0.1:3000/

# 3. Open Grafana: http://127.0.0.1:3001, user admin, the password above
#    Dashboards > "Forge application"  request rate, latency, errors, connection pool, process and Tokio runtime, jobs, outbox, logs, traces
#    Explore > Tempo                   traces; "Logs for this span" opens the matching log lines
#    Explore > Loki                    logs; a trace_id links to its trace
#    Explore > Prometheus              metrics, also at http://127.0.0.1:9090

# 4. Stop it (keeps the data); add -v to delete traces, logs and metrics too
# compose interpolates the whole file, so GRAFANA_ADMIN_PASSWORD must be set for down too
docker compose -f compose.yaml -f compose.observability.yaml down
```

The app sends traces, metrics and logs to the collector over OTLP (the
overlay sets `FORGE_TELEMETRY_EXPORTER=otlp`, `FORGE_TELEMETRY_LOGS_EXPORTER=otlp`
and, because the image runs in `production` and the collector is on the
compose network, `FORGE_TELEMETRY_OTLP_ALLOW_PLAINTEXT=true`), which forwards
them to Tempo, Prometheus and Loki. Grafana is on host port 3001 because the
app uses 3000; set `GRAFANA_PORT` or `PROMETHEUS_PORT` to use other ports. The
app logs what it or the framework logs (startup, errors, your `tracing` calls);
each line logged inside a request carries `trace_id` and `span_id`, which is
what ties logs and traces together. Output that does not go through `tracing`
(a crash's stack trace, `println!`) is only in `docker compose logs`. The stack
keeps seven days of data and is meant for development, not production. To
export from an app started with `cargo run` while the stack runs:

```bash
FORGE_TELEMETRY_SERVICE_NAME=hello-forge FORGE_TELEMETRY_EXPORTER=otlp \
  FORGE_TELEMETRY_LOGS_EXPORTER=otlp FORGE_TELEMETRY_OTLP_ENDPOINT=http://127.0.0.1:4318 cargo run
```

## Container

```bash
docker build -t hello-forge .
docker run --rm -p 3000:3000 hello-forge
docker compose up --build
```

The image is built with `cargo build --release --locked`, so commit
`Cargo.lock`. The runtime stage is distroless and runs as uid 65532; the Docker
health check runs `hello-forge healthcheck`.
