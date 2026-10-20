# ADR 0022: Server-rendered views, Hotwire-compatible, full-stack by default

- Status: Accepted
- Date: 2026-10-14
- Refines: [ADR 0009](0009-rails-style-generated-delivery-assets.md)
- Refined by: [ADR 0023](0023-form-csrf-flash-and-method-override.md) (form CSRF, flash, method override), [ADR 0024](0024-scaffold-generator.md) (scaffold generator), [ADR 0031](0031-live-updates-over-server-sent-events.md) (live updates over Server-Sent Events)
- Decision owner: project owner (2026-10-14: "siga igual o Rails com views")

## Context

Rails is a full-stack framework: `rails new` produces HTML views, layouts,
partials, forms with CSRF protection, Hotwire (Turbo + Stimulus) through an
import map, and a digest-based asset pipeline, with `--api` for API-only
applications. Forge produced JSON APIs only, so building a product UI required
a second, separately deployed frontend.

Forge's architecture constrains the design: views are an HTTP adapter concern
(ADR 0002), output must be safe by default, deployment is a single binary
(ADR 0008), and generated code must be readable without macro archaeology.

## Decision

1. **Full-stack by default, `--api` to opt out.** `forge new` generates an
   application with views; `forge new --api` generates the current API-only
   shape. Both keep the hexagonal layout.
2. **Compile-time templates.** Views use [Askama](https://github.com/askama-rs/askama)
   (MIT OR Apache-2.0; subject to dependency admission review): Jinja-like
   syntax, templates type-checked at compile time against a typed view struct,
   and HTML auto-escaping on by default. A missing variable or a type error is
   a build error, not a runtime 500. Rendering is a pure function of the view
   struct, so views are unit-testable.
3. **Rails-like conventions, hexagonal placement.**
   `templates/layouts/application.html` (layout), `templates/<resource>/index.html`,
   `show.html`, `new.html`, `edit.html`, `_form.html` (partials). View structs
   live in `src/adapters/http/views/`; handlers build them from use-case results
   and return `forge::web::html(&view)`. Domain and application rings never see
   templates (enforced by the architecture test).
4. **Hotwire without a Node toolchain.** Turbo (Drive, Frames, Streams) and
   Stimulus are vendored as pinned ES modules (MIT, license notices retained)
   and loaded through an import map, as in Rails 8. Forge provides helpers for
   Turbo Stream responses (`text/vnd.turbo-stream.html`) and Turbo-aware
   redirects (303 after non-GET). Turbo Streams over SSE/WebSockets arrive with
   the Phase 4 pub/sub. Teams can still add a JavaScript bundler; Forge does not
   require one.
5. **Asset pipeline.** `assets/` holds CSS, JavaScript and images.
   `forge assets precompile` writes content-digested files plus a manifest;
   the build embeds them in the binary so deployment stays `./app`. The static
   handler serves only manifest entries (no directory listing, no path
   traversal), with `Cache-Control: public, max-age=31536000, immutable` for
   digested names. In development assets are served from disk.
6. **Secure defaults.**
   - HTML auto-escaping; raw HTML requires an explicit, greppable `|safe`
     on values the code has already sanitized.
   - A default Content-Security-Policy that permits only same-origin scripts,
     the import map by hash, and no inline event handlers.
   - Every non-GET form carries the Phase 3 CSRF token (hidden field and
     `<meta name="csrf-token">` for Turbo/fetch); unsafe requests without a
     valid token are rejected.
   - HTML forms may tunnel PATCH/PUT/DELETE through `POST` with a `_method`
     field, accepted only on POST and only after CSRF validation.
   - Flash messages are session-bound and one-shot.
7. **Generators.** `forge generate scaffold <Resource> field:type ...` produces
   what `rails g scaffold` produces: domain entity, repository port and adapter,
   use cases, migration (UUIDv7, optimistic version), HTML handlers and views
   (index/show/new/edit/_form), JSON endpoints when requested, routes and tests.
   `forge generate controller <Name> <action>...` produces handlers and views
   without persistence. The existing `generate page` and `generate resource`
   generators converge on this design instead of competing with it.
8. **Developer loop.** `forge dev` rebuilds and restarts on source, template or
   asset changes. Because templates are compiled, a template edit triggers an
   incremental rebuild; this is the accepted cost of compile-time checking.

## Consequences

- One deployable unit serves both the product UI and the API, as in Rails.
- Template mistakes fail the build; XSS-by-default is removed from generated
  code; CSRF is on for every generated form.
- Template edits need a rebuild (seconds with incremental compilation), slower
  than Rails' interpreted ERB reload.
- New dependencies (Askama, vendored Turbo/Stimulus) enter through admission
  review and the supply-chain gate; the vendored JavaScript is pinned and its
  digest recorded.
- The `--api` shape keeps current API-only users unaffected.
