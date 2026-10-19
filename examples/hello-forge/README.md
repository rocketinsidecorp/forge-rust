# hello-forge

A [Forge](https://github.com/fabriciobonjorno/forge-rust) application organized
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
([ADR 0022](https://github.com/fabriciobonjorno/forge-rust/blob/main/docs/adr/0022-server-rendered-views.md)):

```text
templates/layouts/application.html  layout: CSRF meta tag, stylesheet, import map
templates/layouts/_flash.html       one-shot notice/alert messages
templates/welcome/index.html        the page served at /
src/adapters/http/views/            view models checked against the templates at compile time
assets/                             CSS and JavaScript, digested and embedded by build.rs
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

## Container

```bash
docker build -t hello-forge .
docker run --rm -p 3000:3000 hello-forge
docker compose up --build
```

The image is built with `cargo build --release --locked`, so commit
`Cargo.lock`. The runtime stage is distroless and runs as uid 65532; the Docker
health check runs `hello-forge healthcheck`.
