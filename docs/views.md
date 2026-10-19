# Server-rendered views

Status: implemented for generated applications. `forge new` is full-stack by
default ([ADR 0022](adr/0022-server-rendered-views.md)): HTML pages rendered on
the server from compile-time checked Askama templates, a layout, digested
assets embedded in the binary, Turbo and Stimulus through an import map, a
strict Content-Security-Policy, CSRF-protected forms with method override and
one-shot flash messages ([ADR 0023](adr/0023-form-csrf-flash-and-method-override.md)).
`forge new --api` generates the JSON-only shape instead.

The framework pieces live in the `forge-views` crate, re-exported as
`forge::views` with the `forge` feature `views`. They belong to the HTTP
adapter ring: domain and application code never import `forge::views` or
`askama`, and the generated `tests/architecture.rs` fails if they do.

`forge generate scaffold` (CRUD resources with views) is a later slice; the
APIs below are what scaffolded code uses.

## Getting started

```sh
forge new shop -d sqlite3          # or postgresql (default), mysql, mariadb, none
cd shop
cargo run                          # http://127.0.0.1:3000
```

Open `http://127.0.0.1:3000` in Chromium or Firefox (the CSRF and flash
cookies are `__Host-` cookies, which browsers accept on `https://` origins and
on `http://localhost` / `http://127.0.0.1`). The welcome page is made of:

```text
templates/layouts/application.html  <head>: CSRF meta tag, stylesheet, import map, application.js
templates/layouts/_flash.html       notice/alert partial
templates/welcome/index.html        {% extends "layouts/application.html" %}
src/adapters/http/mod.rs            site(): Site::load and layers; routes(): GET /
src/adapters/http/views/mod.rs      Site (assets, import map, CSP, layers) and Layout
src/adapters/http/views/welcome.rs  WelcomeIndex view model
assets/application.css              digested and embedded by build.rs
assets/application.js               imports Turbo and starts Stimulus
build.rs                            forge_views::build::embed_assets_with_crate_path("assets", "forge::views")
tests/http.rs                       layout, CSP, CSRF and asset tests
```

`src/bootstrap` registers the adapter in its `compose` function:
`crate::adapters::http::site(router)` loads the `Site` and registers it, and
`crate::adapters::http::routes(router, &site)` adds the pages.
`Site::register` adds the asset routes and three request layers, outermost
first: the Content-Security-Policy, CSRF protection (which also applies the
`_method` override) and flash messages.

### A page with a form

A contact form that validates its input, shows errors next to the field with
`422`, and redirects with a flash message. First the view model and template:

```rust
// src/adapters/http/views/messages.rs (add `pub mod messages;` to views/mod.rs)
use askama::Template;
use forge::views::FieldErrors;

use super::Layout;

#[derive(Template)]
#[template(path = "messages/new.html")]
pub struct NewMessage<'a> {
    pub layout: Layout<'a>,
    pub csrf_field: String,
    pub body: &'a str,
    pub errors: FieldErrors,
}
```

```html
<!-- templates/messages/new.html -->
{% extends "layouts/application.html" %}

{% block content %}
<h1>Contact us</h1>
<form action="/messages" method="post">
  {{ csrf_field|safe }}
  <label for="body">Message</label>
  <textarea id="body" name="body">{{ body }}</textarea>
  {% if let Some(error) = errors.get("body") %}<p class="flash-alert">{{ error }}</p>{% endif %}
  <button type="submit">Send</button>
</form>
{% endblock %}
```

Then the handlers in `src/adapters/http/mod.rs`, registered in `routes` like
the index (`let site = Arc::clone(&site)` in each closure):

```rust
use forge::http::StatusCode;
use forge::views::{FieldErrors, Flash, csrf_field, csrf_token, see_other};

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct MessageForm {
    body: String,
}

// GET /messages/new: message_form(&site, &request, "", FieldErrors::new(), StatusCode::OK)
fn message_form(
    site: &Site,
    request: &Request,
    body: &str,
    errors: FieldErrors,
    status: StatusCode,
) -> Response {
    let csrf_field = csrf_token(request)
        .map(|token| csrf_field(&token))
        .unwrap_or_default();
    forge::web::html_with_status(
        status,
        &NewMessage {
            layout: site.layout(request, "Contact us"),
            csrf_field,
            body,
            errors,
        },
    )
}

// POST /messages
fn create_message(site: &Site, request: &Request) -> Response {
    let form: MessageForm = match request.form() {
        Ok(form) => form,
        Err(failure) => {
            let errors = FieldErrors::from(&failure);
            return message_form(site, request, "", errors, StatusCode::UNPROCESSABLE_ENTITY);
        }
    };
    if form.body.trim().is_empty() {
        let mut errors = FieldErrors::new();
        errors.add("body", "can't be blank");
        return message_form(site, request, &form.body, errors, StatusCode::UNPROCESSABLE_ENTITY);
    }
    // Call the use case with `form.body` here.
    let Ok(mut response) = see_other("/") else {
        return Response::empty(StatusCode::INTERNAL_SERVER_ERROR);
    };
    if Flash::new().with_notice("Message sent.").set_on(&mut response).is_err() {
        return Response::empty(StatusCode::INTERNAL_SERVER_ERROR);
    }
    response
}
```

