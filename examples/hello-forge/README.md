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

## Development

```bash
cargo run                     # serves on http://127.0.0.1:3000
curl http://127.0.0.1:3000/health/live
forge check && forge lint && forge test
```

Binary commands: `serve` (default), `healthcheck`, `version`, `help`.

Configuration uses `FORGE_*` environment variables; unknown `FORGE_*` keys are
rejected at startup. Run `cargo run -- help` for the full list.

## Container

```bash
docker build -t hello-forge .
docker run --rm -p 3000:3000 hello-forge
docker compose up --build
```

The image is built with `cargo build --release --locked`, so commit
`Cargo.lock`. The runtime stage is distroless and runs as uid 65532; the Docker
health check runs `hello-forge healthcheck`.
