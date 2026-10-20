# Deploying Forge Applications

Status: Phase 3 in progress. This guide covers the generated container image,
standalone binary, PostgreSQL development profile, least-privilege database roles, environment configuration,
health endpoints and graceful shutdown. The examples use an application named `shop`.
Kubernetes and systemd snippets are starting points to review, not certified
configurations. Forge does not generate a deployment tool yet
([ADR 0009](adr/0009-rails-style-generated-delivery-assets.md)).

## Container image

`forge new` generates a multi-stage `Dockerfile` (unless `--skip-docker` is
used; see [ADR 0008](adr/0008-docker-by-default.md)).

**Builder stage**
- Image: `rust:<pinned toolchain>-slim-trixie` (Debian 13), selected by
  `ARG RUST_VERSION`. The generated `tests/container.rs` fails if that value
  differs from `rust-toolchain.toml`.
- `.dockerignore` excludes `rust-toolchain.toml` (along with build output), so
  the builder uses the image's preinstalled toolchain instead of downloading
  components.
- Build: `cargo build --release --locked`, with BuildKit cache mounts for the
  Cargo registry and target directory.
- Needs a committed `Cargo.lock`, so a stale or missing lockfile fails the build
  instead of re-resolving dependencies.

**Runtime stage**
- Image: `gcr.io/distroless/cc-debian13:nonroot`. It has no shell and no package
  manager. It contains C runtime libraries, CA certificates and the application
  binary.
- User: numeric non-root uid/gid `65532`.
- Environment: `FORGE_ENV=production` and `FORGE_BIND=0.0.0.0:3000`.
- Port: `EXPOSE 3000`.
- `HEALTHCHECK` (exec form) runs `<binary> healthcheck`, which probes
  `GET /health/live` over loopback.
- `ENTRYPOINT` is the binary itself (exec form). It runs as PID 1 and receives
  `SIGTERM` directly, with no shell wrapper.

Build and run:

```sh
docker build -t shop .
docker run --rm -p 3000:3000 \
  --read-only --cap-drop ALL --security-opt no-new-privileges \
  shop
curl localhost:3000/health/live
docker ps   # STATUS shows (healthy) once the HEALTHCHECK passes
```

Because the image has no shell, `docker exec -it <container> sh` does not work.
Debug with logs, the health endpoints, or an ephemeral debug container that
shares the process namespace.

### Compose (local convenience)

The generated `compose.yaml` builds the image and publishes it on
`127.0.0.1:3000` only. By default it also provides PostgreSQL 18, pinned by
image digest, a named `db-data` volume, and an initialization script that creates
two development-only application roles:

- `app_migrator`: owns schema changes and is used only by migrate/rollback;
- `app_runtime`: has data privileges, no schema ownership and `NOBYPASSRLS`.

The PostgreSQL health check does not become healthy until the application database
exists and the runtime role has its expected schema usage privilege. A one-shot
`migrate` service then runs with only the migration credential. The long-running
app receives only the runtime credential.

The app and migration containers use `read_only: true`, drop all capabilities
and set `no-new-privileges`; the app also uses a `tmpfs` at `/tmp` and
`stop_grace_period: 20s`. Compose is a development convenience, not Forge's
production orchestration contract.

```sh
docker compose up -d db

export FORGE_MIGRATION_DATABASE_URL='postgres://app_migrator:app_migrator@127.0.0.1:5432/<database>'
cargo run -- migrate

export FORGE_DATABASE_URL='postgres://app_runtime:app_runtime@127.0.0.1:5432/<database>'
cargo run

# Or start the complete development stack:
docker compose up --build
```

Applications generated with `-d none` omit the database and the migration
service while retaining the ordinary app container workflow.

### Pinning base images for release builds

