# ADR 0034: Preconfigured local observability stack (Grafana, Prometheus, Tempo, Loki)

- Status: Accepted
- Date: 2026-10-20
- Parity: Forge-Go ADR 0038 ("Preconfigured local observability stack") as
  updated by ADR 0040 (OTLP log export replaced the fluentd log driver); part 2
  of [forge-rust#13](https://github.com/rocketinsidecorp/forge-rust/issues/13)
- Refines: [ADR 0033](0033-telemetry.md) (telemetry), [ADR 0008](0008-docker-by-default.md) (Docker by default)

## Context

[ADR 0033](0033-telemetry.md) gave Forge-Rust applications traces, metrics
and trace-correlated logs over OTLP/HTTP, with Forge-Go's variables, metric
names and wire format, but left the receiving side to each team. Forge-Go
generates a preconfigured local stack (Forge-Go ADR 0038): an OpenTelemetry
Collector, Prometheus, Tempo, Loki and Grafana with data sources, trace-to-log
links and a dashboard, started with one documented command. The parity rule
asks Forge-Rust for the same capability with the same commands, names and
style. Forge-Go's ADR 0038 states that the static files can be copied and that
the application side (variables, service name, dashboard metric names) must
follow Forge-Rust's own telemetry; since ADR 0040 both frameworks export logs
over OTLP, so no Docker log driver is involved.

Facts specific to Forge-Rust that shaped the port:

- The generated image runs with `FORGE_ENV=production`, which refuses a plain
  `http://` collector unless `FORGE_TELEMETRY_OTLP_ALLOW_PLAINTEXT=true`
  (ADR 0033). Forge-Go's compose application runs in development.
- The generated `compose.yaml` publishes the application on
  `127.0.0.1:3000`, the port Forge-Go's stack uses for Grafana (Forge-Go's
  application uses 8080). Compose overlays append to `ports`, so the overlay
  cannot move the application.
- Forge-Rust exports no `db.client.operation.duration`, pool wait counters,
  scheduler durations or Go runtime metrics; it exports `process.*` and
  `tokio.*` runtime metrics instead (ADR 0033, "Differences from Forge-Go").
- `forge-cli` has no YAML or JSON dependency.

## Decision

`forge new` generates, for every application with Docker (every database,
full-stack and `--api`; `--skip-docker` omits it with the other container
assets), Forge-Go's files under the same names:

```
compose.observability.yaml                      overlay for compose.yaml
observability/otel-collector.yaml
observability/prometheus.yml
observability/tempo.yaml
observability/loki.yaml
observability/grafana/provisioning/datasources/datasources.yaml
observability/grafana/provisioning/dashboards/dashboards.yaml
observability/grafana/dashboards/forge-application.json
```

```sh
export GRAFANA_ADMIN_PASSWORD="$(openssl rand -base64 18)"   # or in .env
docker compose -f compose.yaml -f compose.observability.yaml up --build
# Grafana http://127.0.0.1:3001 (user admin), Prometheus http://127.0.0.1:9090
docker compose -f compose.yaml -f compose.observability.yaml down       # -v deletes the data
```

The default `docker compose up` is unchanged and its file mentions nothing of
the stack (a test enforces this). The overlay is used instead of a profile for
Forge-Go's reason: Compose interpolates the whole file, so a required
`${GRAFANA_ADMIN_PASSWORD:?}` behind an inactive profile would still break the
default `up`, and a profile cannot change another service's environment.

