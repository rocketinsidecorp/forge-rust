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

`forge generate scaffold` builds a whole CRUD resource on these APIs
([Scaffolding a resource](#scaffolding-a-resource),
[ADR 0024](adr/0024-scaffold-generator.md)).

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
assets/forge-rust-logo.png          the logo shown on the welcome page
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

## Scaffolding a resource

`forge generate scaffold` writes what the form walkthrough above writes by
hand, for a whole resource and every ring. In a PostgreSQL or SQLite
application (see [docs/cli.md](cli.md#forge-generate-scaffold) for every
option):

```sh
forge generate scaffold post title:string body:text published:boolean --global
```

It creates the migration pair, `Post` with its validation rules,
`PostRepository` (port), `PostService` (use cases), the SQLx repository,
handlers, view models, six templates and two test files, then prints the
wiring (add `--broadcast` for [live updates](#live-updates)). Add the
printed `pub mod` lines to the ring modules and the printed statements to the
end of `compose` in `src/bootstrap/mod.rs`, then:

```sh
cargo run -- migrate     # with FORGE_MIGRATION_DATABASE_URL
cargo test
```

The routes exist now but answer `403`: a global resource is wired with
`DenyAll` until you choose who may use it. Replace it in `compose` with an
implementation of `GlobalAccess`, for example one that admits signed-in
operators with the generated `authenticate_request`. It returns the principal
it admitted (`Ok(None)` only for anonymous callers of a public resource), so
that [live updates](#live-updates) limit each person's open streams rather
than everyone behind one address:

```rust
// src/adapters/http/access.rs (or a module of your own)
pub struct Operators {
    pub sessions: Arc<dyn SessionStore>,
}

#[async_trait]
impl GlobalAccess for Operators {
    async fn authorize(
        &self,
        request: &Request,
        _: &Permission,
    ) -> Result<Option<PrincipalId>, AccessDenied> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| UnixTimestamp::from_secs(elapsed.as_secs()))
            .map_err(|_| AccessDenied::Unavailable)?;
        let principal = authenticate_request(self.sessions.as_ref(), request, now)
            .await
            .map_err(|_| AccessDenied::Unauthenticated)?;
        enforce_csrf(self.sessions.as_ref(), request, principal)
            .await
            .map_err(|_| AccessDenied::Forbidden)?;
        // Your rule: may this principal read or change global rows?
        if is_operator(principal.principal_id()) {
            Ok(Some(principal.principal_id()))
        } else {
            Err(AccessDenied::Forbidden)
        }
    }
}
```

A `--tenant-scoped` resource lives under `/tenants/{tenant_id}/posts` and, on
PostgreSQL, is wired with `SessionTenantAccess` (session, session CSRF token,
membership, RBAC); the printed `RbacPolicy` grants `posts:read` and
`posts:write` to the `owner` role that `setup` gives the first member, and
nothing to other roles until you grant them. Its rows are isolated by a
`tenant_id` predicate in every statement and, on PostgreSQL, by forced
row-level security. A signed-out browser is sent to the sign-in page (next
section) and back.

What the generated pages do:

| Request | Answer |
| --- | --- |
| `GET /posts` | the first 25 posts in creation (UUIDv7) order, `?after=<id>` for the next page |
| `GET /posts/new`, `GET /posts/{id}/edit` | the form (`_form.html`) with the CSRF field; edit adds `_method=patch` and the `version` |
| `POST /posts` | `303` to the post with a flash notice, or the form again with `422` and an error next to each field |
| `PATCH /posts/{id}` (form `POST` + `_method`) | `303` with a notice; `422` with errors; `409` when the version is stale, with the submitted values, a message and the current version so saving again is a deliberate overwrite |
| `DELETE /posts/{id}` (form `POST` + `_method`) | `303` to the index; a stale version redirects back with an alert |
| an unknown or malformed `{id}` | the `not_found.html` page with `404` |
| a form without the page's CSRF token | `403` from `CsrfProtection`, before any handler |

Text that does not parse (`abc` for an integer, a malformed timestamp or
UUID) is reported together with the business-rule violations of the other
fields. `timestamp` fields use `<input type="datetime-local" step="1">` read
as UTC, in whole seconds: sub-second input is refused, because browsers blank
values with more than three fractional digits.
The templates are ordinary Askama files: edit them freely; the view models in
`src/adapters/http/views/posts.rs` give every value as display text.

## Sign-in pages

Full-stack PostgreSQL applications have the pages of
`rails generate authentication` in `src/adapters/http/authentication.rs`,
`src/adapters/http/views/authentication.rs`, `templates/sessions/` and
`templates/passwords/` (`forge generate authentication` adds them to an older
application; see [docs/cli.md](cli.md#forge-generate-authentication) and
[ADR 0026](adr/0026-authentication-pages-and-setup.md)):

```sh
FORGE_MIGRATION_DATABASE_URL=... cargo run -- migrate
FORGE_MIGRATION_DATABASE_URL=... cargo run -- setup --tenant acme --email owner@example.com
FORGE_DATABASE_URL=... cargo run
# http://127.0.0.1:3000/session/new?tenant=acme
```

They are ordinary handlers and templates: change the markup and wording
freely. Keep the security steps each handler documents: the
`csrf_verified` check, the throttle before any credential check, the uniform
failure message, `issue_persisted_session` plus the session and CSRF cookies
at sign-in, `return_path` on every return path, and `no-store` with
`Referrer-Policy: no-referrer` on the reset pages.

`authentication::routes` also registers `forge::views::SignInRedirect`: when
an access policy answers `401` to a client whose `Accept` header prefers
HTML (`forge::views::prefers_html`), the layer answers `303` to
`/session/new?return_to=<page>` instead (`GET`/`HEAD` keep the page; other
methods go to the sign-in page alone). JSON clients keep the `401`. To show a
sign-out button elsewhere, include `templates/sessions/_sign_out.html` in a
template whose view model has a `csrf_field`.

Reset links go through the `PasswordResetNotifier` in `compose`:
`forge::mail::password_reset_notifier` emails them once mail is configured
and otherwise delivers nothing; the page answers the same either way
([mail](mail.md#password-reset-emails)).

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

### Translations

Generated pages take their text from `config/locales`
([i18n](i18n.md), [ADR 0037](adr/0037-internationalization.md)). The
`Layout` every page receives carries the request's locale as `layout.i18n`:

```html
{% block title %}{{ layout.i18n.t("products.show.title") }}{% endblock %}
<p>{{ layout.i18n.t("products.price").with("amount", layout.i18n.number_with_precision(price, 2)) }}</p>
```

`t(...)` renders HTML (plain keys escaped, `_html` keys as trusted markup
with escaped arguments) and is never escaped twice; `.text()` gives the text
for a flash message. `LocaleNegotiation`, which `Site::register` installs
first, chooses the locale (cookie, `Accept-Language`, default) and adds
`Content-Language` and `Vary` to the responses that used it.

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
- Answer a form submission with streams only when Turbo asked for them:

  ```rust
  if forge::views::turbo_stream_requested(&request) {
      return forge::views::turbo_stream_with_status(StatusCode::UNPROCESSABLE_ENTITY, &[errors]);
  }
  forge::views::see_other("/products")?   // browsers, `*/*` and API clients
  ```

  `turbo_stream_requested` needs the `Accept` header to name
  `text/vnd.turbo-stream.html` itself with a non-zero quality (Turbo adds it
  to every form submission); stream responses carry `Vary: Accept`.

## Live updates

Pages update when someone else changes the data they show, like Rails'
`turbo_stream_from` with Action Cable broadcasts, over Server-Sent Events
([ADR 0031](adr/0031-live-updates-over-server-sent-events.md), the same names,
token format and limits as Forge-Go). The fastest way is the scaffold:

```sh
forge generate scaffold event name:string starts_at:timestamp --tenant-scoped --broadcast
forge generate scaffold note body:text --global --broadcast
export FORGE_STREAMS_KEY=$(openssl rand -base64 32)   # required in production, same on every replica
cargo run                                             # open /tenants/<tenant id>/events in two browsers
```

Add the printed wiring (it includes `Streams::register` once). Creating,
editing or deleting an event in one browser appends, replaces or removes its
row in the other, without a reload.

By hand, with the `broadcast` feature of `forge` (on by default in
`forge new` database applications, so the scaffold needs no `Cargo.toml`
edit; it compiles the API only and runs nothing until wired, see
[cli](cli.md#the-forge-dependency)):

```rust
use forge::broadcast::{Channel, EventStreams, Signer, StreamAccess, StreamViewer, publish};

// Composition root: broker, signer, endpoint.
let broker = composition.broadcast(forge::Fanout::Postgres)?;      // or Fanout::InProcess
let signer = Arc::new(Signer::from_config(composition.config())?); // FORGE_STREAMS_KEY
EventStreams::new(broker.clone(), signer.clone(), Arc::new(MyAccess))
    .register(composition.router(), "/streams")?;

// A page subscribes (render `|safe`): <turbo-stream-source src="/streams?stream=...">
let channel = Channel::for_tenant(context.tenant_id(), "products")?;
let source = signer.turbo_stream_from(&channel)?;

// After the change is stored, every subscriber receives the actions.
publish(broker.as_ref(), &channel, &[TurboStreamAction::remove(dom_id(id, "product")?)]).await?;
```

`MyAccess` implements `StreamAccess`: it authenticates the request and
returns `StreamViewer::member(&tenant_context)` for a tenant channel (built by
`TenantContext::authorize`, so it needs a session, an active membership and a
grant), `StreamViewer::principal(id)` or, for public global channels only,
`StreamViewer::anonymous()`. The handler then requires a tenant channel's
tenant to be the viewer's.

What protects a stream:

- The channel name in the page is signed (HMAC-SHA256 with
  `FORGE_STREAMS_KEY`, expiring after 24 hours); a browser cannot subscribe to
  a channel it was not rendered. Forged, tampered and expired tokens all get
  `403 stream_invalid`.
- A tenant channel is never served to another tenant (`403 stream_forbidden`);
  the access policy decides first (`401`, `403`, `503`).
- 16 open streams per principal (anonymous viewers per client address,
  `Request::client_ip`, so behind a proxy listed in
  `FORGE_HTTP_TRUSTED_PROXIES` each client has its own; IPv6 per `/64`) and
  1 000 per tenant (`429 stream_limit`). Generated streams count
  global-channel viewers by the principal their `GlobalAccess` returns, so
  signed-in people behind one proxy each have their own 16; 10 000 per process and
  1 000 per channel (`503 stream_unavailable`); 1 MiB per message.
- A subscriber that falls 64 messages behind is dropped, never waited for;
  its browser reconnects. Streams end after 30 minutes (re-authorizing on
  reconnect) and as soon as shutdown begins.
- Broadcast fragments are rendered without a request, so they carry no CSRF
  token or per-user content.

Delivery is at most once, without replay: a browser that was offline sees the
change on its next page load. Generated pages wait at most 2 seconds for the
broker (`PUBLISH_TIMEOUT` in `src/adapters/http/streams.rs`), then log the
update as failed, so a busy database pool never fails the stored change. `Fanout::Postgres` reaches every replica through
`LISTEN/NOTIFY` (256 KiB per message); MySQL, MariaDB and SQLite refuse it and
use `Fanout::InProcess`, which reaches the browsers connected to this process
only. Behind a proxy, see [deployment](deployment.md#live-updates-behind-a-proxy).

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

MFA and user management pages, JSON scaffolds and `forge generate controller`, development-mode
asset serving from disk, `forge assets precompile`, `forge dev`, multipart
forms and file uploads, per-response CSRF token masking, WebSocket transport,
replay of missed live updates and presence.