What happens on submission: `CsrfProtection` compares the `csrf_token` field
with the `__Host-forge_csrf` cookie before routing (`403` otherwise);
`Request::form` decodes the body (the `csrf_token` field never reaches
`MessageForm`); the `303 See Other` redirect carries the flash cookie; the
next page's layout reads the flash, renders it escaped and removes the cookie.
With Turbo the same form is submitted with `fetch` and the token travels in
the `x-csrf-token` header taken from the layout's meta tag.

## Rendering

```rust
use askama::Template;

#[derive(Template)]
#[template(path = "products/show.html")]
struct ProductPage<'a> {
    name: &'a str,
}

async fn show(_: forge::http::Request) -> forge::http::Response {
    forge::web::html(&ProductPage { name: "Desk" })
}
```

- `forge::web::html(&view)` (`forge::views::html`) answers `200` with
  `Content-Type: text/html; charset=utf-8`; `html_with_status(status, &view)`
  sets another status (for example `422` when a form is shown again).
- Templates whose extension is `.html` are HTML-escaped by default. `{{ value }}`
  never emits markup; raw output needs an explicit, greppable `|safe` on a
  value the code has already sanitized (the CSRF helpers and the import map
  are such values).
- A render failure answers `500` with an empty body. The error is logged with
  `tracing`; template error text is never sent to the client.
- Templates are compiled into the binary. Editing one triggers an incremental
  rebuild (`cargo run` again); a template error is a build error.

## Forms

### Decoding

`Request::form::<T>()` (and `forge::web::decode_form`, which maps failures to a
`422 invalid_form` `ForgeError` with the field in its metadata) decodes
`application/x-www-form-urlencoded` bodies:

- exactly one `Content-Type: application/x-www-form-urlencoded` header (an
  optional `charset` must be UTF-8); anything else, including
  `multipart/form-data`, is `UnsupportedMediaType`;
- the strict query-string grammar: `+` is a space, `%XX` must form UTF-8,
  empty segments, pairs without `=` and empty names are `Malformed`;
- at most `MAX_FORM_FIELDS` (1000) pairs; the whole body is already bounded by
  `FORGE_MAX_BODY_BYTES` (1 MiB by default);
- a name may repeat only for a sequence field (`Vec<T>`: checkbox groups,
  multiple selects); a repeated scalar field is `DuplicateField`;
- `bool` accepts `true`, `on` (a checked checkbox without `value`) and `1`, or
  `false` and `0`. An unchecked checkbox sends nothing: declare checkboxes
  `#[serde(default)] bool`;
- a blank value is `None` for an `Option<T>` field (a blank number input);
- unknown fields are rejected with `#[serde(deny_unknown_fields)]`; the
  framework fields `csrf_token` and `_method` are always skipped.

`FormError::kind()` and `FormError::field()` are safe to show; `Display` is
for logs. `FieldErrors::from(&error)` turns one into a per-field message, and
`FieldErrors::add(field, message)` records validation failures from the
application ring (use `BASE_FIELD` for form-wide messages).
`Request::form_value(name)` reads one raw field, including the reserved ones.

### CSRF protection

`CsrfProtection` (a `forge::http::Middleware`, ADR 0023) protects every route
of the router it is added to:

- The token is 256 bits from the OS CSPRNG, kept in the `__Host-forge_csrf`
  cookie (`Secure`, `HttpOnly`, `SameSite=Lax`, `Path=/`, no `Domain`).
  `csrf_token(&request)` returns it for rendering; a browser without a valid
  cookie gets a new token and the cookie is set on that response, which is
  also marked `Cache-Control: no-store`. Responses that never read the token
  (assets, health) set no cookie.
- `csrf_meta(&token)` renders `<meta name="csrf-token" content="...">` for the
  layout (Turbo and `fetch` send it as `x-csrf-token`); `csrf_field(&token)`
  renders `<input type="hidden" name="csrf_token" ...>` for every non-GET form.
  Both escape the value and return markup: insert them with `|safe`.
- Every method except `GET`, `HEAD`, `OPTIONS` and `TRACE` must present
  exactly one well-formed token, in the `x-csrf-token` header or the
  `csrf_token` field of a form-encoded body (both, if present, must agree),
  equal to the cookie's token (constant-time comparison). The query string is
  never read; repeated or malformed values are refused.