The generated `Dockerfile` references base images by tag, and generated
Dependabot configuration proposes updates. For release or certification builds,
pin both stages by immutable digest and keep the tag for readability
([dependency policy](dependency-policy.md#supply-chain-gates)):

```sh
docker buildx imagetools inspect rust:1.98.1-slim-trixie
docker buildx imagetools inspect gcr.io/distroless/cc-debian13:nonroot
```

```dockerfile
FROM rust:${RUST_VERSION}-slim-trixie@sha256:<digest> AS builder
# ...
FROM gcr.io/distroless/cc-debian13:nonroot@sha256:<digest>
```

Dependabot's `docker` ecosystem can update a `tag@digest` reference while
keeping the tag. Scan the final image before publishing it.

## Configuration

Configuration comes only from environment variables and is validated before the
server starts. Invalid values stop startup with an error. **Unknown `FORGE_*`
variables are rejected** in every environment, so a typo such as `FORGE_BIDN`
fails startup instead of being silently ignored. Remove stale `FORGE_*` entries
from env files, Kubernetes manifests and unit files.

| Variable | Default | Meaning |
| --- | --- | --- |
| `FORGE_ENV` | `development` | `development`, `test` or `production`. Selects safety-sensitive defaults such as the log format. |
| `FORGE_BIND` | `127.0.0.1:3000` | Listen socket address. The container image sets `0.0.0.0:3000`. |
| `FORGE_REQUEST_TIMEOUT_SECS` | `30` | Maximum time for one request, including reading the request body. A response write that makes no progress for this long (at least 30 seconds) ends the connection. |
| `FORGE_SHUTDOWN_GRACE_SECS` | `15` | Drain deadline for in-flight requests after SIGTERM/SIGINT. |
| `FORGE_MAX_BODY_BYTES` | `1048576` | Maximum request body size in bytes. |
| `FORGE_MAX_CONNECTIONS` | `10000` | Maximum concurrent connections. Extra connections wait for a slot (backpressure) instead of being served without bound. |
| `FORGE_LOG` | `info` | `tracing` filter directive, for example `info,shop=debug`. |
| `FORGE_LOG_FORMAT` | `json` in production, `text` otherwise | `json` or `text`. |
| `FORGE_DATABASE_URL` | none | Least-privilege PostgreSQL runtime connection URL. Treated as a secret and redacted from ordinary diagnostics. |
| `FORGE_MIGRATION_DATABASE_URL` | none | PostgreSQL schema-owner/migration URL used only by `migrate`, `rollback` and application commands such as `setup` and `seed`. Treated as a secret and redacted. |
| `FORGE_SETUP_PASSWORD` | none | Owner password read only by `setup` when it is neither typed nor piped with `--password-stdin`. Never set it for the serving process. |
| `FORGE_RATE_LIMIT_MAX_KEYS` | `100000` | Live buckets of the in-memory rate limiter, `1` to `10000000` ([rate limiting](rate-limiting.md)). |
| `FORGE_CACHE_MAX_ENTRIES` | `10000` | Entry bound of every cache ([caching](caching.md)). |
| `FORGE_CACHE_MAX_BYTES` | `67108864` | Memory bound of the in-memory cache; at least `FORGE_CACHE_MAX_VALUE_BYTES` plus `4096`. |
| `FORGE_CACHE_MAX_VALUE_BYTES` | `1048576` | Largest cached value, at most `8388608`. |
| `FORGE_CACHE_DEFAULT_TTL_SECS` | `3600` | Time to live of cache writes that pass none, `1` to `31536000`. |
| `FORGE_STORAGE_ROOT` | none | Directory of the disk storage adapter; storage is unused when unset. In `production` it must be an absolute path on a persistent volume ([storage](storage.md)). |
| `FORGE_STORAGE_SIGNING_KEY` | none | HMAC key of signed links: unpadded base64url of 32 to 1024 random bytes. Redacted. **Required in `production` whenever `FORGE_STORAGE_ROOT` is set**; there is no default key. |
| `FORGE_STORAGE_MAX_UPLOAD_BYTES` | `524288` | Largest uploaded file, at most `1073741824`; when storage is configured it must not exceed `FORGE_MAX_BODY_BYTES`. |
| `FORGE_STORAGE_LINK_TTL_SECS` | `900` | Default lifetime of signed links, `1` to `604800`. |
| `FORGE_STREAMS_KEY` | none | HMAC key of signed live-update stream names: base64 (standard or URL-safe, padded or not) of at least 32 random bytes, as `openssl rand -base64 32` prints; the same on every replica. Redacted. **Required in `production` by applications that serve live updates** (`scaffold --broadcast`); development and test use a random per-process key ([views](views.md#live-updates)). |

Logs go to standard output, and the platform (Docker, Kubernetes, journald)
collects them. The HTTP layer also enforces a header-read timeout and adds
`X-Content-Type-Options: nosniff`, `X-Frame-Options: DENY` and a UUIDv7
`x-request-id` to responses. Transient `accept` errors, such as running out of
file descriptors, do not stop the server.

Do not put secrets in the image or in build arguments. Both database URLs contain
credentials and must come from the deployment platform's secret mechanism.
Forge's typed configuration redacts them from normal Debug/serialization output,
but adapters must still avoid logging exposed URLs.

Do **not** inject `FORGE_MIGRATION_DATABASE_URL` into the long-running serving
process. Run migrations as a separate deployment job/task with the migration
credential, then start the application with only `FORGE_DATABASE_URL`. The
runtime database role should be `NOSUPERUSER`, `NOCREATEDB`, `NOCREATEROLE`,
`NOBYPASSRLS`, and should not own application schemas. See
[ADR 0011](adr/0011-postgresql-runtime-migration-roles-and-rls-context.md).

## Authentication cookies and TLS

Generated authentication infrastructure serializes the server-side bearer as
`__Host-forge_session` with `Secure`, `HttpOnly`, `Path=/`, no `Domain`
attribute and `SameSite=Lax`. Client-facing authentication therefore requires
an HTTPS origin. A reverse proxy/load balancer may terminate TLS before forwarding
plain HTTP on a trusted private hop; the application still emits the Secure
cookie because the browser-facing origin is HTTPS.

Do not weaken production behavior by removing `Secure` or the `__Host-` prefix
to make browser login work over arbitrary development HTTP. Use an HTTPS local
proxy/origin when testing the full browser flow.

Full-stack applications also set the `__Host-forge_csrf` and
`__Host-forge_flash` cookies (same attributes, ADR 0023), so their forms need
the same HTTPS origin; browsers make an exception for `http://localhost` and
`http://127.0.0.1` during development. Behind a proxy, keep the browser's
`Host` (or rely on browsers sending `Sec-Fetch-Site`): the CSRF layer refuses
requests whose `Origin` does not name the request's `Host`.

Cookie-authenticated POST/PUT/PATCH/DELETE requests must also send exactly one
`x-csrf-token` header (or, in full-stack applications, the `csrf_token` form
field) containing the independent CSRF token issued with the session. The HttpOnly bearer cookie and CSRF token are intentionally different
256-bit secrets. CORS credential support is not enabled implicitly; applications
that add cross-origin credentialed requests need an exact reviewed origin
allowlist.

## Health endpoints

These routes are always registered:

| Route | Use | Semantics |
| --- | --- | --- |
| `GET /health/live` | Liveness | The process is running and serving HTTP. It never depends on external systems. |
| `GET /health/ready` | Readiness | The instance can accept its configured work. Phase 1 has no external dependencies, so this reflects only the process itself. Later phases add checks for required dependencies. |
| `GET /health` | Summary | Overall health summary. |

Use liveness to decide when to restart and readiness to decide when to route
traffic. Do not point liveness at readiness: a dependency outage should take an
instance out of rotation, not restart it in a loop.

The access-controlled diagnostic behavior for `/health` described in the
[architecture](architecture.md#observability-and-health) is not part of Phase 1.
Assume anyone who can reach the port can reach all three routes.

The `healthcheck` subcommand (`shop healthcheck`) probes `GET /health/live` on
the loopback address of the configured port and exits `0` or `1`. It is meant
for the Docker `HEALTHCHECK` in shell-less images. Orchestrators that do their
own HTTP probing (Kubernetes) should use `httpGet` probes instead. Kubernetes
ignores the image `HEALTHCHECK`.

## Graceful shutdown

On `SIGTERM` or `SIGINT`, the application:

1. stops accepting new connections;
2. lets in-flight requests finish, for up to `FORGE_SHUTDOWN_GRACE_SECS`;
3. exits. A graceful shutdown exits with status `0`.

The supervisor's stop timeout must be **longer** than
`FORGE_SHUTDOWN_GRACE_SECS`, or the process is killed (`SIGKILL`) mid-drain:

| Platform | Setting | Default | Recommendation (grace 15 s) |
| --- | --- | --- | --- |
| Docker | `docker stop -t <secs>` | 10 s | `docker stop -t 20 <container>` |
| Compose | `stop_grace_period` | 10 s | `20s` (set by the generated `compose.yaml`) |
| Kubernetes | `terminationGracePeriodSeconds` | 30 s | `30` |
| systemd | `TimeoutStopSec` | 90 s (distribution default) | `30` |

Docker's default 10-second stop timeout is **shorter** than the default 15-second
grace, so pass `-t` to plain `docker stop` when long requests matter. If you
raise `FORGE_SHUTDOWN_GRACE_SECS`, raise every stop timeout above it as well.

## Read-only root filesystem

The application writes nothing to its filesystem in Phase 1. It runs with a
read-only root filesystem (`docker run --read-only`, Compose `read_only: true`,
Kubernetes `readOnlyRootFilesystem: true`). The generated Compose file mounts a
`tmpfs` at `/tmp` for libraries that expect a writable temporary directory. The
Kubernetes example below mounts an `emptyDir` at `/tmp` for the same reason.
Features that need persistent writes must declare an explicit volume.

## Kubernetes

A starting point for a Deployment. Replace the image with a digest-pinned
reference from your registry.

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: shop
spec:
  replicas: 2
  selector:
    matchLabels:
      app: shop
  template:
    metadata:
      labels:
        app: shop
    spec:
      terminationGracePeriodSeconds: 30   # > FORGE_SHUTDOWN_GRACE_SECS
      automountServiceAccountToken: false
      securityContext:
        runAsNonRoot: true
        runAsUser: 65532
        runAsGroup: 65532
        seccompProfile:
          type: RuntimeDefault
      containers:
        - name: shop
          image: registry.example.com/shop@sha256:<digest>
          ports:
            - name: http
              containerPort: 3000
          env:
            - name: FORGE_ENV
              value: production
            - name: FORGE_BIND
              value: 0.0.0.0:3000
            - name: FORGE_SHUTDOWN_GRACE_SECS
              value: "15"
          livenessProbe:
            httpGet:
              path: /health/live
              port: http
            periodSeconds: 10
            failureThreshold: 3
          readinessProbe:
            httpGet:
              path: /health/ready
              port: http
            periodSeconds: 5
            failureThreshold: 2
          securityContext:
            readOnlyRootFilesystem: true
            allowPrivilegeEscalation: false
            capabilities:
              drop: ["ALL"]
          volumeMounts:
            - name: tmp
              mountPath: /tmp
      volumes:
        - name: tmp
          emptyDir: {}
```

Removing a Pod from Service endpoints happens in parallel with `SIGTERM`, so a
few requests can still arrive after shutdown starts. If that matters, add a
short `preStop` delay. The image has no shell, so `exec` hooks such as
`sleep` are unavailable. Use the native `sleep` lifecycle action if your cluster
version supports it.

Set CPU and memory requests and limits from your own measurements. Forge does
not publish sizing guidance yet.

## systemd (bare metal / VMs)

Build the binary with `forge build` (`cargo build --release --locked`) and copy
`target/release/shop` to the host, for example `/usr/local/bin/shop`. The binary
links against glibc, so build on or for a distribution with a compatible glibc.
The container image does not have this constraint, because it builds and runs
on matching Debian 13 stages.

```ini
# /etc/systemd/system/shop.service
[Unit]
Description=shop (Forge application)
Wants=network-online.target
After=network-online.target

[Service]
Type=exec
ExecStart=/usr/local/bin/shop serve
Environment=FORGE_ENV=production
Environment=FORGE_BIND=127.0.0.1:3000
Environment=FORGE_SHUTDOWN_GRACE_SECS=15
KillSignal=SIGTERM
TimeoutStopSec=30
Restart=on-failure
RestartSec=2

# Hardening
DynamicUser=yes
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
PrivateDevices=yes
ProtectKernelTunables=yes
ProtectKernelModules=yes
ProtectKernelLogs=yes
ProtectControlGroups=yes
ProtectClock=yes
ProtectHostname=yes
RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX
RestrictNamespaces=yes
RestrictRealtime=yes
RestrictSUIDSGID=yes
LockPersonality=yes
MemoryDenyWriteExecute=yes
SystemCallArchitectures=native
CapabilityBoundingSet=
AmbientCapabilities=
UMask=0077

[Install]
WantedBy=multi-user.target
```

```sh
sudo systemctl daemon-reload
sudo systemctl enable --now shop
systemd-analyze security shop.service
```

Port 3000 is unprivileged, so no capabilities are needed. Put a TLS-terminating
reverse proxy or load balancer in front of the loopback bind. If you use an
`EnvironmentFile=`, it must contain only known `FORGE_*` keys.

See also the [CLI reference](cli.md), [threat model](threat-model.md) and
[test strategy](test-strategy.md).

## Live updates behind a proxy

Live updates (`forge generate scaffold --broadcast`, [ADR 0031](adr/0031-live-updates-over-server-sent-events.md))
are long-lived `text/event-stream` responses on `/streams` and
`/tenants/{tenant_id}/streams`. A reverse proxy must not buffer them (Forge
sends `X-Accel-Buffering: no` for nginx) and its idle timeout must exceed the
20 second heartbeat. Each open stream holds one connection slot
(`FORGE_MAX_CONNECTIONS`); streams end after 30 minutes and when shutdown
begins, and a client that stops reading is disconnected once a write makes no
progress for `FORGE_REQUEST_TIMEOUT_SECS` or 30 seconds, whichever is longer
(the floor keeps a short API request timeout from cutting off slow but live
downloads and streams). With several replicas on
PostgreSQL, the generated wiring fans updates out with `LISTEN/NOTIFY`, which
holds one database connection per replica and does not work through
PgBouncer in transaction pooling mode. Set the same `FORGE_STREAMS_KEY` on
every replica:

```sh
export FORGE_STREAMS_KEY=$(openssl rand -base64 32)
```

