# OpenAPI from typed routes

Forge describes an application's JSON API as an OpenAPI 3.1 document built
from the Rust types its routes declare, so the description cannot be edited
out of step with the code. Nothing is written by hand and nothing is
computed per request. The decision record is
[ADR 0039](adr/0039-openapi-from-typed-routes.md), the port of Forge-Go ADR
0043: same route, variable and commands.

```sh
forge new shop && cd shop && forge dev
curl http://127.0.0.1:3000/openapi.json             # development and test only, by default

forge openapi                                       # print the document
forge openapi --output openapi.json                 # write it, to commit and diff
forge openapi --check openapi.json                  # exit 1 when the committed file is stale (CI)

FORGE_ENV=production FORGE_OPENAPI_ENABLED=true ./target/release/shop   # serve it in production too
```

## Describe a type

Derive `OpenApiSchema` next to `Serialize`/`Deserialize`. The derive reads
serde's attributes, so the schema says what `serde_json` writes:

```rust
use forge::core::{Id, Timestamp};
use forge::openapi::OpenApiSchema;
use serde::{Deserialize, Serialize};

#[derive(Deserialize, OpenApiSchema)]
#[serde(deny_unknown_fields)]
pub struct NewPost {
    #[openapi(doc = "Shown in lists.", example = "Hello")]
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
}

#[derive(Serialize, OpenApiSchema)]
#[serde(rename_all = "camelCase")]
pub struct Post {
    pub id: Id<Post>,
    pub title: String,
    pub created_at: Timestamp,
}
```

| Rust | JSON Schema |
| --- | --- |
| `String`, `&str`, `bool` | `string`, `boolean` |
| `i8`, `i16`, `u8`, `u16` | `integer`, `int32`, with their ranges |
| `i32` / `i64`, `isize` / `u32`, `u64`, `usize` | `int32` / `int64` / `int64` with `minimum: 0` |
| `f32`, `f64` | `number` (`float`, `double`) |
| `forge::core::Id<T>` | `string`, `uuid`, the canonical UUIDv7 pattern |
| `forge::core::Uuid` | `string`, `uuid` |
| `forge::core::Timestamp` | `string`, `date-time` |
| `Option<T>` | nullable: `["T", "null"]`, or `anyOf [$ref, null]` |
| `Vec<T>`, `[T]`, sets | array (never null: serde writes `[]`) |
| `[T; N]` | array of exactly N items |
| `BTreeMap<K, V>`, `HashMap<K, V>` | object with `additionalProperties: V` (text or integer keys) |
| `Box<T>`, `Arc<T>`, `Rc<T>`, `&T`, `Cow<T>` | `T` |
| `serde_json::Value` | any value |
| a derived struct | `$ref: #/components/schemas/Name` (recursion ends there) |
| a newtype or `#[serde(transparent)]` struct | its member's schema |
| a unit-only enum | `string` with `enum` |

Objects list their members with `additionalProperties: false`. A member is
`required` unless it has `skip_serializing_if`; `Option` members without it
are required and nullable (serde writes `null`). Serde attributes the derive
follows: `rename`, `rename_all`, `skip`, `skip_serializing`,
`skip_serializing_if`, `flatten` (of a struct, a map, or an `Option` of
one), `transparent`. Decoding-only ones (`default`, `alias`,
`deny_unknown_fields`, `skip_deserializing`, `bound`) are accepted.
Attributes that change the shape in ways the derive cannot describe (`tag`,
`untagged`, `content`, `from`/`into`, `with`, data-carrying enum variants,
tuple structs, lifetime parameters) are a **compile error**, never a guess.

`#[openapi(...)]` options:

| On | Option | Effect |
| --- | --- | --- |
| member | `doc = "..."` | `description` |
| member | `example = "..."` | `examples` (parsed as JSON when it is JSON) |
| member | `format = "email"` | `format` |
| member | `required` | required even with `skip_serializing_if` |
| member | `nonnull` | not nullable even if an `Option` |
| member or type | `as = Type` | described as `Type` ("encodes like"); required with `serialize_with` |
| type | `name = "HealthReport"` | the component name |
| type | `doc = "..."` | the component's `description` |

Component names are the Rust names, with type arguments reduced to their
names (`Page<Post>` is `Page_Post`). Types that share a name are qualified
with their module (`v1.Item`, `v2.Item`), then their full path; names that
still collide (two equal `name`s) are a build error naming both types.

What the derive writes is the code you would write by hand, which is also
how to describe a type yourself:

```rust
use forge::openapi::{Field, ObjectSchema, OpenApiSchema, Schema, SchemaGenerator};

impl OpenApiSchema for Post {
    fn schema(generator: &mut SchemaGenerator) -> Schema {
        generator.component::<Self>(None, |generator| {
            let mut object = ObjectSchema::new();
            object
                .field::<forge::core::Id<Post>>(generator, "id", Field::new())
                .field::<String>(generator, "title", Field::new())
                .field::<forge::core::Timestamp>(generator, "createdAt", Field::new());
            object.into_schema()
        })
    }
}
```

## Describe a route

`typed` decodes the request, calls the function and writes its result, and
takes the description from the function's types, so the two cannot
disagree:

```rust
use forge::core::{ErrorCategory, ForgeError};
use forge::http::{Method, Request, StatusCode};
use forge::openapi::{Json, Operation, typed};

router.route(
    Method::POST,
    "/v1/posts",
    typed(
        StatusCode::CREATED,
        Operation::new().summary("Create a post").tag("posts").errors([409]),
        |_request: Request, Json(new): Json<NewPost>| async move {
            Ok::<_, ForgeError>(create_post(new).await?)
        },
    ),
)?;
```

`Json<T>` is decoded with `forge::web::decode_json` (`422 invalid_json`
when it does not decode); `NoBody` reads nothing. The `Ok` value is written
as JSON with the status, the `ForgeError` with `forge::web::error`. Validate
in the handler or its use case and return a `Validation` error (`422`).

An ordinary handler keeps working; `describe` attaches what it accepts and
answers:

```rust
use forge::openapi::{Operation, Param, Response, describe};

router.route(
    Method::DELETE,
    "/v1/posts/{id}",
    describe(
        destroy,
        Operation::new()
            .summary("Delete a post")
            .permission("posts:write")                     // session cookie, 401, 403, x-forge-permission
            .param(Param::path::<forge::core::Uuid>("id").doc("The post."))
            .param(Param::query::<i64>("version").required())
            .response(Response::empty(204))
            .errors([404, 409]),                           // answered with forge::web::error
    ),
)?;
```

Register the described handler itself, after any per-route guard: the
document reads the handler the router holds. Undescribed routes keep
working; `forge openapi` lists them on standard error and the document
leaves them out. Path parameters the route declares are documented as
strings unless a `Param::path` gives them a type; a `Param::path` the route
lacks is a build error.

Name the API in `src/bootstrap/mod.rs` (it defaults to the application's
name and version):

```rust
forge::App::new(env!("CARGO_PKG_NAME"))
    .version(env!("CARGO_PKG_VERSION"))
    .api_info(forge::openapi::Info::new("Shop API", "1.0.0").description("Orders and payments."))
```

## Errors and security

Every error answered with `forge::web::error`, and the `default` response,
is the `ErrorReport` component: `{"code", "category", "message",
"metadata"?, "retryable"}`. Statuses are added for what Forge answers on its
own:

| Declared | Adds |
| --- | --- |
| a request body (`Json<T>`, `Operation::request`) | `413` (`text/plain`, `FORGE_MAX_BODY_BYTES`), `422` (`invalid_json`) |
| `authenticated()` | `401` (`text/plain`), `security: sessionCookie` |
| `permission("posts:write")` | `401`, `403` (`text/plain`), `security: sessionCookie`, `x-forge-permission` |

A response with its own body and an error status goes in `response(...)`
(readiness answers `503` with a `HealthReport`; a guard that answers text,
`Response::text(503)`).

Build errors, never guesses: no response; a body on `204` or `304`; the same
status as a response and an error; an error outside 4xx/5xx; duplicate
operation IDs, parameters or members; methods OpenAPI 3.1 has no field for;
a typed handler whose status is not a 2xx with a body.

## Serving and the CLI

`GET /openapi.json` is served when `FORGE_OPENAPI_ENABLED` allows it:

| `FORGE_OPENAPI_ENABLED` | development, test | production |
| --- | --- | --- |
| unset | served | not served |
| `true` | served | served, with a warning at startup |
| `false` | not served | not served |

The document lists every described route, internal ones included, with the
permission each requires: publishing it in production is a decision
([threat model](threat-model.md#openapi-review)). It is built when the
server starts, so an inaccurate description stops startup. An application
that registers its own `GET /openapi.json` keeps it; Forge then does not
mount its document there.

`forge openapi` builds the application and runs its `openapi` command, like
`forge migrate`. The command composes the application without listening, so
it needs the configuration `serve` needs; database pools connect lazily, so
the database does not have to be running. The binary accepts the same
command:

```sh
cargo run -- openapi --output openapi.json
./target/release/shop openapi --check openapi.json
```

In code, `router.openapi(&info)` returns the `Document` (`json()`,
`paths()`, `undocumented()`, `validate_response(...)`).

## Contract tests

`Document::validate_response(method, path, status, content_type, body)`
checks a response against the document. `CheckResponses` is a router layer
for tests that checks every response of a documented route:

```rust
let mut router = Router::new();
app::adapters::http::status::routes(&mut router)?;
let document = router.openapi(&forge::openapi::Info::new("shop", "0.1.0"))?;
router.layer(forge::openapi::CheckResponses::new(document, |failure| {
    panic!("{failure}")
}));
// every request dispatched below is now also a contract test
```

Generated applications ship `tests/openapi.rs`, which fetches
`/openapi.json`, checks its paths and runs every JSON response through
`CheckResponses`; `forge generate controller ... --api` tests do the same.

## What generated applications describe

- `forge new` (full-stack and `--api`): `GET /v1/status` (`typed`) and the
  health routes (`health`, `healthLive`, `healthReady`; with a database,
  readiness also documents its `503` `HealthReport`).
- `forge generate controller NAME ACTION... --api`: each action, with its
  permission unless `--public`.

Not described yet: JSON resource routes (`forge generate scaffold` does not
support `--api` applications yet), HTML pages, forms and event streams. See
the follow-ups in ADR 0039.