- Before that, a browser-declared cross-site request is refused:
  `Sec-Fetch-Site` must be `same-origin` or `none`, or, without it, `Origin`
  must be `http(s)://<Host>`.
- Refusals are `403 Forbidden` with a generic body; the reason is logged
  (`CsrfError`) with the request ID and never the token.
- `csrf_verified(&request)` returns the `CsrfVerified` proof for a verified
  unsafe request.

For signed-in users, generated database applications keep the session-bound
check of [ADR 0014](adr/0014-argon2-session-cookie-and-csrf.md):
`infrastructure::security::enforce_csrf` reads the token with
`presented_csrf_token` (header or form field, same rules). At login, set the
form cookie to the session's CSRF token with `csrf_cookie(&token)` so the
pages render it and both checks agree.

Every unsafe request through a protected router needs a token, JSON endpoints
included; machine clients send the header, and JSON-only services use
`forge new --api`.

### Method override

HTML forms can only submit `GET` and `POST`. A `POST` form may carry
`<input type="hidden" name="_method" value="delete">`. `CsrfProtection`
applies it after the token check and before routing, with
`method_override(verified, method, field)`:

1. only for `POST` (a `GET` with `_method`, in the query or a body, is never
   changed);
2. only `PATCH`, `PUT` and `DELETE`, ASCII case-insensitive; `GET`, `POST`,
   `TRACE`, unknown and empty values leave the request as `POST`;
3. only after CSRF validation (the `CsrfVerified` witness); a request with an
   invalid token is refused before the override is considered, and a repeated
   `_method` is refused.

## Flash messages

```rust
let mut response = forge::views::see_other("/products")?;
forge::views::Flash::new()
    .with_notice("Product created.")
    .set_on(&mut response)?;
```

`FlashLayer` delivers the message to the next request; the layout reads it
with `forge::views::flash(&request)` (generated `Site::layout` does this) and
the partial renders `notice()` and `alert()` escaped. Reading consumes it: that
response removes the `__Host-forge_flash` cookie. Requests that do not read it
(assets, probes) keep it. Each message is at most 512 bytes and the encoded
cookie at most 3 KiB (`FlashError`). The cookie is not signed (ADR 0023): do not
put secrets or authorization decisions in flash messages.

## Redirects and Turbo

Turbo requires `303 See Other` after a non-GET submission so the follow-up
request is a `GET`:

```rust
let response = forge::views::see_other("/products/1")?;
```

`see_other` accepts only a path that starts with a single `/`. Absolute URLs,
`//host`, `/\host`, whitespace, control, non-ASCII and backslash characters
are refused (`RedirectError`), which closes open redirects through a
user-supplied `next` parameter. Redirecting to an absolute URL requires an
explicit, configured origin:

```rust
forge::views::see_other_to_origin(url, "https://app.example.com")?;
```

The origin (scheme, host, port) must match exactly; URLs with credentials are
refused.

### Turbo Streams

```rust
use forge::views::{DomId, StreamAction, TurboStreamAction, dom_id, turbo_stream};

let target = DomId::new("products")?;
let row = TurboStreamAction::from_view(StreamAction::Append, target, &row_view)?;
let removed = TurboStreamAction::remove(dom_id::<Product>(id, "product")?);
let response = turbo_stream(&[row, removed]);   // text/vnd.turbo-stream.html
```

- `DomId` is validated as `[A-Za-z][A-Za-z0-9_:-]*` (at most 128 bytes), so it
  cannot close an attribute or open a tag; it is safe in `id="..."` and
  `target="..."`.
- `dom_id::<T>(id, prefix)` is Rails' `dom_id` for UUIDv7 ids:
  `{prefix}_{uuid}`. The prefix must start with a letter.
- Every action except `remove` needs a template; `remove` takes none.
  `from_view` renders an Askama view so user data inside the template is
  escaped. The template string passed to `new` is trusted, already-rendered
  HTML.

## Assets

Assets live in `assets/` (CSS, JavaScript, images, fonts). The generated
`build.rs` embeds them with content digests; the `forge-views` build
dependency has only the `build` feature (no HTTP or template graph):

```rust
// build.rs
fn main() {
    if let Err(failure) = forge_views::build::embed_assets_with_crate_path("assets", "forge::views")
    {
        eprintln!("asset embedding failed: {failure}");
        std::process::exit(1);
    }
}
```

It writes `$OUT_DIR/forge_assets.rs`, which defines
`FORGE_ASSETS: &[forge::views::AssetEntry]` (`embed_assets("assets")` names
`forge_views::AssetEntry` for applications that depend on `forge-views`
directly). The generated `views/mod.rs` includes it and serves it together with
the vendored Hotwire modules:

```rust
include!(concat!(env!("OUT_DIR"), "/forge_assets.rs"));

let assets = AssetSet::from_tables(&[FORGE_ASSETS, vendor::HOTWIRE_ASSETS])?; // prefix "/assets"
assets.register(&mut router)?;
assets.path("application.css");   // Some("/assets/application-0123456789abcdef.css")
```

Build-time rules (errors name the offending path):

- names starting with `.` (files and directories) are ignored;
- symlinks must resolve to a file inside the asset root; escaping links,
  dangling links and links to directories fail the build;
- names use only ASCII letters, digits, `.`, `_` and `-`;
- extensions must be one of `css`, `js`, `mjs`, `map`, `svg`, `png`, `jpg`,
  `jpeg`, `gif`, `webp`, `ico`, `woff2`, `txt`;
- bounded work: at most 4096 files, 32 MiB per file, 16 directory levels.

The digest is SHA-256 of the contents (`sha2`); the URL carries the first 16
hex characters (`app-<16 hex>.css`) and the `ETag` is the full digest. The
output is sorted, so the table is deterministic. Editing an asset rebuilds the
binary on the next `cargo run`.

`AssetSet::register` adds one exact `GET` route per manifest entry. There is
no wildcard route, so `..`, percent-encoded separators (`%2e`, `%2f`),
undigested names, wrong digests, directory URLs and paths outside the prefix
match nothing and answer `404`. `HEAD` answers `405`. Responses carry:

| Header | Value |
| --- | --- |
| `Content-Type` | by extension (closed allowlist) |
| `Cache-Control` | `public, max-age=31536000, immutable` |
| `X-Content-Type-Options` | `nosniff` |
| `ETag` | `"<sha256 hex>"`; a matching `If-None-Match` (including `W/` and `*`) answers `304` with no body |
| `Content-Security-Policy` | SVG only: `default-src 'none'; style-src 'unsafe-inline'; sandbox` |

`AssetSet::new` and `AssetSet::from_tables` re-validate the tables (legal
names, allowlisted extension, well-formed digest, digested name consistent
with the digest, no duplicate logical name across tables), so a hand-edited or
corrupted table fails at startup, not at request time.

## Import map, Turbo and Stimulus

Turbo 8.0.23 and Stimulus 3.2.2 are vendored unmodified with pinned SHA-256
digests and license notices (`crates/forge-views/vendor/`, see
`forge_views::vendor`); no Node toolchain or CDN is involved.
`vendor::HOTWIRE_ASSETS` serves them as `vendor/turbo.js` and
`vendor/stimulus.js`:

```rust
let map = ImportMap::builder()
    .pin("@hotwired/turbo", vendor::TURBO_ASSET)
    .pin("@hotwired/stimulus", vendor::STIMULUS_ASSET)
    .build(&assets)?;

map.script_tag();    // <script type="importmap">{"imports":{...}}</script>
map.csp_source();    // 'sha256-<base64>'
```

Pins resolve to digested URLs; an unknown logical asset, duplicate or invalid
specifier is an error. The JSON escapes `<`, `>`, `&`, U+2028 and U+2029 so it
cannot end the script element. The element has no nonce; its SHA-256 (of the
exact body) is allowed by the policy. The layout puts `script_tag()` in
`<head>` before `<script type="module" src="/assets/application-<digest>.js">`,
whose `import` statements resolve through the map. To add a Stimulus
controller, put it under `assets/controllers/`, pin it in `Site::load`
(`.pin("controllers/hello", "controllers/hello_controller.js")`) and register
it in `assets/application.js`.

## Content-Security-Policy

```rust
let policy = forge::views::ContentSecurityPolicy::strict().with_import_map(&map);
router.layer(policy);                  // every response without its own CSP
// or, for one response: policy.apply(&mut response);
```

The default policy is:

```text
default-src 'self'; script-src 'self' 'sha256-<import map>'; style-src 'self';
img-src 'self' data:; object-src 'none'; base-uri 'self'; form-action 'self';
frame-ancestors 'none'
```

(on one line in the header). There is no `unsafe-inline` and no
`unsafe-eval`: inline event handlers, inline `<script>` and `style=""`
attributes are blocked by design. The policy also sets `Referrer-Policy:
strict-origin-when-cross-origin`. As a layer it skips responses that already
carry a policy (SVG assets keep their sandbox). Extra inline scripts can be
allowed only by hash (`with_script_hash`, which validates the base64 digest
so policy syntax cannot be injected). Turbo's progress bar injects an inline
stylesheet that the policy blocks; the generated `application.css` carries
those rules.

## Not in this slice

`forge generate scaffold` and `forge generate controller`, development-mode
asset serving from disk, `forge assets precompile`, `forge dev`, multipart
forms and file uploads, per-response CSRF token masking, and Turbo Streams over
SSE/WebSockets (Phase 4).
