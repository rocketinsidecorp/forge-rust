# ADR 0023: Form CSRF, flash messages and method override for HTML applications

- Status: Accepted
- Date: 2026-10-19
- Refines: [ADR 0022](0022-server-rendered-views.md), [ADR 0014](0014-argon2-session-cookie-and-csrf.md)
- Refined by: [ADR 0026](0026-authentication-pages-and-setup.md) (sign-in binds the form token to the session)

## Context

ADR 0022 makes `forge new` full-stack: HTML forms carry a CSRF token,
`_method` tunnels `PATCH`/`PUT`/`DELETE` through `POST` only after CSRF
validation, and flash messages are "session-bound and one-shot". ADR 0014
binds the CSRF token to a server-side session and stores only its SHA-256
digest, enforced on the `x-csrf-token` header.

Three facts make ADR 0014 alone insufficient for server-rendered pages:

1. Anonymous forms exist before any session: sign-up, login, password reset,
   contact forms. They need CSRF protection too.
2. A page must render the raw token in every form and in the
   `<meta name="csrf-token">` tag, but the server keeps only the digest of the
   session token. Something must hold the raw token between requests.
3. Generated applications with `-d none`, and those on MySQL, MariaDB and
   SQLite (ADR 0021), have no persistent session store at all.

Method override changes the routed method, so it must happen before routing,
and only after the token check. The framework had no request layer that runs
before the route table, no form body decoder, and no flash.

## Decision

### Request layers

`forge_http::Router::layer` adds `Middleware` that runs before routing for every
request (including unmatched paths), outermost first. `Request` gains typed
extensions (per-request values handed from a layer to the handler) and
`set_method` (for a layer that rewrites the method before routing).
`Request::form` decodes `application/x-www-form-urlencoded` bodies with the
strict query grammar plus HTML form semantics and never passes the reserved
`csrf_token` and `_method` fields to the application's type.

### CSRF: synchronizer token in a host-only double-submit cookie

`forge_views::CsrfProtection` is the enforcement path for HTML forms and
Turbo/`fetch` requests:

- **Token.** 256 bits from the operating-system CSPRNG (`getrandom`), in the
  canonical `CsrfToken` encoding (64 lowercase hex characters), the same
  format as ADR 0014's session CSRF secret.
- **Storage.** The `__Host-forge_csrf` cookie: `Secure`, `HttpOnly`,
  `SameSite=Lax`, `Path=/`, no `Domain`, no expiry (browser session). The
  `__Host-` prefix means conforming browsers accept the cookie only from a
  secure origin, without `Domain`, so a sibling subdomain or a plain-HTTP
  network attacker cannot plant a token they know (the classic weakness of
  naive double-submit cookies). `HttpOnly` because page scripts read the token
  from the meta tag, never from the cookie.
- **Issuance.** Lazily: a handler that renders a form calls
  `forge_views::csrf_token(&request)`; if the browser has no valid cookie a
  token is generated and the layer adds `Set-Cookie` to that response only, and
  marks it `Cache-Control: no-store` (unless the handler set a policy). Assets
  and health probes never receive the cookie, so `public, immutable` responses
  never carry `Set-Cookie`.
- **Verification.** Every method except `GET`, `HEAD`, `OPTIONS` and `TRACE`
  must present exactly one well-formed token in the `x-csrf-token` header or in
  the `csrf_token` field of a form-encoded body (if both are present they must
  be equal), and it must equal the cookie's token. Comparison is constant time
  (`subtle`). The query string is never read. Repeated headers, fields or
  cookies, malformed values and malformed form bodies are refusals, not
  guesses. Multipart bodies are not parsed; such requests send the header (as
  Turbo does).
- **Cross-site check (defense in depth).** Before the token check, a request
  whose `Sec-Fetch-Site` is anything but `same-origin` or `none` is refused
  (including `same-site`: a sibling subdomain is not trusted). Without
  `Sec-Fetch-Site`, an `Origin` header must be `http(s)://<Host>`; `Origin:
  null` is refused. Requests with neither header (non-browser clients, very old
  browsers) still need the token. This is the algorithm of Go 1.25's
  `CrossOriginProtection`; no scheme comparison is attempted because TLS is
  often terminated by a proxy.
- **Failure.** `403 Forbidden` with a generic body. The reason (`CsrfError`)
  is logged with the request ID, never the token.