- **Same as Forge-Go.** Service names (`otel-collector`, `prometheus`,
  `tempo`, `loki`, `grafana`), images pinned by exact version **and** the same
  multi-architecture index digests
  (`otel/opentelemetry-collector-contrib:0.163.1`, `prom/prometheus:v3.15.0`,
  `grafana/tempo:2.10.8`, `grafana/loki:3.6.18`, `grafana/grafana:13.2.4`),
  variables (`GRAFANA_ADMIN_PASSWORD`, `GRAFANA_PORT`, `PROMETHEUS_PORT`), the
  collector, Prometheus, Tempo, Loki and Grafana provisioning files (comments
  adapted), the data flow (app → OTLP/HTTP → collector → Tempo, Prometheus's
  OTLP receiver, Loki's OTLP endpoint), Tempo's trace-to-logs query on the
  `trace_id` structured metadata and trace-to-metrics queries, Loki's
  `trace_id` derived field, seven-day retention and the security defaults
  below. Dependabot's `docker-compose` ecosystem is added to the generated
  repository so the pins move deliberately, and the generated CI's `docker`
  job runs `docker compose -f compose.yaml -f compose.observability.yaml
  config --quiet`, as Forge-Go's does.
- **Application side.** The overlay sets on `app`, all existing ADR 0033
  variables: `FORGE_TELEMETRY_SERVICE_NAME: "<name>"` (quoted, so names such
  as `null`, `yes` or `off` stay strings), `FORGE_TELEMETRY_EXPORTER=otlp`,
  `FORGE_TELEMETRY_LOGS_EXPORTER=otlp`,
  `FORGE_TELEMETRY_OTLP_ENDPOINT=http://otel-collector:4318`,
  `FORGE_TELEMETRY_METRICS_INTERVAL=15s` and
  `FORGE_TELEMETRY_OTLP_ALLOW_PLAINTEXT=true`. The last is a Forge-Rust
  addition: the image runs in `production`, and the collector is reached only
  over the compose project's private network, which is the sidecar case the
  variable exists for. A test loads the image's environment, `compose.yaml`'s
  and the overlay's through `AppConfig::from_vars`, so the overlay can never
  produce an application that refuses to start.
- **Grafana on host port 3001** (`GRAFANA_PORT` still overrides it): the
  application keeps 3000 as in every other Forge-Rust document. This is the
  one user-visible difference from Forge-Go's commands.
- **Dashboard.** Forge-Go's `forge-app` dashboard ("Forge application", the
  Grafana home page, `service` variable) with its layout and the panels whose
  metrics Forge-Rust exports kept unchanged. Database operation panels, pool
  wait panels, the Go runtime row and the scheduler series are removed; a
  "Process and Tokio runtime" row (tasks and workers, global queue depth, CPU,
  memory, threads and file descriptors), an outbox deliveries panel and a
  metric series overflow panel are added. A test parses the metric catalogue
  table of [observability.md](../observability.md) and fails when the
  dashboard or the data sources query a metric it does not list, and another
  fails on Forge-Go-only names.
- **Tests without new dependencies.** `forge-cli`'s tests parse the JSON with
  `serde_json` (a workspace dependency) and the YAML with a small block-YAML
  reader in the test file that types `null` and YAML 1.1 booleans and fails
  on anything outside the subset the files use. `docker compose config`
  validates the real overlay in the end-to-end script.

### Security defaults

- Every published port binds `127.0.0.1`: Grafana (3001), Prometheus (9090)
  and the collector's OTLP/HTTP (4318, so an application run with `cargo run`
  can export). Tempo and Loki publish nothing.
- Grafana's admin password has no default: `${GRAFANA_ADMIN_PASSWORD:?...}`
  makes `docker compose` refuse to start (and to stop) the stack without it;
  no password is generated or written to a file. Anonymous access and sign-up
  are off.
- Nothing leaves the machine: Grafana's usage reporting, update checks, news
  feed, plugin preinstall, Gravatar and external snapshots are off; Tempo's and
  Loki's usage reporting is off; the collector's own metrics are off.
- Configuration is mounted read-only, holds no secrets, and is written
  world-readable (files 0644, directories 0755, independent of the umask)
  because the containers read it as their own users. `.dockerignore` keeps
  `observability/` and the overlay out of the application image.
- Residual risk, as in Forge-Go: the password gates Grafana only. Prometheus
  (with its OTLP write receiver) and the collector accept any local process,
  which can read metrics or inject telemetry. This is a single-developer local
  stack; logs reach Loki as standard output shows them.

## Consequences

- One documented command gives metrics, traces and logs in Grafana with
  trace-to-log navigation in both frameworks, with no framework dependency
  added.
- Forge-Rust maintains the dashboard and must keep it within its metric
  catalogue; image pins are shared with Forge-Go and should move together.
- Only `app` exports to the stack under the overlay; the `migrate` one-off and
  `jobs work` (not part of the generated compose file) export nothing there.
- The process panels are empty outside Linux (the metrics are read from
  `/proc`), and the pool panels are empty for `-d none` applications.
- The CI end-to-end script validates the overlay with `docker compose config`
  and smoke-tests the stack against the generated application (Grafana
  authentication, data source health, provisioned dashboard, metrics in
  Prometheus, traces in Tempo and logs in Loki), with loopback requests only.
