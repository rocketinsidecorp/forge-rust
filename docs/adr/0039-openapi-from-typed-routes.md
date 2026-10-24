# ADR 0039: OpenAPI from typed routes

- **Status:** Accepted
- **Date:** 2026-10-20
- **Port of:** Forge-Go ADR 0043 (rocketinsidecorp/forge-rust#25)

The guide is [docs/openapi.md](../openapi.md).

## Context

Forge-Go describes its JSON routes as an OpenAPI 3.1 document built from the
Go types the routes declare (Forge-Go ADR 0043), served at `GET
/openapi.json` behind `FORGE_OPENAPI_ENABLED` and printed by the
application's `openapi [--output FILE | --check FILE]` command and `forge
openapi`. Its "Forge-Rust port" section asks for the same contract with Rust
idioms. Go reflects over types at startup; Rust has no reflection, so the
schema of a type must be produced at compile time by a trait implementation.

Constraints: descriptions must not drift from the code (a wrong description
is worse than none); the core model needs no HTTP dependency (hexagonal
rings); generated code must be understandable without reverse-engineering
macros; new dependencies must pass `cargo deny` and the
[dependency policy](../dependency-policy.md); the document must be stable
enough to commit and diff.

## Decision

1. **Schemas come from a Forge-owned trait and a Forge-owned derive.**
   `forge::openapi::OpenApiSchema` (`fn schema(&mut SchemaGenerator) ->
   Schema`, plus `flatten` for `#[serde(flatten)]`) is implemented by hand
   for the standard and Forge types and derived with
   `#[derive(OpenApiSchema)]` next to `Serialize`. The options were:

   | Option | Accuracy | Compile time | Supply chain | serde fidelity |
   | --- | --- | --- | --- | --- |
   | (a) trait, manual impls and a `macro_rules!` helper | members restated by hand drift from the struct | none added | nothing new | none: a declarative macro cannot read `#[serde]` attributes |
   | (b) Forge derive (`forge-macros`) | follows the struct and its serde attributes; refuses what it cannot describe | `syn` is already compiled for `serde_derive` | no new package (`syn` 2, `quote`, `proc-macro2` are in the locked graph) | the attributes that shape JSON, read as serde reads them |
   | (c) `utoipa` / `schemars` | good | a second large derive stack | new packages and their release cadence | good, but their output differs from Forge-Go's document and `schemars` renames colliding types instead of failing |

   (b) wins: it is accurate by construction, adds no package to the graph,
   produces exactly the document Forge-Go does, and its expansion is plain
   trait code (`ObjectSchema::field::<T>(generator, "name", Field::new()...)`,
   the calls a person writes by hand; the guide shows them). The derive
   follows `rename`, `rename_all` (all of serde's rules), `skip`,
   `skip_serializing`, `skip_serializing_if` (optional), `flatten`
   (structs, maps and `Option`s of them), `transparent` and newtypes; it
   accepts decoding-only attributes (`default`, `alias`,
   `deny_unknown_fields`, `skip_deserializing`, `bound`, ...) and refuses, at
   compile time, everything that changes the shape in a way it cannot say
   (`tag`, `untagged`, `content`, `from`/`into`, `with`/`serialize_with`
   without `#[openapi(as = Type)]`, data-carrying enum variants, tuple
   structs, lifetime parameters, different serialize and deserialize
   names). `#[openapi(...)]` refines a member (`doc`, `example`, `format`,
   `required`, `nonnull`, `as = Type`: the counterparts of Go's tags and
   `Typer`) or a type (`name`, Go's `Namer`; `doc`; `as`; `crate`).
2. **Mapping.** `Option<T>` is nullable (`["T","null"]`, or `anyOf` with a
   reference); a member is required unless it has `skip_serializing_if`;
   objects are `additionalProperties: false`. Integers carry `int32`/`int64`
   and ranges exactly as in Go (`i8`, `i16`, `u8`, `u16` are `int32` with
   bounds; `u32`, `u64`, `usize` are `int64` with `minimum: 0`); `f32`/`f64`
   are `float`/`double`; `forge::core::Id<T>` is `format: uuid` with the
   canonical UUIDv7 pattern; a bare `Uuid` is `format: uuid`;
   `forge::core::Timestamp` is `date-time`; `Version` is an `int64` of at
   least 1; `serde_json::Value` is any value; maps need text or integer
   keys (`MapKey`); unit-only enums are string enums.
3. **Components and determinism.** Every named struct or enum is a component
   referenced with `$ref`, which also ends recursion. Names are the Rust
   names with type arguments reduced to theirs (`Page<Post>` is
   `Page_Post`); names that collide are qualified with the module of each
   type, then with its path; names that still collide (two equal chosen
   names) are a build error naming both types. Keys are sorted, the JSON is
   indented with two spaces, ends in a newline and uses Go's string escaping,
   so the same routes and types give the same bytes, and the same bytes as
   Forge-Go where the descriptions are equivalent.
4. **Descriptions travel with the handler, opt-in.** `forge_http::Handler`
   gains `fn operation(&self) -> Option<&Operation>` (default `None`);
   `forge::openapi::describe(handler, Operation)` returns the handler
   unchanged in behavior, carrying its operation, and the router records it
   per route, so the document is built from the router's own table and
   undescribed routes are listed, never guessed. `Operation` is a builder
   with Go's fields (`id`, `summary`, `description`, `tag`, `deprecated`,
   `permission`, `authenticated`, `param`, `request`, `response`, `errors`);
   parameters are declared with `Param::path/query/header::<T>(name)`
   instead of a tagged struct. The operation ID defaults to Go's
   derivation (`getV1PostsById`).
5. **Typed handlers.** `forge::openapi::typed(status, operation, |request,
   body| async { ... })` takes `Json<T>` (decoded with
   `forge::web::decode_json_strict`: `application/json` only, and no member
   the schema of `T` does not list) or `NoBody`, writes the `Ok` value with
   `forge::web::json` and the `ForgeError` with `forge::web::error`, and
   derives the request body and the success response from its types. A
   status that is not a 2xx with a body makes the description invalid
   (a build error, never a panic).
6. **Errors and security as Forge-Rust answers them.** Every error the
   handler answers through `forge::web::error`, and the `default` response,
   is the `ErrorReport` component (`code`, `category`, `message`,
   `metadata`?, `retryable`), Forge-Rust's only JSON error shape. A request
   body implies `413` (the server's `FORGE_MAX_BODY_BYTES` refusal,
   `text/plain`) and `422` (`invalid_json`, `ErrorReport`); `permission` or
   `authenticated` imply `401` (`text/plain`, as Forge's guards answer it)
   and the `sessionCookie` security scheme (`apiKey` in the
   `__Host-forge_session` cookie); `permission` adds `403` (`text/plain`)
   and `x-forge-permission`. Those refusals (the server's `413`/`400` and
   the generated access policy's `401`/`403`/`503`) are written with
   `Response::text`, so they carry `Content-Type: text/plain;
   charset=utf-8` as declared; the generated contract tests found that they
   had none. The same build errors as Go: no response, a
   body on `204`/`304`, a status both declared and an error, an error
   status outside 4xx/5xx, a path parameter the path lacks, duplicate
   parameters, operation IDs or members, methods OpenAPI lacks.
7. **Contract tests.** `Document::validate_response(method, path, status,
   content_type, body)` checks a response against the published document
   (the JSON Schema subset the build emits, plus `enum`, lengths and
   `text/plain`), matching the route with the router's rule that static
   segments win. `forge::openapi::CheckResponses` is a router layer for
   tests that checks every response of a documented route (by the route
   the router matched) and reports mismatches; generated tests panic on
   them.
8. **Served only where safe.** `FORGE_OPENAPI_ENABLED` (`forge-config`,
   in `KNOWN_KEYS`, strict boolean): unset serves `GET /openapi.json` in
   development and test only, `true` everywhere with a startup warning in
   production, `false` never. The document is built once composition has
   finished (Rust routes are frozen before serving), so an inaccurate
   description stops startup with exit 1. An application that registered
   its own `GET /openapi.json` keeps it, and Forge logs that it did not
   mount its own. The document lists every described route with its
   permission ([threat model](../threat-model.md#openapi-review)).
9. **The binary writes it; the CLI mirrors `forge migrate`.** `openapi` is a
   reserved command of the application binary: it composes the application
   without listening (logs go to standard error), prints the document or
   writes it (`--output FILE`), or exits 1 when FILE differs (`--check FILE`,
   for CI), and lists undescribed routes on standard error. It needs the
   configuration `serve` needs; pools connect lazily, so no database has to
   be running. `forge openapi [--output FILE | --check FILE]` builds the
   application and runs that command like `forge migrate` (ADR 0036),
   resolving FILE where `forge` runs.
10. **Rings.** `forge-openapi` (the model, document and validator) depends
    on `forge-core`, `forge-macros` and `serde_json` only; the HTTP side
    (`describe`, `CheckResponses`, serving, health descriptions) is in
    `forge-http`; `typed` is in the `forge` facade, which owns
    `ForgeError`'s HTTP mapping. Generated architecture tests forbid
    `forge::openapi` in the domain and application rings.
11. **Generated applications.** `forge new` (full-stack and `--api`) serves
    `GET /v1/status` through `typed` and gets `tests/openapi.rs`, which
    fetches `/openapi.json`, checks its paths and runs every JSON response
    through `CheckResponses`. The framework's health routes describe
    themselves (`health`, `healthLive`, `healthReady`; readiness with a
    database also documents its `503` `HealthReport`). `forge generate
    controller ... --api` describes each action (summary, tag, permission
    and the access policy's `503`), and its tests run through
    `CheckResponses`.

## Consequences

- A fresh application has a valid OpenAPI 3.1 document with nothing to
  maintain by hand; client generation is left to standard tooling.
- `describe` descriptions can still be wrong about what a handler does;
  `typed` handlers are correct by construction, and contract tests catch
  drift for what the tests exercise.
- Adding `openapi` to the reserved binary commands makes an application
  command of that name a startup error.
- `forge-macros` is a proc-macro crate: it runs at compile time, does no I/O
  and adds no package; its expansion is covered by unit tests of the
  expansion and of every refusal.

## Deviations from Forge-Go

- `Vec`, sets and maps are not nullable: serde writes them as arrays and
  objects, never `null` (Go's nil slices and maps are `null`).
- Errors are `ErrorReport`, not `ErrorBody`/`ErrorDetail`; the implied
  statuses are what Forge-Rust answers (`413` text, `415` and `422` for a body
  instead of `400`/`413`/`415`; `401`/`403` as text), and the security
  scheme is the session cookie instead of `bearerAuth`: Forge-Rust has no
  bearer-token API authentication.
- A typed request is `Json<T>` or `NoBody`; there is no `Validate` hook:
  invalid JSON is already `422`, and the handler (or its use case) returns a
  validation `ForgeError`.
- Parameters use a builder (`Param::query::<T>("limit")`) instead of a
  tagged struct; `pagination.Query` has no counterpart yet (Forge-Rust has
  no pagination type).
- `forge openapi` inherits the caller's environment like the other Rust
  lifecycle commands instead of loading `.env.development`, and needs no
  running database.
- Not applicable yet: Forge-Rust's `generate scaffold` refuses `--api`
  applications (no JSON resource routes to describe), and Forge-Rust's
  identity is HTML pages, so there is no `GET /v1/auth/session` or JSON
  sign-in, sign-out, MFA or recovery handler to describe.

## Follow-ups

- Describe JSON resource routes when `forge generate scaffold` supports
  `--api` applications (Forge-Go's `createPost`, `listPosts`, `getPost`,
  `updatePost`, `deletePost`), and JSON identity routes when Forge-Rust
  ships them.
- Internally and adjacently tagged and untagged enums in the derive
  (`oneOf`), and `time`/`chrono` types if a Forge crate adopts them.
- A cross-framework golden test that an equivalent Forge-Go and Forge-Rust
  application print identical bytes.
- A `--check` step in the generated CI workflow once applications commit
  their `openapi.json`; request validation from the document; `enum`,
  length and pattern constraints from explicit attributes.