- **Proof.** After verification the layer stores `CsrfVerified` in the request
  extensions (`forge_views::csrf_verified`).

### Method override

After a successful check, and only for `POST`, the layer reads the single
`_method` field of a form-encoded body and applies `forge_views::method_override`
(`PATCH`, `PUT`, `DELETE`, ASCII case-insensitive; anything else stays `POST`)
with the `CsrfVerified` witness, then rewrites the request method before the
route table runs. A `GET` is never overridden, from the query or a body; a
repeated `_method` is refused.

### Relation to session-bound CSRF (ADR 0014)

ADR 0014 stays in force for authenticated requests: the generated
`infrastructure::security::enforce_csrf` still compares the presented token's
digest with the one stored for the session. In full-stack applications it reads
the token with `forge_views::presented_csrf_token`, so the form field works as
well as the header. When an application signs a user in, it issues the
ADR 0014 session secrets and sets the form CSRF cookie to the session's CSRF
token (`forge_views::csrf_cookie`), so pages render the session-bound token
and both checks agree; signing out rotates the cookie the same way with a
fresh token (for example the CSRF secret of `issue_session_secrets`). The login
flow itself remains application code (ADR 0014).

### Flash: host-only cookie, consumed on read

ADR 0022 calls flash "session-bound". Forge has no server-side session for
anonymous visitors, `-d none` applications have none at all, and a flash
written to a database row on every redirect would add a write per form
submission. Flash therefore travels in the browser, scoped like the session
cookie would be:

- `forge_views::Flash` (`notice` and `alert`, each at most 512 bytes) is
  attached to the response that ends an action, typically the `303` redirect,
  as the `__Host-forge_flash` cookie (`Secure`, `HttpOnly`, `SameSite=Lax`,
  `Path=/`): JSON, percent-encoded, at most 3 KiB.
- `FlashLayer` hands it to the next request. The first handler that reads it
  (`forge_views::flash`) consumes it: that response removes the cookie
  (`Max-Age=0`) unless it sets a new flash. Requests that do not read it keep
  it, so an asset or prefetch request cannot swallow a message.
- A tampered, repeated or out-of-shape cookie is ignored and removed.
- Messages are rendered through auto-escaping templates like any other value,
  so a forged cookie cannot inject markup. The cookie is not signed: its
  content is display text the server chose, an attacker who can write
  `__Host-` cookies for the origin already controls it, and signing would
  require a new long-lived secret and key-rotation design. Flash messages
  therefore must not contain secrets or authorization decisions.

### Content-Security-Policy

`ContentSecurityPolicy` is also a request layer that sets the strict policy on
every response that has none (an SVG asset keeps its sandboxing policy).
Generated applications register it outermost, so refusals carry it too. Turbo
injects its progress-bar stylesheet inline, which the policy blocks; the
generated `assets/application.css` carries those rules instead.

## Consequences

- HTML forms are protected from the first request, with or without a
  database, and the same token format serves anonymous and authenticated
  pages.
- Browsers accept `__Host-` cookies only from secure origins: production needs
  HTTPS (already true for the session cookie), and local development works on
  `http://localhost` and `http://127.0.0.1` in Chromium and Firefox. A plain
  HTTP origin with another host name cannot submit forms.
- A browser's first two concurrent page loads without a cookie may each issue a
  token; the last `Set-Cookie` wins and a form rendered by the other page is
  refused once (fail closed). The next page load fixes it.
- Tokens are not masked per response. Forge does not compress responses; a
  proxy that compresses HTML pages that also reflect attacker-controlled input
  could enable BREACH-style token recovery. Masking is a follow-up if Forge
  adds compression.
- The `Origin`/`Host` fallback can refuse legitimate requests behind a proxy
  that rewrites `Host` for browsers that do not send `Sec-Fetch-Site`; every
  current major browser sends it.
- Every unsafe request through a router with `CsrfProtection` needs a token,
  including JSON endpoints in a full-stack application. Machine clients use the
  header; webhook-style endpoints that cannot belong in a separate router or an
  `--api` application are a future, explicitly reviewed exemption mechanism.
- New direct dependencies of `forge-views`: `cookie` (already admitted for
  generated applications), `getrandom` (already admitted) and `subtle`; see
  [dependency policy](../dependency-policy.md).
