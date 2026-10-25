# Deploying Forge Applications

Status: Phase 3 in progress. This guide covers the generated container image,
standalone binary, PostgreSQL development profile, least-privilege database roles, environment configuration,
health endpoints and graceful shutdown, and [`forge deploy`](#forge-deploy):
Kamal-style deploys to your own servers over SSH with zero-downtime switches
through forge-proxy ([ADR 0042](adr/0042-forge-deploy.md)). The examples use
an application named `shop`. Kubernetes and systemd snippets are starting
points to review, not certified configurations.

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
`stop_grace_period: 35s` (the HTTP grace plus the job drain). Compose is a development convenience, not Forge's
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

### Pinned base images

The generated `Dockerfile` pins both stages by the digest of their
multi-architecture index and keeps the tag for readability (the same pins as
`crates/forge-proxy/Dockerfile`; the supply-chain gate requires both to agree,
[dependency policy](dependency-policy.md#supply-chain-gates)). Docker builds
the digest, not the tag: when `RUST_VERSION` changes, look up the new tag's
digest and replace it in the same commit:

```sh
docker buildx imagetools inspect rust:1.98.1-slim-trixie --format '{{ .Manifest.Digest }}'
docker buildx imagetools inspect gcr.io/distroless/cc-debian13:nonroot --format '{{ .Manifest.Digest }}'
```

```dockerfile
FROM rust:${RUST_VERSION}-slim-trixie@sha256:<digest> AS build
# ...
FROM gcr.io/distroless/cc-debian13:nonroot@sha256:<digest> AS runtime
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
| `FORGE_HTTP_TRUSTED_PROXIES` | empty | Comma-separated CIDRs or addresses of reverse proxies whose `X-Forwarded-For` is believed; empty trusts nobody. Empty or malformed entries, `/0`, IPv4 prefixes shorter than `/8`, IPv6 prefixes shorter than `/16`, IPv4-mapped prefixes and zoned addresses stop startup; the error names the entry's position, never its text. See [client addresses behind a proxy](#client-addresses-behind-a-proxy). |
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
| `FORGE_I18N_DEFAULT_LOCALE` | the application's `DEFAULT_LOCALE` (`forge new --locale`, else `en`) | The locale requests that ask for no available locale get, and the one production shows for a missing translation. A locale such as `en` or `pt-BR`, spelled canonically; `config/locales/LOCALE.json` must exist or start-up fails ([i18n](i18n.md)). |
| `FORGE_STREAMS_KEY` | none | HMAC key of signed live-update stream names: base64 (standard or URL-safe, padded or not) of at least 32 random bytes, as `openssl rand -base64 32` prints; the same on every replica. Redacted. **Required in `production` by applications that serve live updates** (`scaffold --broadcast`); development and test use a random per-process key ([views](views.md#live-updates)). |
| `FORGE_TELEMETRY_*` | on, exporting nothing | Traces, metrics and log export ([observability](observability.md#configuration)). The ones operators set most: `FORGE_TELEMETRY_METRICS_ADDR` (separate Prometheus listener; in a container `0.0.0.0:9464` with `FORGE_TELEMETRY_METRICS_PUBLIC=true` and the port kept off the internet), `FORGE_TELEMETRY_EXPORTER=otlp` and `FORGE_TELEMETRY_LOGS_EXPORTER=otlp` with `FORGE_TELEMETRY_OTLP_ENDPOINT` (`https` in production unless `FORGE_TELEMETRY_OTLP_ALLOW_PLAINTEXT=true`), `FORGE_TELEMETRY_OTLP_HEADERS` (secret, redacted), `FORGE_TELEMETRY_SERVICE_NAME`, `FORGE_TELEMETRY_TRUST_TRACEPARENT` (only behind a gateway that sets or sanitizes `traceparent`). `FORGE_TELEMETRY_ENABLED=false` turns it off. |

Logs go to standard output, and the platform (Docker, Kubernetes, journald)
collects them. The HTTP layer also enforces a header-read timeout and adds
`X-Content-Type-Options: nosniff`, `X-Frame-Options: DENY` and a UUIDv7
`x-request-id` to responses. A handler that panics gets a `500` with the body
`internal server error` and its `x-request-id`; the panic message is logged
(`request handler panicked`, with the request id) and never sent to the
client, and the connection stays usable. `HEAD` is answered by the `GET`
route of the path without the body. Transient `accept` errors, such as
running out of file descriptors, do not stop the server.

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

Metrics are not on these routes: the Prometheus text is served only by the
separate `FORGE_TELEMETRY_METRICS_ADDR` listener
([observability](observability.md#prometheus-and-grafana)).

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
| Compose | `stop_grace_period` | 10 s | `35s` (set by the generated `compose.yaml`; covers the 25 s job drain) |
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

## Client addresses behind a proxy

Sign-in and password-reset throttles, `forge::ratelimit::by_client_ip` and
the anonymous live-update stream cap count the client address
`Request::client_ip()` returns
([ADR 0032](adr/0032-trusted-proxies-and-client-ip.md)). By default that is
the TCP peer, so behind a reverse proxy every client is the proxy and shares
one budget: 300 sign-in attempts from anyone in 15 minutes refuse every
sign-in. List the proxies that set `X-Forwarded-For`:

```sh
export FORGE_HTTP_TRUSTED_PROXIES=10.0.0.0/24,fd00:1::/64   # proxy CIDRs or addresses
cargo run
```

- Only when the peer is inside a listed network is the header read, from the
  right, skipping listed addresses; the first unlisted address is the client.
  A client that reaches the application directly cannot choose its address.
- An empty, malformed or non-address entry on that walk (a port, brackets,
  `for=`) and a chain of more than 32 listed hops fall back to the peer.
  `Forwarded` and `X-Real-IP` are never read.
- The value is validated at startup: malformed entries, `/0` and prefixes
  broader than IPv4 `/8` or IPv6 `/16` stop the process (bare addresses are
  unaffected). Empty, the default, trusts nobody.
- List only proxy networks, and keep the application port reachable only
  through them: anything inside a listed network can claim any client
  address and spend other clients' budgets.
- Your proxy must overwrite the header or append the address it received the
  connection from; a proxy that passes the client's header on unchanged lets
  clients choose their address.

`forge deploy` sets this variable to the `forge` Docker network automatically
(see [forge-proxy](#forge-proxy)); set it yourself on any other platform.

**nginx on the same host** (the systemd unit above binds to loopback):

```nginx
location / {
    proxy_pass http://127.0.0.1:3000;
    proxy_set_header Host $host;
    # At the edge, overwrite: clients cannot plant entries.
    proxy_set_header X-Forwarded-For $remote_addr;
    # Behind another load balancer you trust, append instead:
    # proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
}
```

```sh
# /etc/shop/shop.env (EnvironmentFile=): only nginx on loopback is trusted
FORGE_HTTP_TRUSTED_PROXIES=127.0.0.1,::1
```

**Docker Compose with nginx or Traefik in front**: give the network a fixed
subnet, publish no application port, and trust that subnet. Traefik replaces
`X-Forwarded-For` from untrusted clients with the connecting address by
default (its `forwardedHeaders.trustedIPs` decides whom it believes).

```yaml
services:
  app:
    image: shop:latest
    environment:
      FORGE_BIND: 0.0.0.0:3000
      FORGE_HTTP_TRUSTED_PROXIES: 172.30.0.0/24
    networks: [edge]            # no `ports:`: only the proxy reaches it
  proxy:
    image: traefik:v3.1         # or nginx with the configuration above
    ports: ["443:443"]
    networks: [edge]
networks:
  edge:
    ipam:
      config:
        - subnet: 172.30.0.0/24
```

The subnet includes the network's gateway (`172.30.0.1`), so processes on
the host reach the application from a trusted address; the host is
root-equivalent anyway. Every other container on `edge` is trusted too: keep
untrusted services on another network.

**Kubernetes**: trust only the ingress controller, not the cluster's whole
pod range: every pod inside a trusted range can choose its own
`X-Forwarded-For`. Prefer the controller's fixed addresses (a dedicated
node pool, or the node CIDR for a `hostNetwork` controller), and add a
`NetworkPolicy` so that only the ingress controller can reach the
application's port. Confirm that your controller appends or overwrites the
header with the connecting address (check its documentation for your
version; with ingress-nginx, enable `use-forwarded-headers` only when a
trusted load balancer sits in front of it).

Check what the application received without printing other variables:

```sh
docker inspect --format '{{range .Config.Env}}{{println .}}{{end}}' CONTAINER | grep '^FORGE_HTTP_TRUSTED_PROXIES='
```

A refused value stops startup with, for example,
`invalid value for FORGE_HTTP_TRUSTED_PROXIES: entry 2 is too broad: IPv4 prefixes shorter than /8 trust too many addresses; list the proxy networks instead`.

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

Terminate HTTP/2 at the proxy: over HTTP/1.1 a browser opens at most 6
connections per origin and every open page holds one for its stream. Stream
limits (16 per signed-in person, 1 000 per tenant, 10 000 in all) are counted
per process, and a client that vanishes without closing its connection keeps
its slot until a write fails, at most until the 30 minute maximum age
([ADR 0031](adr/0031-live-updates-over-server-sent-events.md#accepted-residuals-and-capacity-edges)).

### Rotating the stream signing key

Generated applications sign stream names with `FORGE_STREAMS_KEY` only.
Rotating it invalidates the stream URL of every page already open: on its
next reconnect (at the latest after 30 minutes) such a page gets `403` and
stops updating live until it is reloaded. Nothing else breaks, and pages
loaded afterwards work. Rotate during a quiet period:

```sh
export FORGE_STREAMS_KEY=$(openssl rand -base64 32)   # the new key, on every replica
# redeploy every replica with it; replicas with different keys refuse each other's pages
```

To keep open pages working across a rotation, build the signer yourself in
`src/adapters/http/streams.rs` with
`forge::broadcast::Signer::new(&new_key, &[&old_key])`, which signs with the
new key and still verifies the old one, keep the old key for at least the
token lifetime (24 hours), then remove it. A `FORGE_STREAMS_PREVIOUS_KEYS`
setting will come to Forge-Rust and Forge-Go together
([parity follow-ups](rails-parity.md#parity-follow-ups)).

## forge deploy

`forge deploy` puts the image on your own servers over SSH, Kamal-style, with
zero-downtime switches through **forge-proxy**
([ADR 0042](adr/0042-forge-deploy.md), the port of Forge-Go's ADR 0030).
Hosts need only an SSH server and Docker; your machine needs `ssh` and, to
build, the `docker` CLI. Commands, flags, `config/deploy.yml`, `.forge/secrets`,
the remote layout (`~/.forge/SERVICE`), the forge-proxy control protocol and
state file, and the exit codes are Forge-Go's: a project can move between the
frameworks without rewriting its configuration, and either framework's
`forge deploy` drives either forge-proxy image.

### From a fresh VPS

1. **Prepare each host** (Debian/Ubuntu shown) as root. Install Docker Engine
   from Docker's signed apt repository, following Docker's guide for
   [Ubuntu](https://docs.docker.com/engine/install/ubuntu/) or
   [Debian](https://docs.docker.com/engine/install/debian/), rather than
   piping a script into a shell:

   ```sh
   apt-get update && apt-get install -y ca-certificates curl
   install -m 0755 -d /etc/apt/keyrings
   . /etc/os-release   # ID is ubuntu or debian
   curl -fsSL "https://download.docker.com/linux/$ID/gpg" -o /etc/apt/keyrings/docker.asc
   chmod a+r /etc/apt/keyrings/docker.asc
   echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.asc] https://download.docker.com/linux/$ID ${UBUNTU_CODENAME:-$VERSION_CODENAME} stable" \
     > /etc/apt/sources.list.d/docker.list
   apt-get update && apt-get install -y docker-ce docker-ce-cli containerd.io
   ```

   Then create the deploy user and open the firewall:

   ```sh
   adduser --disabled-password --gecos '' deploy
   usermod -aG docker deploy          # docker access is root-equivalent
   install -d -m 700 -o deploy -g deploy /home/deploy/.ssh
   cp ~/.ssh/authorized_keys /home/deploy/.ssh/ && chown deploy: /home/deploy/.ssh/authorized_keys
   ufw allow 22,80,443/tcp && ufw enable
   ```

   Ports Docker publishes bypass `ufw`. Forge publishes only the proxy's 80
   and 443; application containers publish nothing and accessories bind
   `127.0.0.1` unless you give another address.

   Connect once with `ssh deploy@HOST` so its host key is in your
   `known_hosts`: Forge never disables host key checking. Use an SSH agent for
   passphrase-protected keys (commands run with `BatchMode=yes`).

2. **Build forge-proxy** once from a Forge checkout, push it and note the
   digest:

   ```sh
   docker build -f crates/forge-proxy/Dockerfile -t registry.example.com/acme/forge-proxy:0.1.0 .
   docker push registry.example.com/acme/forge-proxy:0.1.0
   docker buildx imagetools inspect registry.example.com/acme/forge-proxy:0.1.0   # Digest: sha256:...
   ```

   Forge-Go's signed forge-proxy image works too: the command line, control
   protocol and state volume are the same.

3. **Configure** the application:

   ```sh
   forge deploy init        # config/deploy.yml, .forge/secrets (0600), ignore entries
   $EDITOR config/deploy.yml .forge/secrets
   ```

4. **Point DNS** for `proxy.hosts` at the web hosts (needed for `ssl: true`).

5. **Deploy:**

   ```sh
   export REGISTRY_PASSWORD=... FORGE_DATABASE_URL=... FORGE_MIGRATION_DATABASE_URL=...   # referenced as $VAR in .forge/secrets
   forge deploy setup --dry-run   # review every remote command
   forge deploy setup             # docker check, network, proxy, accessories
   forge deploy                   # build, push, migrate, switch
   forge deploy details
   forge logs -f
   ```

### config/deploy.yml

```yaml
service: shop                                # container names, ~/.forge/shop
image: registry.example.com/acme/shop        # no tag: the version is the tag
registry:
  server: registry.example.com
  username: acme
  password: REGISTRY_PASSWORD                # a secret NAME, not the password
servers:
  web:                                       # routed through forge-proxy
    hosts: [192.0.2.10, 192.0.2.11]
  jobs:
    hosts: [192.0.2.11]
    cmd: ["jobs"]                            # arguments to the image entrypoint
    env: {clear: {JOBS_CONCURRENCY: "8"}}
env:
  clear: {FORGE_ENV: production}
  secret: [FORGE_DATABASE_URL, FORGE_MIGRATION_DATABASE_URL, FORGE_AUTH_MFA_KEY]
volumes: []                                  # e.g. shop-data:/data (SQLite)
proxy:
  image: registry.example.com/acme/forge-proxy@sha256:...
  hosts: [shop.example.com]
  ssl: true                                  # Let's Encrypt (accepts its terms)
  acme_email: ops@example.com
  app_port: 3000                             # FORGE_BIND of Forge-Rust applications
  healthcheck: {path: /health/ready, interval: 1s, timeout: 2s, healthy_count: 3, deploy_timeout: 60s}
  drain_timeout: 30s
  response_timeout: 30s
  max_body_bytes: 67108864
migrate: {enabled: true, cmd: ["migrate"], role: web}
accessories:
  db:
    image: postgres:18-alpine
    host: 192.0.2.20
    port: 5432                               # published on 127.0.0.1 only
    volumes: ["shop-postgres:/var/lib/postgresql"]
    env: {secret: [POSTGRES_PASSWORD]}
builder: {context: ., dockerfile: Dockerfile, platform: linux/amd64, args: {RUST_VERSION: "1.98.1"}}
retain: 5                                    # versions kept per role (2 to 100; the running one counts)
stop_timeout: 35s                            # > FORGE_SHUTDOWN_GRACE_SECS
boot_timeout: 60s                            # readiness wait for non-proxied roles
hooks: .forge/hooks
parallelism: 4                               # hosts handled at once
ssh: {user: deploy, port: 22, proxy_jump: bastion.example.com}
```

Decoding is strict: unknown or duplicate keys, a second YAML document,
durations that are not strings (`30s`), a tag in `image`, a password in
`registry.password`, host names starting with `-` or containing shell
characters, `retain` below 2, and more than one proxied role are errors.
`retain` counts the running version, so `retain: 1` would prune the previous
version after every deploy and leave nothing to roll back to; it is refused
(the default is 5). `servers.web` is proxied by default; set `proxy: true`
or `false` on a role to change that. An accessory `port` of `5432` or
`6543:5432` binds `127.0.0.1`; write
`IP:HOST_PORT:CONTAINER_PORT` to choose another address deliberately.
Defaults are Forge-Go's, including `app_port: 8080`; `forge deploy init`
writes `app_port: 3000` because Forge-Rust applications listen there.

The file is YAML, read by Forge's own strict reader (no YAML library is in the
dependency graph): block and flow mappings and sequences, plain and quoted
scalars and comments, the forms `forge deploy init` and Forge-Go's examples
use. Anchors, aliases, tags, block scalars (`|`, `>`), complex keys and
multi-line scalars are refused with the line number rather than guessed.

### Secrets

`.forge/secrets` contains `NAME=value` or `NAME=$VARIABLE` (`${VARIABLE}`)
lines. Passthrough copies a variable from your environment, so values can come
from a password manager or the CI secret store; an unset variable is an error.
Quotes are removed and nothing inside them is expanded. There is no command
substitution. The file must be mode `0600`. Values are sent to each host on
stdin and written to `~/.forge/SERVICE/env/ROLE.env` (mode `0600`, `umask
077`, atomic rename), which containers read with `--env-file`. They never
appear in command lines, dry runs or progress output.

`migrate` runs with the env file of `migrate.role`, so the role's
long-running containers receive `FORGE_MIGRATION_DATABASE_URL` too when it is
listed there. To keep the schema-owner credential out of them, leave it out of
`env.secret`, set `migrate.enabled: false` and run `migrate` as a separate
job with only that credential (see [Configuration](#configuration) and
[ADR 0011](adr/0011-postgresql-runtime-migration-roles-and-rls-context.md)).

Redaction of streamed output is best effort. `forge logs` and
non-interactive `forge app exec` replace the exact values from
`.forge/secrets` (4 characters or longer) line by line, but **output is
otherwise not redacted**: an encoded, partial or derived value passes through,
and interactive sessions (`forge app exec --interactive`, `forge app shell`)
and `docker logs` on the host are not filtered at all. Applications must not
print secrets; treat streamed output as sensitive.

### Commands

Every command reads `config/deploy.yml` (`--config`/`-c`) and `.forge/secrets`
(`--secrets`), and accepts `--dry-run`: mutating remote and local commands are
printed (`$`), stdin is described (`<`) but not shown, and read-only queries
(`?`, such as `docker ps`) still run, so the plan names the exact containers
that would be stopped or pruned.

```sh
forge deploy init --service shop                      # never overwrites
forge deploy setup --json                             # idempotent; warns when proxy.image is not pinned by digest
forge deploy                                          # version: abbreviated commit of a clean tree
forge deploy --version 3f2a9c1d07be --skip-build      # a tag already in the registry
forge deploy --dry-run                                # the plan, secrets redacted
forge deploy rollback 3f2a9c1d07be                    # retained containers, same health-gated switch
forge deploy prune                                    # stopped containers beyond retain, unused images
forge deploy details --json                           # containers, routing, lock, last deploys
forge deploy lock status
forge deploy lock acquire --message "maintenance window"
forge deploy lock release                             # --force for someone else's lock
forge deploy maintenance on                           # forge-proxy's 503 page with Retry-After
forge logs -f --role jobs
forge logs --since 1h --grep ERROR --host 192.0.2.10 --lines 50
forge app exec migrate status                         # one-off container of the running image
forge app exec --reuse /usr/local/bin/shop healthcheck   # inside the running container
forge app exec --interactive --host 192.0.2.10 migrate
forge app shell --image busybox:1.37                  # distroless images have no shell: a sidecar
```

| Command | Effect |
| ------- | ------ |
| `forge deploy init [--service NAME]` | Write a commented `config/deploy.yml` and `.forge/secrets`; add `.forge/secrets` to `.gitignore` and `.forge` to `.dockerignore`. The service defaults to the Cargo package name. Never overwrites. |
| `forge deploy setup [--json]` | Check Docker, create `~/.forge/SERVICE` (0700) and the `forge` network, log in to the registry, start forge-proxy (ports 80 and 443) and accessories. Idempotent. |
| `forge deploy [--version V] [--skip-build] [--json]` | Deploy; the version defaults to the abbreviated commit of a clean working tree. |
| `forge deploy rollback VERSION [--json]` | Restart the retained containers of VERSION on every host through the same health-gated switch. Migrations are not reverted. |
| `forge deploy prune [--json]` | Remove stopped containers beyond `retain` per role, and images no remaining container uses. Never touches running containers. |
| `forge deploy details [--json]` | Containers and versions per host and role, proxy routing and maintenance, lock holder, last deploys. |
| `forge deploy lock status\|acquire\|release [--message M] [--force]` | Inspect or hold the lock (for maintenance windows). `release` removes only a lock this performer holds, and only while its record is unchanged; it refuses one held by someone else (or without a record) unless `--force`. |
| `forge deploy maintenance on\|off` | Serve forge-proxy's static 503 page instead of the application. |
| `forge logs [-f] [--role R] [--host H] [--since 1h] [--grep REGEX] [--lines N]` | Stream `docker logs` of the current role container from every matching host, each line prefixed `host\|`. `--grep` (RE2 syntax) filters locally. Ctrl-C stops the remote `docker logs` processes too. Without `-f` at most `parallelism` hosts are read at once; `-f` keeps one stream (an SSH session) open per selected host, since a followed stream never ends. |
| `forge app exec [--role R] [--host H] [--interactive] [--reuse] CMD...` | Run CMD on one host: by default in a one-off container of the image the role runs, with its env file (CMD goes to the image entrypoint); `--reuse` runs inside the running container. |
| `forge app shell [--image IMAGE]` | Interactive shell over `ssh -t`; `--image busybox:1.37` starts a sidecar sharing the container's network and process namespaces, without the application's environment. |

Exit codes: `0` success, `1` failure (invalid configuration, SSH or remote
command failure), `2` invalid arguments (clap), `3` the lock is held (nothing
changed), `4` a new container failed its health check and every host serves
its previous version, `5` the migration failed before any switch, `6` a hook
failed, `7` a failure could not be fully reverted (run `forge deploy
details`). With `--json`, the report is printed on stdout also on failure,
with `ok`, `error` and `exit_code`.

### How a deploy switches without downtime

1. Take the lock on every host (`mkdir ~/.forge/SERVICE/lock`, atomic; the
   holder, time and message are recorded). It is released at the end, also on
   failure, but only if its record is still the deploy's own: a lock somebody
   else took after a `--force` release is left in place with a warning (or a
   note that it was already released). The record is written to a temporary
   file (created with `set -C`, so a leftover name is never truncated) and
   claimed with `ln`, which fails if the lock already has one, so a record is
   never seen half written. Only a successful `ln` wins: the hard link is
   created atomically by `link(2)`, which fails with `EEXIST` when
   `lock/info` exists, and `info` is removed only by a release whose record
   matches or by `--force`. A lock directory without a record file (the
   process was killed between the `mkdir` and the claim) older than a minute
   (`find -mmin +1`, so up to about two minutes)
   is taken over in place by the next deploy or `lock acquire` without
   `--force`, with a warning printed after the claim has succeeded; nothing
   is removed, so two deploys cannot both win it (a test forces the
   interleaving: a second acquire completes between the first one's
   staleness check and its claim). A younger one reports holder `unknown`,
   and so does a lock with an empty record file (left by earlier versions);
   `lock release --force` clears either at once. The release compares the
   record and then removes the directory as separate shell steps, so it is
   race-aware, not transactional: removing a foreign lock takes a force
   release and a new acquire inside the same few milliseconds. The scripts
   are Forge-Go's, byte for byte (the golden plans are shared).

   The lock needs `~/.forge` on every host to be on a file system that
   supports hard links (any local Linux file system does). Without them `ln`
   fails and every acquire reports the lock as held, which fails closed:
   nothing deploys. The remote acquire script writes `ln`'s own error to
   its stderr, but `forge deploy` reports only the held lock (holder
   `unknown`), so check the file system when a lock without a holder never
   clears; an empty lock directory may be left behind (clear it with `lock
   release --force`). On NFS, `link(2)` is atomic on a correctly configured
   server, but client caching can delay what other hosts see; keep
   `~/.forge` on local disk.
2. Run `pre-build`, build and push the image (unless `--skip-build`), run
   `pre-deploy`.
3. On all hosts in parallel (at most `parallelism`): registry login, `docker
   pull`, write the env files.
4. Run `migrate` once, in a one-off container on the first host of the
   migrate role. A failure stops here: nothing was started or switched.
5. On every host, for each role (the proxied one first), start the new
   container on the `forge` network with no published ports. For the proxied
   role, `forge-proxy deploy` probes `healthcheck.path` until it passes
   `healthy_count` times in a row, then switches traffic atomically and waits
   up to `drain_timeout` for requests still running on the old container.
   Other roles wait until Docker reports the container healthy (or running for
   three checks) within `boot_timeout`.
6. If any host fails, every switched host is routed back to its previous
   container, which is still running, and the new containers are stopped and
   removed. A role deployed for the first time keeps its healthy new
   container, since there is nothing to go back to.
7. When all hosts succeeded: stop the previous containers with `docker stop -t
   stop_timeout` (SIGTERM, then the application's graceful shutdown), prune
   beyond `retain`, append a JSON line (time, performer, version, containers)
   to `~/.forge/SERVICE/deploys.log`, run `post-deploy`.

Interrupting a deploy (Ctrl-C) during step 5 does not revert: forge-proxy
finishes a switch it has started, so hosts may run different versions. The
command exits `7`; check `forge deploy details` and deploy or roll back again.
Keep migrations compatible with the version still serving until the switch
(expand, then contract).

### Hooks

Executables in `.forge/hooks` (`hooks:` in the config) run locally from the
project directory: `pre-build` (before the build), `pre-deploy` (before any
host is touched; also on rollback) and `post-deploy` (after success). They
receive `FORGE_COMMAND` (`deploy` or `rollback`), `FORGE_SERVICE`,
`FORGE_VERSION`, `FORGE_IMAGE`, `FORGE_HOSTS` and `FORGE_ROLES` (comma
separated), `FORGE_PERFORMER` and, in `post-deploy`, `FORGE_RUNTIME` (seconds),
plus your environment. A non-zero exit aborts with exit code `6`; a hook file
that is not executable is an error, not skipped.

### forge-proxy

The proxy container (`forge-proxy`, built from `crates/forge-proxy`) listens
on 8080 and 8443 inside, published as 80 and 443, runs as uid 65532 and keeps
its routing state and ACME account and certificates in the `forge-proxy-data`
volume, so it resumes routing after a restart. It:

- routes by `Host` (one service per host name; a service without hosts gets
  unmatched requests) and answers unknown hosts with `404`;
- with `ssl: true`, obtains certificates from Let's Encrypt (ACME HTTP-01,
  answered on port 80) for exactly the configured hosts, ahead of time when
  the service is deployed and again 30 days before expiry, serves them with
  rustls (TLS 1.2 and 1.3) and redirects plain HTTP with `308`. A handshake
  for any other host, or before the certificate exists, fails rather than
  presenting a wrong certificate; a cached or issued certificate is used only
  if its key matches and its subject alternative names cover the host. The
  cache directory is kept `0700` (an existing one is restricted at start; a
  symbolic link or a directory of another user is refused) with `0600`
  files written atomically and synced to disk; temporary files of an
  interrupted write are removed at start. The cache uses autocert's file layout, so Forge-Go's proxy image
  reuses it and vice versa;
- is the internet edge: it drops any `Forwarded`, `X-Forwarded-*` and
  `X-Real-IP` headers the client sent, sets `X-Forwarded-For` to the
  connecting address only (replacing, never appending), `X-Forwarded-Proto`
  and `X-Forwarded-Host`, and keeps the original `Host`. The proxied role
  trusts that `X-Forwarded-For` because `forge deploy` sets
  `FORGE_HTTP_TRUSTED_PROXIES` to the `forge` network (see [Client addresses
  behind a proxy](#client-addresses-behind-a-proxy)). Running forge-proxy
  behind another load balancer is not supported;
- rejects bodies over `max_body_bytes` with `413`; reads bodies up to 1 MiB
  completely, with or without `Content-Length`, before contacting the
  application, and answers `408` if one does not arrive within 30 s; streams
  larger bodies, answering `408` when no data arrives for 30 s; gives the
  application `response_timeout` to send response headers;
- disconnects clients that hold connections without using them, after
  `-idle-timeout` (120 s by default): a keep-alive connection waiting for its
  next request (or after the 10 s `-read-header-timeout`, whichever is
  shorter), a response the client stops reading, and an upgraded
  (WebSocket) connection with no data in either direction. The application
  connection and the in-flight count are released with it, so a stalled
  client cannot hold a drain open. WebSocket applications should ping more
  often than that. There is no total time limit per connection: like
  nginx's `send_timeout`, a client that reads or sends a little in every
  interval keeps its connection, bounded only by the 10,000 connections per
  listener (no per-address limit);
- tunnels protocol upgrades (WebSocket) and counts them as in flight, so a
  switch drains them; refuses `CONNECT`;
- writes JSON access logs (service, host, method, path without query,
  status, bytes, duration, client IP, target) to `docker logs forge-proxy`;
- is controlled only through a Unix socket inside the container (directory
  0700, restricted at start if it already exists; socket 0600): `docker exec
  forge-proxy /usr/local/bin/forge-proxy status`. A routing state file it
  cannot read stops it from starting.

```sh
docker exec forge-proxy /usr/local/bin/forge-proxy status
docker exec forge-proxy /usr/local/bin/forge-proxy maintenance -service shop on
docker logs forge-proxy --since 10m
```

forge-proxy speaks HTTP/1.1 to clients and applications (no HTTP/2) and opens
one connection to the application per request. To upgrade it, change
`proxy.image`, then on each host run `docker rm -f forge-proxy` and `forge
deploy setup`: requests fail for the few seconds the proxy restarts.
Certificates from static files are not supported.
