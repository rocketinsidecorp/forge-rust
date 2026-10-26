# Forge Threat Model

Status: Phase 0 baseline. This model defines required controls and tests; it does
not assert that those controls are implemented.

## Scope and security objectives

This model covers Forge framework code, generated applications, the `forge` CLI,
HTTP and streaming endpoints, PostgreSQL persistence, jobs/events, telemetry, and
AI provider/tool execution. It assumes an attacker can send arbitrary network
input, own a normal tenant account, influence content processed by an AI model,
and inspect publicly shipped binaries and source.

Primary objectives are:

1. Preserve tenant isolation and enforce authentication/authorization.
2. Prevent untrusted input or AI output from acquiring ambient capabilities.
3. Preserve integrity and replay safety for state changes, jobs, events, webhooks,
   migrations, and generated code.
4. Limit confidentiality leakage through responses, logs, traces, prompts, and
   third-party providers.
5. Remain available under bounded abuse and fail closed when a security decision
   cannot be made.
6. Produce tamper-evident, attributable audit evidence without recording secrets.

## Assets and actors

Protected assets include tenant data, identities and sessions, credentials and
key material, authorization policy, database integrity, code/release artifacts,
AI prompts and responses, cost budgets, audit records, and service availability.

Actors are anonymous clients, authenticated users, tenant administrators, system
operators, application developers, external services/webhook senders, AI
providers, and AI-driven agents. Infrastructure administrators are privileged but
their actions remain auditable. External services and AI providers are never
implicitly trusted merely because they are configured.

## Trust boundaries and data flow

```text
Internet
  | untrusted protocol/input
  v
HTTP adapter -- authn/limits/validation --> application policy boundary
                                               | explicit tenant/capability
                                               v
                                      persistence and external ports
                                               |
                            +------------------+------------------+
                            v                                     v
                     PostgreSQL/RLS                    external vendors

AI provider response/tool call (untrusted)
  -> policy -> authorization -> validation -> approval
  -> sandboxed executor -> filtered result -> audit

developer input -> forge CLI/generator -> reviewed source tree -> build pipeline
```

Trust transitions must use typed, validated contracts. Internal network location
does not establish identity. Tenant and principal context cannot be inferred from
global state, model output, headers accepted from the public edge, or job payloads
alone.

## Threats and required controls

| Threat | Required controls | Verification |
| --- | --- | --- |
| Credential stuffing and session theft | Argon2id password hashing with parameter policy, generic login errors, per-account and per-origin throttles, rotation on privilege change, secure/HttpOnly/SameSite cookies, short-lived credentials, revocation | authentication integration and abuse tests |
| CSRF and cross-origin abuse | SameSite defaults, unpredictable CSRF token for cookie-authenticated mutations and every unsafe request of an HTML application (host-only `__Host-` double-submit cookie, header or form field, constant-time comparison, query string ignored), Fetch Metadata/`Origin` rejection of cross-site requests, `_method` override only for verified `POST` forms, exact-origin CORS allowlist, reject credentialed wildcard origins | `forge-views` CSRF negative tests, generated `tests/http.rs`, e2e `curl` checks |
| Cross-site scripting in server-rendered pages | auto-escaping compile-time templates, explicit greppable `\|safe` only on framework-built markup, strict CSP without `unsafe-inline`/`unsafe-eval` (import map allowed by hash) on every response, validated DOM ids, escaped flash messages, vendored integrity-pinned JavaScript, `nosniff` | template escaping and CSP header tests, vendored digest tests |
| Broken object authorization | authorization at use-case boundary, typed principal and tenant context, deny by default, no authorization based on object-ID secrecy | policy matrix and negative integration tests |
| Cross-tenant data access | mandatory `TenantContext`, transaction-local PostgreSQL setting, RLS, non-bypass application role, pooled-connection reset, audited separate admin path | two-tenant tests for every repository plus RLS tests |
| SQL/command/template injection | parameterized SQL, typed command arguments without shell interpolation, contextual output encoding, validated schemas | fuzz/property tests and static review |
| SSRF | parsed URL policy, deny loopback/link-local/private/control-plane ranges by default, DNS resolution and redirect revalidation, scheme/port allowlist, response/time limits | rebinding and redirect abuse tests |
| Path traversal and unsafe upload | logical storage keys, canonical root confinement, no user-controlled absolute paths, file type/size limits, randomized server names | traversal corpus and symlink-race tests |
| Memory exhaustion through password hashing | every Argon2id hash or verification (19 MiB each, unknown logins included) runs through one process-wide `PasswordWorkLimiter`: at most one per CPU (16 at most) at once, a two-second wait for a slot and then `503`; the slot is held by the blocking work, so a cancelled request cannot free it early | `forge-auth` `concurrent_work_never_exceeds_the_bound`, `a_full_limiter_fails_fast_after_its_wait`, `a_cancelled_caller_keeps_its_slot_until_the_work_ends` |
| Resource exhaustion | request/body/header limits, bounded queues/concurrency, deadlines, backpressure, per-principal quotas, streaming byte limits, circuit breakers | load and saturation tests |
| Replay and duplicate mutation | scoped idempotency keys with request digest and expiry, signed webhook timestamp/nonce, persistent deduplication for jobs/events, single-use execution-plan ID | replay integration tests |
| Webhook forgery | per-source signature verification over raw bytes, constant-time comparison, timestamp window, secret rotation overlap, replay store | signature vector and replay tests |
| Sensitive-data leakage | classified fields, response allowlists, error redaction, telemetry filtering, secrets as opaque references, no prompt/body capture by default | snapshot/redaction tests and log scans |
| Audit tampering | append-only restricted writer, actor/tenant/action/outcome/policy/version/request linkage, integrity chaining or external immutable sink, retention policy | authorization and integrity-verification tests |
| Dependency/build compromise | pinned lockfile, advisory/license/source policy, minimal features, reviewed build scripts, SBOM, isolated CI, provenance and signed release artifacts | `cargo deny`, audit, reproducibility and signature gates; action and image pins, permissions and the tag-only release enforced by `crates/forge/tests/supply_chain.rs` (CycloneDX/SPDX SBOMs and a cosign-signed forge-proxy image: [supply chain](supply-chain.md), [ADR 0043](adr/0043-continuous-fuzzing-sbom-signed-images.md)) |
| Container escape or image secret leakage | multi-stage build, immutable base digests, non-root numeric user, minimal final stage, read-only filesystem compatibility, dropped capabilities, no layer/build-arg secrets, image scanning | image inspection and container E2E tests |
| Database transport interception or TLS downgrade | production refuses `FORGE_DATABASE_URL` and `FORGE_MIGRATION_DATABASE_URL` unless they verify the server certificate (PostgreSQL `sslmode=verify-full`, MySQL/MariaDB `ssl-mode=VERIFY_IDENTITY`; `verify-ca`/`VERIFY_CA` refused because SQLx's rustls backend trusts the public web roots besides the given CA and skips the host name; the mode read as SQLx reads it, last occurrence winning), a Unix socket or SQLite; `FORGE_DATABASE_ALLOW_PLAINTEXT=true` is the explicit opt-out for a trusted private network; errors name the variable and the fix, never the URL | `forge-config` `production_refuses_downgradable_tls_and_says_how_to_fix_it`, `production_database_urls_must_verify_the_server_certificate`, e2e `healthcheck` refusal |
| Migration compromise | checksummed immutable released migrations, dedicated migration role, advisory lock, backup/restore plan, least privilege | migration integrity and rollback-recovery exercises |
| Malicious generated input | strict generator grammar, normalized paths confined to project, no template evaluation from untrusted packages, preview/diff before overwrite | path/property tests and golden outputs |
| Unsafe Rust memory flaw | default deny for framework-written `unsafe`; isolated reviewed modules with stated invariants and Miri/fuzz coverage when unavoidable | lint policy, review, Miri/fuzz gates |

## AI-specific abuse cases

AI prompts, retrieved documents, provider responses, structured outputs, and tool
arguments are untrusted. Prompt instructions cannot grant authority.

| Abuse case | Required response |
| --- | --- |
| Prompt injection requests secrets or policy bypass | Secret data is unavailable to the model unless an explicit scoped capability permits it; policy is evaluated outside the model. |
| Model invents or broadens a tool argument | Validate against a closed schema and compare the executed plan byte-for-byte with the authorized plan. Unknown fields and ambiguous paths fail. |
| Model requests shell/network/filesystem access | Deny unless an explicit capability and sandbox profile grants the exact operation. Default sandbox has no network and minimal filesystem visibility. |
| Tool output injects a second instruction | Treat output as data, label provenance, filter secrets, and require a new policy decision for every subsequent action. |
| Provider fallback violates data residency | Route only among providers permitted by the request's data-classification policy; otherwise return a policy error. |
| Retry repeats a side effect | Generation may retry within budget; execution requires an idempotency contract and a single-use execution-plan identifier. |
| CLI provider escapes process controls | Use argv without a shell, sanitized environment, fixed executable resolution, bounded stdio, working-directory confinement, timeout, cancellation, and sandbox. |
| Model drains token/cost budget | Enforce per-request, principal, tenant, and provider token/cost ceilings before and during streaming; cancel when exceeded. |
| Sensitive prompt content reaches telemetry | Record identifiers, model, timing, token counts, policy outcome, and cost by default; content capture is separate opt-in with classification and retention. |

Approval is a policy action, not a UI confirmation alone. Approval records bind a
principal to the exact execution-plan digest and expire. High-risk capabilities
(credential access, external mutation, broad filesystem writes, or privileged
commands) must not be auto-approved.

## Cryptography and secret management

Forge will use maintained cryptographic crates and platform/provider KMS systems;
it will not implement primitives. Keys have identifiers and versions, and
ciphertexts record the version required to decrypt. Rotation supports an overlap
window and background re-encryption. Comparisons of authenticators use
constant-time library functions.

Secrets are resolved at the infrastructure boundary, never stored in ordinary
configuration values, and redacted from `Debug`, errors, telemetry, CLI output,
and generated diagnostic bundles. Secret access is capability-scoped and audited.

## Fail-closed rules

- Missing or invalid identity, tenant, policy, sandbox support, key material, or
  replay state denies the operation.
- Failure of the audit sink denies high-risk AI execution and privileged admin
  actions; normal read availability may use a bounded durable local buffer only if
  policy explicitly permits it.
- A database transaction that cannot establish tenant-local settings is aborted.
- Unknown configuration keys fail startup in production mode.
- Provider fallback is disabled if classification policy cannot be evaluated.

## Residual risks and assumptions

- A fully privileged host administrator can inspect process memory; deployment
  hardening and managed secret/KMS services reduce but do not remove this risk.
- RLS cannot protect data copied to logs, caches, search systems, or third parties;
  every adapter needs equivalent tenant scoping.
- OS sandbox guarantees differ. An executor must publish its supported capability
  level and refuse policies it cannot enforce.
- Compromised allowed dependencies or toolchains remain a risk despite audits and
  provenance controls.
- Models can produce unsafe content even when system access is denied. Product
  policy and output moderation are application concerns exposed through hooks.

## Phase 3 review: identity and PostgreSQL tenant isolation

Review date: 2026-09-27.

The implemented Phase 3 slices provide typed authenticated principals,
deny-by-default RBAC, explicit tenant membership/TenantContext, separate
PostgreSQL migration/runtime roles, transaction-local tenant/principal settings
for tenant-owned data, persistent credential-digest sessions, membership
persistence, Argon2id password verification, independent opaque bearer/CSRF
secrets, secure host-only cookie helpers, and append-only audit persistence. The
long-running runtime role is generated without schema ownership or BYPASSRLS, and
migration commands require a credential distinct from the runtime URL.

`scripts/e2e-generated-app.sh` is the linked negative integration test for the
database boundary. It verifies migration/runtime privilege separation, tenant
RLS isolation, append-only audit restrictions, identity/session schema
invariants, digest-only session persistence, and membership lifecycle storage.
Unit tests in the generated security module cover Argon2id authentication,
policy-upgrade detection, generic login denial, opaque token generation, cookie
attributes, session lookup, and CSRF enforcement. The generated database E2E also
checks that runtime password-hash updates are column-restricted and use an
optimistic credential version so stale upgrades do not overwrite newer state.

Append-only audit persistence remains a separate structured security-evidence
channel. The generated runtime role can INSERT audit events but cannot SELECT,
UPDATE, or DELETE them, and a database trigger rejects audit-row mutation. This
does not claim tamper-evident cryptographic integrity or immutable external
storage.

Per-account and per-source credential throttling and the HTTP
sign-in/sign-out/password-reset composition are now generated
([browser authentication review](#browser-authentication-review)). Remaining
Phase 3 threat-model controls are not claimed complete: a shared (multi-process)
throttle, password-reset delivery, audit integrity chaining/retention and
privileged reader workflows, and administrative cross-tenant role separation
still require implementation and abuse tests.

## Application lifecycle commands review

[ADR 0036](adr/0036-application-lifecycle-commands.md): `forge dev`, `migrate`,
`rollback`, `setup`, `seed`, `db ...` and `doctor` run next to production
credentials and can destroy a database.

| Threat | Control | Verification |
| --- | --- | --- |
| Dropping, resetting or rolling back a production database by mistake or by a pasted command | `db drop`, `db reset` and `db rollback` refuse `FORGE_ENV` other than development or test (an unknown value counts as protected) before anything is built; `--force-production` plus the application name typed at a terminal, or `--yes` with `FORGE_ALLOW_DESTRUCTIVE=true` in the process environment; `--yes` alone is a usage error; the application's own `db drop` refuses production and non-local servers unless `--force-production`, so a container is guarded without the CLI | `lifecycle` unit tests for every environment value, flag combination and wrong confirmation; `tests/lifecycle.rs` (no build, no application run on refusal); the `forge` crate's SQLite `db` test; the PostgreSQL scaffold test |
| Destructive switch granted by a file | `FORGE_ALLOW_DESTRUCTIVE` is read from the process environment only, any value but `true` is refused, and it is removed from the application's environment | `yes_needs_the_process_environment_to_allow_it`, `the_clis_own_switch_never_reaches_the_application`, `the_database_url_is_passed_through_and_the_clis_switch_is_not` |
| Credential exposure by the CLI | The CLI reads no database URL for lifecycle commands (the application inherits the environment); `forge doctor` prints engine, database and host only, never user, password or query, and `--json` omits values; no URL appears in any error | `delimiters_in_a_password_never_leak_into_a_description` (unescaped `/`, `?`, `#`, `@`), `describe_names_the_effective_target_and_never_credentials`, doctor tests that grep the output for the secret, `tests/lifecycle.rs` |
| Running the wrong project's code | Lifecycle commands require a `Cargo.toml` with a `[package]` name and a `forge` dependency; the application is built with `--locked`; the binary run is the one Cargo reported | `project` tests, `other_directories_and_packages_are_refused` |
| Injection through a database name, or a query parameter that moves the target | `db create|drop` accept `[A-Za-z0-9_$-]{1,63}` only, never the engines' system databases, checked on the database the driver would use (`dbname` and the path), and quote nothing else; the host and its locality are read from the driver's own parser, so `host`, `hostaddr`, `port` and `socket` query parameters cannot make a remote server look local | `names_are_restricted_to_what_every_tool_accepts`, `names_come_from_the_driver_not_the_path`, `query_parameters_cannot_make_a_remote_server_look_local`, `malformed_urls_are_refused_without_echoing_them` |
| Orphaned or runaway processes | The application and each build run in their own process group, stopped with SIGTERM, a 35-second grace and SIGKILL (at once on a second Ctrl-C); an owned child is killed when dropped; one signal thread, joined on exit; no detached tasks | `process` tests (stop, ignore-TERM kill, drop kills), `tests/lifecycle.rs` SIGTERM and SIGINT while `forge dev` runs |
| Dev loop breaking a healthy server | The old process keeps serving until a build succeeds; a failed or stale build never replaces it; a crashed application is not restarted in a loop | supervisor tests with a fake host, `dev_keeps_the_running_application_when_a_build_fails` |
| Resource exhaustion by the watcher | Trees beyond 50 000 files are refused; build output and hidden directories are skipped; one thread polls | `scan_skips_build_output_hidden_and_non_code_directories` |

Residual: `forge dev` is for development; its restart replaces the binary under
`target/` and has no browser-reload proxy. The CLI does not authenticate the
application it runs beyond the project check above; run lifecycle commands only
in code you trust, as with `cargo run`.

## Server-rendered views review

Review date: 2026-10-19 ([ADR 0022](adr/0022-server-rendered-views.md),
[ADR 0023](adr/0023-form-csrf-flash-and-method-override.md)).

Full-stack generated applications register three request layers before
routing: the strict Content-Security-Policy, `CsrfProtection` and
`FlashLayer`.

| Threat | Control | Verification |
| --- | --- | --- |
| Cross-site form or `fetch` submission | Every unsafe request needs exactly one 256-bit token (header or form field) equal to the `__Host-forge_csrf` cookie (`Secure`, `HttpOnly`, `SameSite=Lax`, no `Domain`); constant-time comparison; repeated/malformed values refused; `403` with a generic body | missing, wrong, repeated, malformed, query-string and header/field-mismatch tests in `forge-views`; generated `tests/http.rs`; e2e |
| CSRF token recovered through compressed responses (BREACH) | Pages never embed the token itself: `csrf_meta` and `csrf_field` write it masked with a fresh 32-byte one-time pad from the OS CSPRNG on every render (pad, then token XOR pad, 86 characters of unpadded base64url, Forge-Go's and Rails' layout), so no two responses carry the same token bytes; verification unmasks (exact length, strict base64url, canonical re-encoding) and compares with the cookie in constant time. The cookie keeps the raw token, which no compressed body reflects; the unmasked token stays accepted in the header for good (harmless: masking is a public transform anyone holding the token can apply, and clients the server gave the token to may send it plain; the cookie stays `HttpOnly`) and in the form field until v0.2.0 (pages rendered before the upgrade); a CSPRNG failure renders an empty value that is refused | `forge-views` masking tests (fresh masks, both forms accepted, tampered, truncated, padded, foreign and arbitrary input refused without panics); generated `tests/http.rs` (`home_page_is_html_with_layout_csp_and_csrf_token`, `form_posts_need_the_csrf_token_of_the_page`); e2e |
| Cookie tossing from a sibling subdomain or plain-HTTP attacker | `__Host-` prefix; `Sec-Fetch-Site: same-site` refused | cookie attribute tests; cross-site tests |
| Login CSRF / anonymous forms | Token issued before any session, independent of the database | first-visit issuance test |
| Tunnelled `DELETE` without CSRF | `_method` honored only on `POST`, only `PATCH`/`PUT`/`DELETE`, only after verification (`CsrfVerified` witness), repeated field refused, `GET` never overridden | method-override negative tests |
| Token leakage through caches | Token-bearing pages are `Cache-Control: no-store`; asset and probe responses never receive `Set-Cookie` | cache and lazy-issuance tests |
| Flash injection | Messages rendered escaped; cookie `__Host-`, bounded, shape-checked, removed when tampered | flash tests |
| Form parser abuse | Exactly one UTF-8 form `Content-Type`, body bounded by `FORGE_MAX_BODY_BYTES`, at most 1000 pairs, strict grammar, duplicate scalar fields refused, caller-safe errors | `forge-http` form tests |
| Asset path traversal or MIME confusion | Exact digested routes only, closed extension allowlist, `nosniff`, SVG sandbox CSP | existing asset tests |

Residual risks: until v0.2.0 the form field also accepts the unmasked token,
which only matters for pages that still embed it (rendered before the
upgrade); the flash cookie is unsigned (display text only); the `Origin`/`Host` fallback can
refuse legitimate requests behind `Host`-rewriting proxies for browsers without
Fetch Metadata; `__Host-` cookies require HTTPS outside `localhost`; a full-stack
router requires the token on JSON endpoints too (no reviewed exemption mechanism
yet); multipart forms are not parsed and must send the header. Login-time
binding of the form token to the session (ADR 0023) is done by the generated
sign-in page (below).

## Browser authentication review

Review date: 2026-10-19
([ADR 0026](adr/0026-authentication-pages-and-setup.md)). Scope: the
generated sign-in, sign-out and password-reset pages, the `setup` and `seed`
commands, and the framework rules they rely on.

| Threat | Control | Verification |
| --- | --- | --- |
| Online password guessing and credential stuffing | Attempts counted before any credential check against the canonical login (whatever organization or letter case is typed), the source (`Request::client_ip`: the socket peer unless it is a proxy listed in `FORGE_HTTP_TRUSTED_PROXIES`, ADR 0032; IPv6 per /64) and the pair: 5 per login and source, 50 per login, 300 per source in 15 minutes (reset requests and submissions 3, 10 and 30 per hour, `password_reset_throttle_config`), counted in PostgreSQL by `PgLoginThrottle` so replicas share one budget and a restart or deploy resets nothing (database clock, rows locked in a fixed order, a refused attempt counts and stores nothing, expired rows pruned by each attempt, digests only, no tenant data; an unreachable table refuses the attempt); `429` with `Retry-After`; success clears the pair and login counters, not the source; Argon2id | `forge-auth` throttle tests and `tests/throttle_contract.rs` (the same contract on memory and PostgreSQL, concurrent first attempts, replicas, restart, pruning, refusals storing nothing); generated `attempts_are_throttled_even_with_the_right_password`, `letter_case_and_unicode_variants_do_not_get_fresh_guesses`, `clients_behind_one_trusted_proxy_have_their_own_sign_in_counters` |
| Throttle as a denial of service | One source cannot lock a login out for others (the tight limit is per login and source). In memory, a full table evicts the oldest idle counters instead of refusing everyone, and keeps blocking ones; counters are indexed in expiry order, so an attempt never scans the table under its lock. In PostgreSQL (`PgLoginThrottle`) there is no entry cap: the table is bounded by the attempt rate times the window, and every attempt prunes up to 8 expired rows (`prune_expired` empties a quiet table); the global and HTTP rate limits bound the attempt rate | `one_source_cannot_lock_a_login_out_for_other_sources`, `a_full_table_evicts_idle_counters_and_keeps_blocking_ones`, `expired_counters_leave_from_the_front_and_the_index_stays_consistent` |
| Reset queue flooding, token rows for undeliverable links | One source may send 30 reset requests an hour, so filling the 256-request queue takes many sources; without a mailer (`UndeliverablePasswordResets::can_deliver` is `false`) nothing is queued and no token row is issued | `reset_limits_match_forge_go_and_are_valid`, `reset_requests_issue_nothing_when_no_link_can_be_delivered` |
| Look-alike (Unicode) logins | `CanonicalLogin` refuses non-ASCII logins at the boundary (storage, lookup, throttle keys); reset links are delivered to the login as stored, never as typed | `forge-auth` login tests; `reset_links_go_to_the_stored_login_and_look_alikes_get_nothing` |
| Timing oracle on reset requests and sign-in | Reset requests are queued to a bounded lifecycle-owned worker and answered after at least 100 ms; sign-in runs the tenant and membership lookups with random identifiers for malformed or unknown organizations; unknown reset tokens are refused before Argon2id | `reset_requests_take_the_same_minimum_time`, `sign_in_looks_up_a_membership_whatever_the_organization`, `unknown_reset_tokens_are_refused_before_hashing` |
| Account and tenant enumeration | One `422` message for a wrong organization, email or password, a disabled account or an inactive membership; unknown logins still cost one Argon2id hash; reset requests answer the same `303` and flash whether or not the account exists or the link was delivered | `every_wrong_field_gets_the_same_answer`, `reset_requests_answer_the_same_whether_or_not_the_account_exists` |
| Login CSRF and forged sign-out | Handlers refuse (`403`) unless `CsrfProtection` verified the request (`csrf_verified`); the sign-in form's token is issued before any session; sign-out also checks the session-bound CSRF token (ADR 0014) | `sign_in_without_the_csrf_token_is_forbidden`, `signing_out_needs_the_csrf_token_and_revokes_the_session` |
| Session fixation and token planting | Sign-in issues a new server-side session and a new CSRF token in fresh cookies and revokes the session the browser held; sign-out and reset clear the session cookie and rotate the CSRF token | `signing_in_issues_a_fresh_session_and_csrf_token_and_returns_to_the_page`, `signing_in_again_revokes_the_previous_session` |
| Unattended or stolen session used long after its last use | Besides the absolute lifetime (24 hours), a session ends after `FORGE_AUTH_SESSION_IDLE_TIMEOUT` (default `2h`, `5m` to `720h`, no way to turn it off) without a request: `PostgresSessionStore` refuses a stale `last_seen_at` like a revocation (`401`, or the sign-in page), and the refused attempt does not revive it. The use is written at most once a minute per session by a conditional `UPDATE` that re-checks that the session is live, decided by the database clock; a failed write fails the request (`503`) | generated PostgreSQL `sessions_end_after_the_idle_timeout_and_record_their_use`, `sessions_use_the_default_idle_timeout_of_two_hours`, `a_session_use_is_written_at_most_once_a_minute` (16 concurrent resolutions of a stale session write once, counted by a trigger), `the_idle_timeout_migration_keeps_open_sessions_and_reverts`; `forge-config` bounds test |
| Process-local throttle in a replicated production | Generated applications count in PostgreSQL (`PgLoginThrottle`); custom wiring that passes the in-memory `MemoryLoginThrottle` for sign-in, two-factor or password reset logs one warning at start-up when `FORGE_ENV=production` (`Composition::warn_if_process_local_throttle`, Forge-Go's wording). A warning, not a refusal: a single-process application may count in memory on purpose | `forge` `process_local_throttles_warn_in_production_only`; `forge-auth` `is_process_local` tests |
| Open redirect through `return_to` | `return_path` accepts only local paths (single leading `/`, visible ASCII, no backslash, at most 2048 bytes) when rendering, posting and redirecting; `see_other` re-checks | `forge-views` return-path tests; `return_paths_off_the_site_are_ignored` |
| Reset token theft or replay | 256-bit CSPRNG token, SHA-256 digest persisted, single use (row lock, every outstanding token of the account used up), 15-minute expiry, bound to principal and tenant membership, never logged or in `Debug`; reset pages `no-store` and `Referrer-Policy: no-referrer`; every session revoked on reset | `forge-auth` token tests; `a_reset_link_works_once_changes_the_password_and_revokes_sessions`; PostgreSQL `reset_tokens_work_once_expire_and_revoke_every_session` |
| Takeover through `setup` | Runs only with the migration credential, one transaction under an advisory lock, refuses once any tenant exists; password never an argument (`--password-stdin`, `FORGE_SETUP_PASSWORD` or an echo-off prompt), at least 12 characters, never printed; audited | `setup_creates_one_owner_who_can_sign_in_and_refuses_afterwards`, `concurrent_setups_create_exactly_one_tenant`, `setup_refuses_password_arguments_and_short_passwords_before_touching_a_database` |
| Signed-out browser leaks a protected page | Access policies answer `401`; `SignInRedirect` turns it into a `303` to sign in for HTML clients only; JSON keeps `401` | `forge-views` sign-in redirect tests; `signed_out_browsers_are_sent_to_sign_in_and_api_clients_get_401`; slow scaffold e2e |

Residual risks: custom wiring with the in-memory throttle counts in one
process (a restart or several replicas multiply the budget; production logs
a warning); behind a
reverse proxy clients share one source budget until `FORGE_HTTP_TRUSTED_PROXIES`
lists the proxy, and anything inside a listed network can choose its source
(see the trusted-proxy review below); an attacker who controls 50 sources (or /64s)
can still keep one known login throttled for 15 minutes at a time, and one
with very many sources can fill the table with blocking counters, after
which new attempts are refused until counters expire; a full reset queue
drops requests (logged), so a flood can delay legitimate resets; the reset
worker's database load still depends on whether accounts exist, observable
only through server load; sign-in still does slightly more work (session
issuance) when it succeeds; without `FORGE_MAIL_URL` the notifier is
`UndeliverablePasswordResets`, reset requests are not queued (no link, no
token row) and only logged (see the mail review below); within the idle
timeout a stolen session cookie stays usable as long as it is used, up to
the 24-hour lifetime, and an application generated before the idle timeout
has none until it adopts it. Two-factor authentication has its own review below.

## Phase 4 review: jobs, schedules and the outbox

Review date: 2026-10-19 ([ADR 0025](adr/0025-durable-jobs-scheduler-and-outbox.md),
[jobs guide](jobs.md)).

| Threat | Control | Verification |
| --- | --- | --- |
| Secrets or personal data in payloads leak through logs, errors or operator output | Payloads are JSON in the database only; logs carry job/event id, name, queue and attempt, never payloads; panic messages and decode errors are not recorded, and a process-wide panic hook installed once by the job runtime withholds the message of panics raised inside job attempts and outbox publications from stderr (other panics reach the previous hook unchanged); operator commands never print payloads; envelope `Debug` omits the payload; documentation requires references instead of secrets | `forge-jobs`/`forge-events` unit tests (sanitized errors, payload-free `Debug`); panic test asserts the stored message is generic; `tests/panic_redaction.rs` asserts attempt panics never reach the previous hook |
| Log forging through handler error messages | Stored/logged messages truncated to 1024 bytes and control characters replaced | `job` and dispatcher sanitizer tests |
| Cross-tenant job or event access | `tenant_id` + RLS policy identical to ADR 0011 (malformed scope fails closed); runtime role has only `SELECT, INSERT`; tenant jobs require an authorized `TenantContext` at enqueue and a matching transaction scope (tenant and principal); the worker reinstalls the job's scope for the handler; idempotency keys are unique per tenant | `row_level_security_isolates_tenants_and_workers_restore_the_scope`, `tenant_events_are_isolated_by_row_level_security` (non-superuser, non-owner runtime role) |
| Cross-tenant reads and changes through the worker path | Cross-tenant operations exist only as `SECURITY DEFINER` functions with fixed semantics (`EXECUTE` revoked from `PUBLIC`, `search_path` pinned with `pg_temp` last, argument bounds checked). They **are** cross-tenant reads: `forge_jobs_claim` and `forge_outbox_claim` return full payloads of every tenant, and `stats`/`dead` return other tenants' metadata and error messages. Mitigations: every one of them except the lease-fenced `forge_jobs_complete` refuses to run in a transaction carrying `forge.tenant_id` (a request-serving scope); with `forge.worker_role` they are granted only to a separate worker/operator role, and the request-serving runtime role keeps only `forge_jobs_complete` | `schema_reverts_cleanly_and_grants_least_privilege` (grants, denied writes/DDL, pinned search paths); `worker_functions_refuse_tenant_scope_and_can_be_split_from_the_runtime_role`; `dispatcher_functions_refuse_tenant_scope_and_can_be_split_from_the_runtime_role` |
| Stale or partitioned worker double-completes a reclaimed job | Fenced lease tokens on every transition; lease renewal cancels attempts that lost their lease; `complete_in` commits completion with effects | `expired_leases_are_reclaimed_and_stale_workers_are_fenced_out`, `transactional_completion_is_fenced_and_rolls_back`, `stale_dispatchers_are_fenced_out` |
| Poison jobs (crash, hang, panic, malformed payload) | Per-attempt timeout; panic isolation per task; lease-expiry reclaim counts the attempt; attempt ceiling then `dead`; undecodable payload or invalid stored scope fails permanently | timeout/panic and lease-expiry dead-letter tests |
| Queue flooding / resource exhaustion | Bounded per-queue concurrency (≤ 256) and claim batches; payload ≤ 256 KiB; no in-memory queues; bounded pruning batches; idempotency keys for deduplication | registration/settings tests; pruning test |
| Duplicate side effects from at-least-once delivery | Inbox `(consumer, event_id)` recorded in the effect transaction; concurrent duplicates serialize on the row | `concurrent_duplicate_deliveries_record_one_first_delivery`, end-to-end ledger test |
| Duplicate cron firing across replicas | Conditional upsert of `last_tick_at` plus per-tick idempotency key, in one transaction | `two_schedulers_fire_each_tick_exactly_once` |

Residual risks: whichever role runs the workers (the runtime role unless
`forge.worker_role` names a separate one) can read every tenant's payloads
through the claim functions and claim, retry or discard any job or event; in
the default single-role setup that is the request-serving process, so
deployments with strict isolation needs should run `jobs work` with a
separate worker role and `WorkerMode::Dedicated`. The tenant-scope refusal is
defense in depth against application bugs, not against a compromised process
(which can simply omit the setting). Tenant membership is not re-checked when
a job runs, so revocation does not cancel already enqueued work; dead-job
error messages are visible across tenants to operators; there are no
per-tenant quotas or rate limits on enqueueing yet.

## Rate limiting, cache and file storage review

Review date: 2026-10-19 ([ADR 0027](adr/0027-rate-limiting.md),
[ADR 0028](adr/0028-cache.md), [ADR 0029](adr/0029-file-storage.md); guides:
[rate limiting](rate-limiting.md), [caching](caching.md), [storage](storage.md)).
Ported from the reviewed Forge-Go design; the controls and negative tests are the
same unless a difference is stated.

| Threat | STRIDE | Control | Verification |
| --- | --- | --- | --- |
| Limiter key spoofing: a client rotates a forwarded header or address to get a fresh bucket per request | S, D | `by_client_ip` uses `Request::client_ip` (ADR 0032): `X-Forwarded-For` only from a peer listed in `FORGE_HTTP_TRUSTED_PROXIES`, read from the right; `by_remote_addr` uses only the TCP peer; `X-Real-IP` and `Forwarded` are never read; IPv6 clients are keyed by `/64`. The key function documents that its result must not be client-chosen. **Residual:** behind an unlisted reverse proxy every client shares the proxy's bucket; anything inside a listed network can choose its key | spoofed-header test proves the limit holds; `direct_clients_cannot_choose_their_client_ip_bucket`, `clients_behind_a_trusted_proxy_get_their_own_buckets`; `/64` tests |
| Limiter memory exhaustion by distinct keys | D | `MemoryLimiter` is bounded by `max_keys` (deadline-ordered expiry, no task); `EvictSoonest` never refuses new clients, `Deny` never drops a counter; counters saturate. Shared stores keep one 64-character-key row per live bucket and prune in bounded batches after every `prune_batch` hits. **Residual:** with `EvictSoonest`, more than `max_keys` distinct keys in one window can reset other clients' counters, so never key it on attacker-choosable per-target values (use `Deny` or a database limiter) | flood test keeps `len == max_keys`; `Deny` keeps counters; sustained-keys pruning test per backend |
| Limiter bypass or outage when its store fails | D, E | The middleware fails closed (`503`); `fail_open` is an explicit per-route choice; limiter errors are logged server-side, never returned | fail-closed/open test; error text never contains SQL or keys |
| Personal data in limiter storage | I | Keys are SHA-256 digests of length-prefixed (scope, window, identifier); addresses and e-mail addresses are never stored | key and recording-limiter tests |
| Concurrent hits over-admitted | E | One atomic statement per hit (MySQL: upsert and read in one transaction) | 160 concurrent hits allow exactly `N` on every store |
| Unbounded or stale cache growth | D | Entries, value size, key size and ttl are bounded; memory also by bytes; database stores prune expired entries and evict the oldest writes in bounded batches. **Residual:** the database entry bound is soft but tight (about `prune_batch` per writing process) | entry-bound, bounded-prune and sustained-write tests per backend |
| Cache poisoning through attacker-influenced keys or values | T, E | Keys built with `key` (length-prefixed parts and namespace); an undecodable entry is a miss; values are copied. **Residual (application):** whoever can write the table can poison every replica; per-user data must carry the user in the key | key-collision matrix, JSON-miss tests |
| Cross-tenant cache reads | I, E | `tenant_key` derives the key from an authorized `TenantContext`, never a bare id | distinct tenants/same principal tests |
| Secrets or identifiers leaked through the cache | I | Rows hold `SHA-256(key)`; keys and values are never logged and errors never contain them | log-capture test; hashed-key test per backend |
| Thundering herd on a cold key | D | `fetch` collapses concurrent misses in a process; a dropped or panicking leader never fails waiters, who take over. Across replicas the herd is not collapsed (documented) | 24 concurrent misses compute once; cancelled/panicking leader tests |
| Upload abuse: oversized bodies, many files or fields | D | `receive` caps the body, bounds files, text-field count (every repetition counts) and bytes, and header sizes; the server body limit bounds buffering and `FORGE_STORAGE_MAX_UPLOAD_BYTES` cannot exceed it; all buffered bodies together are bounded by `FORGE_MAX_IN_FLIGHT_BODY_BYTES` ([HTTP core review](#http-core-review)). **Residual:** bodies are buffered (up to `FORGE_MAX_BODY_BYTES`) until `forge-http` streams; disk space is not metered per user | oversized, body-cap, repeated-field (1500 values) and header-budget tests |
| Hostile multipart framing: smuggled parts, re-encoded content, duplicate headers, preamble/epilogue tricks | T, S | A strict parser: no preamble/epilogue/padding/folding, one disposition, identity encodings only, bounded headers and names | rejection table, differential and mutation tests |
| Hostile file content: HTML, SVG or script uploaded as an image; polyglots | T, E | A mandatory allow-list decided on the type **sniffed from the bytes**, not the label; labels that need a signature must match it; text labels need text content. Served as downloads (`attachment`, `nosniff`, sandboxing CSP) except raster images explicitly requested inline. **Residual:** the sniffer reads 512 bytes, so a valid image with appended active content is still an image to a browser; use a separate origin for user files and do not widen `inline_types`; no malware scanning | disguised-type table, polyglot, inline tests |
| Path traversal and symlink escape in the disk adapter | T, I, E | `StorageKey` makes `..`, hidden files, separators, drive letters and escapes unrepresentable; every component is opened with `openat` and `O_NOFOLLOW` against descriptors opened at startup, so there is no check-then-use window | traversal-key, planted-symlink (file, directory, metadata), replaced-directory and startup-link tests |
| Partial or torn files after a crash or concurrent overwrite | T | Temp file, `fsync`, `rename`; a failed or oversized write leaves the old blob; orphaned temp files are bounded by `prune_temp`; a multi-file upload is staged then published as one unit, so a cancelled request leaves no committed blob and a failed overwrite keeps the previous metadata | concurrent-overwrite test never observes a torn write; cancelled-upload and failed-overwrite tests |
| Client-chosen file name used as a path or header | T | Keys are UUIDv7; the name is sanitized and encoded (RFC 5987 for non-ASCII); invalid header values are dropped | sanitizer table; header-injection test through stored metadata |
| Signed link tampering or forgery | S, T, E | HMAC-SHA256 over key, expiry, disposition and binding with a 32-byte-minimum key, constant-time comparison against every key; strict parameter parsing with no second spelling | other key, extended expiry, added inline, duplicate and malformed parameters, percent-encoding, short/padded signature tests |
| Signed link replay: a leaked or forwarded link works until it expires | I | A link is a bearer capability. Default 15 minutes, maximum 7 days; `Cache-Control: private` for at most the remaining lifetime; `binding` ties a link to a principal; rotating the key revokes all links. **Residual:** within its lifetime an unbound link works for anyone who holds it, and links are logged like any URL | binding and rotation tests |
| Existence oracle for stored keys | I | Signature, expiry and binding are checked before the store is consulted; unsigned and forged requests get the same `403` whether the key exists or not | store-call counter stays zero for every invalid form |
| Missing or weak signing key | S, E | No default key; configuration requires canonical base64url of 32 to 1024 bytes and, in production, a key whenever storage is configured; the key is redacted and never echoed | config and signer tests |
| Stored XSS from served files | T, I | Active content never renders on the application origin: every response is `nosniff` with `default-src 'none'; sandbox` | active-content table |

Residual risks beyond the rows above: the disk adapter assumes a trusted volume
(an attacker with write access can still delete or corrupt blobs, though not
escape the root); `MemoryLimiter` and `MemoryCache` state is per process;
byte-range serving is not available.

## Live updates review

Review date: 2026-10-20 ([ADR 0031](adr/0031-live-updates-over-server-sent-events.md);
guide: [views](views.md#live-updates)). Ported from the reviewed Forge-Go design
(its ADR 0029); the token format, limits and negative tests are the same unless a
difference is stated.

| Threat | STRIDE | Control | Verification |
| --- | --- | --- | --- |
| Subscribing to a channel the server never rendered (guessing or editing a stream name) | S, I, E | Channel names are HMAC-SHA256 tokens (RustCrypto, 32-byte minimum key, domain-separated label) compared in constant time (`subtle`) against every key; expiring (24 h default, 7 days maximum); at most 512 bytes; strict unpadded base64url; exactly one `stream` parameter. Forged, tampered, foreign-key and expired tokens get the same `403 stream_invalid` | signer forgery table (tampered MAC and claims, other key, padding, trailing bytes, oversize), expiry tests, handler test that every refusal body is identical; tokens pinned byte for byte against Forge-Go |
| Cross-tenant disclosure: a member of tenant B reads tenant A's channel | I, E | A tenant channel is a distinct key; the handler serves it only to a `StreamViewer::member` built from a `TenantContext` of the same tenant (which needs a session, an active membership and a grant); generated apps serve tenant channels at `/tenants/{tenant_id}/streams` through the resource's own `TenantAccess`, so a token for A under B's path is refused; `NOTIFY` payloads carry the tenant in the channel key | handler other-tenant/principal/anonymous tests, generated `broadcasts_stay_in_their_tenant`, broker isolation test, PostgreSQL fanout test with another tenant's subscriber |
| A leaked stream URL (logs, referrer) used by someone else | I | The token is a capability, never an identity: the access policy still authenticates the caller and checks `<plural>:read`; streams end after 30 minutes and reconnects re-run every check. **Residual:** URLs appear in access logs for the token lifetime | generated `streams_refuse_forged_unknown_and_unauthorized_subscriptions` (DenyAll, unregistered channel), running-server `401` without a session |
| Channels without a reader policy | E | Generated `Readers` deny every channel no resource registered; global channels follow the resource's `GlobalAccess` (`DenyAll` by default) | generated unknown-channel and DenyAll tests |
| One person (or a proxy's shared address) exhausting the stream slots of everyone signed in behind it | D | `GlobalAccess::authorize` returns the principal it admitted; generated `Readers` serve global channels to `StreamViewer::principal` for signed-in callers, so each person has their own 16 streams, and only anonymous viewers of public resources are counted by address. Tenant channels are always counted by principal (`StreamViewer::member`). Older access modules that return `()` are refused by the scaffold with the change to make | generated `signed_in_viewers_from_one_address_each_have_their_own_stream_limit`; scaffold refusal unit test |
| Script or field injection through broadcast content or stream targets | T | Targets are `DomId`s (`[A-Za-z][A-Za-z0-9_:-]*`), content comes from auto-escaping Askama views; SSE framing splits every line terminator into its own `data:` field, so a payload cannot end an event or add `id:`/`event:`/comment lines; the source element escapes its URL and the path is restricted | `events_frame_every_line_and_cannot_be_ended_early`, `dom_ids_reject_injection_attempts`, `from_view_escapes_user_data`, path validation test |
| Request-bound secrets broadcast to everyone | I | Fragments are rendered from views that take no request; generated tests assert broadcasts carry no CSRF token | generated `changes_are_broadcast_to_open_lists` |
| Memory or connection exhaustion by many or slow subscribers | D | No task per subscriber; bounded per-subscriber queues (64) with slow consumers dropped, never awaited; 10 000 subscribers per process, 1 000 per channel, 1 MiB per message; 16 open streams per principal (generated global channels: the principal `GlobalAccess` admitted; anonymous: per IP, IPv6 per `/64`) and 1 000 per tenant (`429`); the HTTP server ends a connection whose writes make no progress for the request timeout (at least 30 seconds); streams end at 30 minutes and when shutdown begins. Anonymous viewers are counted by `Request::client_ip` (ADR 0032). **Residual:** anonymous viewers behind an unlisted reverse proxy share the proxy's slots | slow-consumer, subscriber-cap, per-principal, per-tenant, IPv6 `/64`, `anonymous_viewers_behind_a_trusted_proxy_are_counted_per_client`, full-broker and stalled-reader (real socket) tests |
| Open streams holding up shutdown | D | The broker is bound to the shutdown-start signal; streams end before the server drains | real-server drain test with a 60 s grace that finishes at once |
| Forged or oversized `NOTIFY` payloads | T, D | Notifications are parsed strictly, bounded in chunk count, chunk size, pending messages (64), bytes and age (30 s); malformed or inconsistent ones are logged and ignored; payloads above 7 500 bytes are never sent; messages above 256 KiB are refused. **Residual:** whoever can `NOTIFY` on the database can publish; the database role is the trust boundary | envelope malformed/out-of-order/budget tests, PostgreSQL malformed-notification and oversize tests |
| LISTEN/NOTIFY emulated insecurely on other backends | T | `Fanout::Postgres` is refused on MySQL, MariaDB and SQLite (`UnsupportedBackend`); they use the in-process broker | `postgres_fanout_is_refused_on_sqlite_not_emulated`, `ensure_fanout_supported` doctest |
| Missing or shared signing key | S, E | `FORGE_STREAMS_KEY` and `FORGE_STREAMS_PREVIOUS_KEYS` (at most 3 keys that only verify, for rotation) are validated (base64 of at least 32 bytes), redacted and never echoed, errors name an entry's position only; previous keys without a current key are refused; production refuses to start without the key; development and test use a random per-process key | config and `from_config` tests, generated `production_refuses_to_start_without_a_signing_key`, `a_rotated_key_keeps_open_pages_working_and_signs_with_the_new_key` and `bad_previous_keys_refuse_to_start_without_echoing_a_key` |

Residual risks beyond the rows above: delivery is at most once without
replay; the in-process broker reaches one process only; each open stream
holds a connection slot; stream limits are per process; a vanished client
keeps its slot until a write fails (at most 30 minutes); rotating
`FORGE_STREAMS_KEY` stops live updates on open pages until reloaded unless the
old key moves to `FORGE_STREAMS_PREVIOUS_KEYS` (which must then stay for the
token lifetime, 24 hours, and is removed afterwards); `sqlx::PgListener` buffers
notifications in an unbounded channel and drops with a detached `UNLISTEN`
task, accepted as bounded in practice (ADR 0031, "Accepted residuals and
capacity edges").

## Trusted proxies and the client address review

Review date: 2026-10-20
([ADR 0032](adr/0032-trusted-proxies-and-client-ip.md), port of Forge-Go ADR
0037). Scope: `FORGE_HTTP_TRUSTED_PROXIES`, the resolver the HTTP server runs
before every layer, and the counters that read `Request::client_ip`
(sign-in and reset throttles, `by_client_ip`, the anonymous stream cap).

| Threat | STRIDE | Control | Verification |
| --- | --- | --- | --- |
| A client that connects directly forges `X-Forwarded-For`, `X-Real-IP` or `Forwarded` to pick a fresh throttle, limiter or stream key | S, D | The header is read only when the TCP peer is inside a listed network; the default list is empty (trust nobody); `X-Real-IP` and `Forwarded` are never read | `resolves_like_forge_go` (untrusted-peer cases), `trusting_nobody_uses_the_peer`, `other_forwarding_headers_are_never_read`, `server_resolves_the_client_from_trusted_proxies_only_before_every_layer`, `without_trusted_proxies_forwarding_headers_are_ignored`, `direct_clients_cannot_choose_their_client_ip_bucket` |
| A client behind a trusted proxy prepends entries to escape its counters | S, D | The walk starts at the right (the entry the proxy appended), skips only listed proxies and stops at the first unlisted address; entries to its left are never read | `client-supplied prefix ignored` cases, `clients_behind_a_trusted_proxy_get_independent_sign_in_counters` (TCP), `clients_behind_one_trusted_proxy_have_their_own_sign_in_counters` (generated), `anonymous_viewers_behind_a_trusted_proxy_are_counted_per_client` |
| Malformed or overlong chains resolve to a chosen value or cost unbounded work | S, D | An empty or non-address entry on the walk (port, brackets, `for=`, non-UTF-8, empty zone) and more than 32 listed hops resolve to the peer; the walk stops at the first untrusted, malformed or 33rd entry | malformed cases, `non_utf8_entries_fall_back_to_the_peer_only_when_reached`, `overlong_trusted_chains_fall_back_to_the_peer`, `the_result_is_always_the_peer_or_an_entry_of_the_header` (20 000 generated chains) |
| A client rotates through its IPv6 `/64` behind the proxy | S, D | The resolver returns the full canonical address (zone dropped, IPv4-mapped as IPv4); every counter buckets IPv6 by `/64` | `client_ip_keys_group_ipv6_by_64_and_keep_the_peer_key_the_peer`, broadcast and TCP `/64` rotation tests |
| A trusted-proxy list that trusts clients | S | Empty or malformed entries, `/0`, IPv4 prefixes shorter than `/8`, IPv6 prefixes shorter than `/16`, IPv4-mapped prefixes and zoned addresses stop startup; `TrustedProxy::new` is the only way to make a trusted network, so deserialized and hand-built configurations obey the same rules; errors name the entry position, never its text | `trusted_proxies_reject_invalid_entries_without_echoing_them`, `trusted_proxy_rules_apply_to_programmatic_values`, `trusted_proxies_deserialize_through_the_same_rules`, `unsafe_trusted_proxies_stop_the_configuration_without_echoing_them`, e2e script |
| A layer overwrites the resolved address | T | The address is a private field of `Request` that only `TrustedProxies::apply` sets; the server applies it before the first layer | `server_resolves_the_client_from_trusted_proxies_only_before_every_layer` |

Residual risks: anything inside a listed network (other containers on the
same Docker network, accessories, a compromised dependency in one of them,
host processes through the network's gateway address) can send requests with
a chosen `X-Forwarded-For` directly to the application and pick its key,
including spending other clients' per-source budgets; operators must list
only proxy networks and keep untrusted services off them. An application also
reachable directly from a listed network trusts that network's headers. A
proxy that forwards the client's header unchanged lets clients choose their
address. The minimum prefix lengths stop only obvious mistakes (a `/8` still
trusts 16 million addresses). Forge-Rust has no `forge deploy` yet, so the
value is the operator's to set; audit records carry no source digest yet.

## Telemetry review

Review date: 2026-10-20 ([ADR 0033](adr/0033-telemetry.md), port of Forge-Go
ADR 0034 and 0040). Scope: `FORGE_TELEMETRY_*`, the HTTP server
instrumentation, the job and outbox instruments, the OTLP exporter, the log
export layer and the Prometheus listener.

| Threat | STRIDE | Control | Verification |
| --- | --- | --- | --- |
| A client picks the trace IDs in the logs, forces recording past the sample ratio or pushes `tracestate` downstream | S, T, D | Incoming `traceparent`/`tracestate` are ignored unless `FORGE_TELEMETRY_TRUST_TRACEPARENT=true`; parsing is strict (lowercase hex, exact lengths, no zero ids, one `traceparent` header, W3C `tracestate` grammar, 32 members, 512 bytes) | `incoming_traceparent_is_ignored_unless_trusted`, `invalid_traceparents_are_refused`, `tracestate_is_validated`, `extract_requires_exactly_one_traceparent`, `parsers_never_panic_on_arbitrary_input` |
| Personal data or secrets reach spans or metrics | I | Spans and labels hold the method, route template, status, outcome, queue and job names and ids; never paths, queries, headers, cookies, client addresses, bodies, SQL, payloads or error messages (errors become `error.type`) | `metrics_use_route_templates_and_status_classes`, `server_spans_and_correlated_logs_are_exported`, `attempts_are_traced_and_measured_by_outcome`, `deliveries_are_traced_and_measured_by_outcome`, e2e script |
| Exported log records carry more than standard output | I | The log layer sits behind the same `FORGE_LOG` filter, records only event and span fields formatted as the formatter formats them (`SecretString` is `[REDACTED]`), and is opt-in | `logs_are_exported_with_trace_context_and_redacted_values` |
| Request input explodes metric cardinality (memory, backend cost) | D | Route templates, empty route for unmatched requests, `_OTHER` methods, status classes; 1000 series per instrument then one `_overflow` series and a counter; label values cut at 128 bytes | `series_beyond_the_limit_fold_into_overflow`, `label_values_are_cut`, `methods_and_statuses_are_bounded` |
| A slow or broken collector slows requests or exhausts memory | D | Bounded queues (2048) that drop and count without waiting; exports run on owned tasks with a hard timeout; shutdown flush is bounded by the remaining grace period | `full_queue_drops_and_counts_without_waiting`, `a_full_queue_drops_and_counts_without_blocking`, `shutdown_is_bounded_by_its_deadline` |
| A failing log export logs its failure, which is exported and fails again | D | Export diagnostics use the `forge_telemetry` target, which the log layer never exports | `a_failing_log_export_does_not_feed_itself` |
| Collector or vendor credentials leak | I | `FORGE_TELEMETRY_OTLP_HEADERS` values are `SecretString`, sent as sensitive header values; configuration and export errors never contain the endpoint, headers or response bodies; redirects are not followed | `header_values_are_redacted_and_never_echoed`, `otlp_needs_a_valid_endpoint_that_is_never_echoed`, `spans_are_exported_as_otlp_json`, e2e script |
| Telemetry is read or altered in transit | I, T | `production` requires an `https` endpoint (rustls, webpki roots) unless `FORGE_TELEMETRY_OTLP_ALLOW_PLAINTEXT=true` confirms a sidecar or private network | `production_requires_https_unless_plaintext_is_allowed` |
| Anyone reads the metrics endpoint | I | Off by default; a separate listener never mounted on the application router; non-loopback addresses refused in production unless `FORGE_TELEMETRY_METRICS_PUBLIC=true`, warned elsewhere; only `GET`/`HEAD /metrics`, 16 connections | `metrics_listener_rules`, e2e script (`/metrics` on the application port is 404) |
| The development span log leaks into production | I | `FORGE_TELEMETRY_EXPORTER=log` is refused in production | `log_exporter_is_refused_in_production` |
| Local observability stack (`compose.observability.yaml`, [ADR 0034](adr/0034-local-observability-stack.md)) exposed on the network or opened with a guessable credential | I, S, E | Every published port binds `127.0.0.1` (Grafana, Prometheus, the collector's OTLP); Tempo and Loki publish nothing. Grafana's admin password has no default (`GRAFANA_ADMIN_PASSWORD` is required with `${...:?}`), anonymous access and sign-up are off, nothing phones home, images are pinned by version and digest, and configuration is mounted read-only and holds no secrets. **Residual:** the password gates Grafana only; Prometheus (`127.0.0.1:9090`, OTLP write receiver on) and the collector's OTLP (`4318`) accept any local process, which can read metrics or inject telemetry. A single-developer local stack: do not run it on shared hosts | `the_overlay_is_opt_in_and_loopback_only`, `grafana_is_secured_by_default_and_nothing_holds_a_secret`, e2e script (anonymous Grafana API access is 401) |
| Secrets in the local stack's data | I | Logs reach Loki as standard output shows them (secrets redacted only where the formatter redacts them), kept seven days in a named volume and readable by Grafana users; the overlay's plaintext OTLP (`FORGE_TELEMETRY_OTLP_ALLOW_PLAINTEXT=true`) travels only on the compose project's network. `docker compose ... down -v` deletes the data. Never log secrets | `the_app_settings_are_valid_framework_configuration` |

Residual risks: with `FORGE_TELEMETRY_LOGS_EXPORTER=otlp` every log line goes
to the collector, which must be protected like the log store; a raw secret
logged by application code leaks there as it does to standard output.
Attributes added by application code are the application's responsibility.
The metrics listener has no authentication: confirming it public relies on
the network keeping it private. Trusting `traceparent` is safe only behind a
gateway that sets or sanitizes it. Dropped spans and log records under load
are counted, not retried.

## Mail review

Review date: 2026-10-20 ([ADR 0035](adr/0035-mail.md), port of Forge-Go ADR
0024). Scope: `forge-mail` (message validation and encoding, the SMTP,
file and test senders, previews, templates, the password reset adapter) and
`FORGE_MAIL_*` / `FORGE_AUTH_PASSWORD_RESET_URL`.

| Threat | STRIDE | Control | Verification |
| --- | --- | --- | --- |
| Header, recipient or MIME injection through user data (CRLF in a name, subject, address, header or file name) | T, S | Every sender encodes first; control characters, U+2028/2029, U+FFFD and direction overrides are refused in every header-bound string; addresses are ASCII dot-atom with an LDH domain; extra headers from a seven-name allow-list, printable ASCII; `Bcc` envelope-only; random 128-bit boundaries; errors name the field, never the value | `header_injection_is_refused_everywhere`, `only_allow_listed_headers_once`, `errors_name_the_field_never_the_value`, `bcc_is_envelope_only_and_ids_use_the_sender_domain`, golden parity tests against Forge-Go |
| Encoded-word spoofing (`=?utf-8?b?...?=` typed as text) and direction-override file names (`invoice\u{202E}fdp.exe`) | S | ASCII text containing `=?` is base64-encoded whole; such file names are refused, directories stripped | `encoded_word_lookalikes_are_encoded_whole`, `lookalike_names_and_quoted_printable_edges`, `attachments_are_checked` |
| SMTP downgrade (stripped STARTTLS) or certificate spoofing | I, S | `smtp://` requires STARTTLS, `smtps://` implicit TLS; certificates and names always verified (Mozilla roots plus `FORGE_MAIL_TLS_CA_FILE`), no skip switch; a TLS failure never falls back; STARTTLS is used when offered even with the plaintext flag | `a_server_without_starttls_is_refused_before_auth_or_mail`, `certificates_are_verified_and_failures_never_fall_back`, `offered_starttls_is_used_even_with_the_plaintext_flag` |
| STARTTLS command injection (bytes sent before the handshake read as protected) | T | Any byte buffered after the `220` to STARTTLS fails the send | `bytes_injected_before_the_handshake_are_refused` |
| SMTP password disclosure | I | Credentials are `SecretString`; AUTH PLAIN/LOGIN only over TLS, or in clear to a loopback host with `FORGE_MAIL_ALLOW_PLAINTEXT`; production refuses the flag with credentials for a remote host; credentials with control characters refused; `Debug`, errors and config errors never contain them; server reply text is discarded | `plaintext_never_carries_credentials_to_a_remote_host`, `plaintext_to_loopback_is_allowed_only_with_the_flag`, `production_refuses_plaintext_with_remote_credentials_and_previews`, `malformed_ambiguous_or_parameterised_urls_are_refused_without_echoing_them`, `configuration_is_validated_and_redacted`, `a_refused_recipient_aborts_before_data_and_reply_text_is_dropped` |
| A slow or hostile mail server stalls workers or exhausts memory | D | One deadline per delivery (`FORGE_MAIL_TIMEOUT`, at most 10 min) and a 10 s connect; reply lines bounded to 2048 bytes and 128 lines; message size limits; no spawned task, dropping the future closes the socket; reset delivery runs on the bounded reset worker queue | `a_silent_server_hits_the_deadline`, `structure_and_limits`, `a_closed_port_is_transient` |
| Duplicate or partial delivery | T | A refused recipient aborts before `DATA`; after `DATA` is accepted a failing or hanging `QUIT` is success | `a_refused_recipient_aborts_before_data_and_reply_text_is_dropped`, `a_lost_quit_after_acceptance_is_still_success`, `a_hanging_quit_after_acceptance_is_still_success` |
| Reset token or recipient leaks into logs, metrics or errors | I | The token appears only in the email body link; failures log the error code, step and SMTP stage only; metrics carry the outcome code only | `failures_are_logged_without_recipient_link_or_token`, `instrumented_sends_record_the_outcome_code` |
| Reset link points at an attacker or travels in clear | S, I | `FORGE_AUTH_PASSWORD_RESET_URL` and the adapter require https (http only on localhost), no credentials, query or fragment; links go to the stored login only (ADR 0026) | `reset_urls_are_absolute_https_or_local_http`, `reset_urls_must_be_https_or_local`, generated `reset_emails_carry_a_working_link_through_the_mailer` |
| Half-configured mail silently drops resets | D | `FORGE_MAIL_URL` without `FORGE_MAIL_FROM` or the reset URL stops start-up; the reset email is probed at start-up | `a_url_needs_a_valid_sender`, `mail_without_a_reset_url_stops_start_up`, `settings_are_validated_with_a_probe` |
| Development senders or previews in production | I, E | The file sender and `FORGE_MAIL_PREVIEW` are refused in production by configuration loading, `from_config` and `PreviewHandler::new` (both switches required); `.eml` files are `0600` in a `0700` directory, created with `O_EXCL` | `file_urls_name_a_directory_outside_production`, `hand_built_production_configs_cannot_use_the_file_sender`, `previews_need_development_or_test_and_the_explicit_flag`, `messages_become_private_eml_files_with_the_envelope`, `existing_paths_are_never_followed` |
| Script in a previewed email runs in the developer's browser | E | Preview pages are askama-escaped under `default-src 'none'`; the HTML part is served under a `sandbox` CSP inside a sandboxed iframe; `nosniff`, `no-store`, `GET`/`HEAD` only, whole-segment prefix | `a_preview_page_escapes_everything_and_frames_html_in_a_sandbox`, `routing_is_by_whole_segments_and_read_only` |
| Template data injects markup into the HTML part | T | HTML parts are askama `.html` templates (auto-escaped); a rendered subject is validated as a header; rendering is buffered | `html_is_escaped_and_text_is_not`, `subject_injection_through_template_data_is_refused_at_encoding`, `a_failing_part_renders_nothing` |

Residual risks: an application can still render an HTML part from a template
that disables escaping or from raw strings; the development file sender
depends on `FORGE_ENV` (an unset value means development), mitigated by the
start-up warning; mail is not DKIM-signed by Forge (the relay's job); no
SMTPUTF8 or IDNA, so non-ASCII addresses are undeliverable rather than
converted; the test CA and key under `crates/forge-mail/tests/fixtures/tls`
are public test material and trusted only by the tests.

## Internationalization review

Review date: 2026-10-20 ([ADR 0037](adr/0037-internationalization.md), port
of Forge-Go ADR 0042 and its review follow-ups). Scope: `forge-i18n` (catalog
loading, translation, plural rules, locale matching, formatting),
`LocaleNegotiation`, `I18n` and `Translation` in `forge-views`,
`FORGE_I18N_DEFAULT_LOCALE`, `forge i18n check` and the generated
translations. Guide: [i18n](i18n.md).

| Threat | STRIDE | Control | Verification |
| --- | --- | --- | --- |
| Path traversal through a locale name (`Accept-Language`, the `locale` cookie, `?locale=`, a path prefix, `FORGE_I18N_DEFAULT_LOCALE`) | T, I | Locale names are validated against a BCP 47 subset (`language[-Script][-REGION]`, ASCII letters and digits only) and canonicalized before any use; only locales loaded at start-up from the files embedded at build time can be selected, so request input never reaches the file system. File names must be canonical (`pt-br.json` is refused); the build helper and the directory loader refuse symbolic links | `refuses_everything_else` (`../`, `%2e%2e`, `/`, `\`, NUL, `_`), `file_names_carry_a_canonical_locale`, `negotiation_follows_the_forge_go_order` (traversal cookie and parameter), `i18n_default_locale_is_validated_and_canonical`, `directories_are_read_flat_and_links_are_refused`, `locales_are_embedded_flat_sorted_and_without_links` |
| XSS through translated text or its arguments | T, E | `Translation` renders HTML: a plain key's text and arguments are escaped, and it is `HtmlSafe`, so `|safe` cannot make it markup; only keys ending in `_html` are markup, and in them every argument is escaped unless passed with `with_markup`; a missing `_html` key is never markup; the missing-key text is escaped. `.text()` is plain text that Askama escapes where it is rendered (flash) | `html_escapes_interpolation_outside_trusted_keys`, `templates_escape_every_argument_and_plain_keys_are_never_markup` (element, attribute, `_html`, `|safe`), generated `every_locale_translates_the_welcome_page` |
| Recursive or injected interpolation through argument values | T | Interpolation is one pass over the translation; values are never re-scanned for `%{...}` | `interpolation_is_not_recursive` |
| Memory or log exhaustion through missing keys built from input | D | Missing-translation warnings are logged once per locale and key, for at most 1,024 distinct problems, then one "missing-translation warnings suppressed" line and a counter (`suppressed_warnings`); keys longer than 256 bytes or with invalid segments are refused before lookup and truncated in logs | `missing_translation_warnings_are_bounded_and_the_cap_is_announced`, `missing_translations_in_production_fall_back_and_log_once` |
| Oversized or hostile request headers | D | `Accept-Language` over 4096 bytes is ignored; only its first 32 entries are parsed; tags over 64 bytes, malformed q-values and subtags are skipped | `accept_language_parsing_matches_forge_go` |
| Path prefix confusion between the routed and the escaped path | T, E | The URI holds only the escaped path, which routing matches; the locale segment is read from it and must be an available locale, so `/pt-BR%2F..%2Fadmin` is not stripped; the rest of the path and the query are kept byte for byte | `the_layer_binds_the_locale_and_strips_the_prefix` |
| Oversized or ambiguous translation files | D, T | 1 MiB per file, 512 files, 16 levels, 50,000 keys per locale; duplicate keys (in one object or across files, which `serde_json` alone would overwrite) and a second top-level key are start-up errors naming the files, so review sees exactly what is served | `unsafe_or_ambiguous_files_are_refused` |
| Shared caches serve one visitor's language to another | I | Responses that used the request's translations carry `Content-Language` and `Vary: Accept-Language, Cookie` (merged with an existing `Vary`, never duplicated); assets and JSON are not marked and stay cacheable | `only_translated_responses_carry_content_language_and_vary`, `vary_is_merged_not_duplicated`, generated `turbo_and_stimulus_are_served_digested` |
| Path guard bypassed by the locale path prefix (`with_path_prefix`) | E | The prefix is removed in the router's rewrite step (`Middleware::rewrite`), which runs for every layer before the first layer handles the request, so a guard registered before or after `LocaleNegotiation` sees `/admin` for `/pt-BR/admin` (and `/PT-br/admin/users`), exactly what the router serves; the segment is removed once per request, so `/pt-BR/en/admin` is `/en/admin` and not `/admin` | `a_locale_prefix_cannot_bypass_a_path_guard_registered_before_the_negotiation` (both layer orders), `rewrites_run_before_every_layer_wherever_they_are_registered`, `the_layer_binds_the_locale_and_strips_the_prefix`; documented in [i18n](i18n.md#choosing-the-locale) |
| Open redirect through the sign-in return path with a locale prefix | T | `LocaleNegotiation` keeps the prefix it removed as an available locale's canonical name (`path_prefix`, never request text) and `SignInRedirect` puts it back in front of the stripped path; the result must pass `return_path` (single leading `/`, visible ASCII, no backslash) here and again when the form posts and redirects, otherwise the unprefixed path is used. Tested with `//evil`, `/pt-BR//evil`, backslash and encoded-slash variants, a case variant, prefix on and off, and both layer orders | `the_locale_prefix_survives_the_sign_in_redirect`, `the_locale_prefix_is_put_back_only_in_a_local_path`, `the_layer_keeps_the_canonical_prefix_it_removed` |
| Locale cookie tampering | T | The cookie only selects among available locales; invalid values are ignored; `locale_cookie` writes only an available locale, `Secure`, `HttpOnly`, `SameSite=Lax`, `Path=/`, a year | `the_locale_cookie_is_strict_and_only_names_available_locales` |
| Wrong-language or missing text hides a security message in production | I | Missing translations fall back to the default locale, then the humanized key, and are logged; development and test show `translation missing: LOCALE.KEY`, which the generated tests reject; `forge i18n check` exits 1 on any missing key or plural form. The failed sign-in message stays one key (`sessions.invalid_credentials`) for every wrong field and the reset answer one key (`passwords.reset_requested`), in every locale, so translation cannot reintroduce account enumeration | `the_pages_are_translated_in_every_locale`, `generated_applications_are_complete_in_every_locale`, `reports_missing_plural_and_unused_keys` |

Residual risks: `_html` translations are trusted code; a malicious or
careless translator can inject markup, and an argument interpolated into
`href="%{url}"` of an `_html` translation is HTML-escaped but not URL-checked
(`javascript:` is not filtered, as in Rails), so authors must never pass a
user-supplied URL there and review translation files like templates (the
page CSP still blocks inline scripts). A translated response built without
the request's `I18n` (or before the negotiation layer) gets no `Vary` unless
the handler calls `set_content_language`; so does a response that depends on
the locale only through `LocaleNegotiation::negotiate()` (a locale redirect),
which cannot mark it, and a shared cache can serve it across languages until
the handler marks it. Live-update fragments render in
the default locale for every subscriber.

## OpenAPI review

Review date: 2026-10-20 ([ADR 0039](adr/0039-openapi-from-typed-routes.md),
port of Forge-Go ADR 0043). Scope: `forge-openapi`, `forge-macros`, the
`forge-http` descriptions, `CheckResponses` and `/openapi.json`, the
`openapi` command, `FORGE_OPENAPI_ENABLED`. Guide: [OpenAPI](openapi.md).

| Threat | STRIDE | Control | Verification |
| --- | --- | --- | --- |
| Reconnaissance: the document lists every described route, internal and admin ones included, with the permission each requires | I | `GET /openapi.json` is served only in development and test unless `FORGE_OPENAPI_ENABLED=true`, which logs a startup warning in production; `false` never serves it; the variable is a strict boolean in `KNOWN_KEYS` | `openapi_is_served_in_development_and_test_unless_configured`, `the_document_is_served_where_the_configuration_allows_it` |
| An application's own `/openapi.json` (a hand-written or filtered document) silently replaced | T | Forge mounts its document only when no `GET /openapi.json` route exists and logs that it did not | `the_document_is_served_where_the_configuration_allows_it` |
| A wrong description misleads clients or reviewers (a field that is not written, a missing error) | T, R | Typed handlers derive request and success from their types; the derive follows serde's attributes and refuses at compile time those it cannot describe; inaccurate operations are build errors that stop startup; generated tests check every JSON response with `CheckResponses` | `shapes_it_cannot_describe_are_compile_errors`, `inaccurate_descriptions_are_build_errors`, `an_inaccurate_description_stops_startup`, generated `json_responses_match_the_document` |
| Secrets in the document (examples, descriptions) | I | The document holds only what code declares: types, names, and `doc`/`example` strings an author writes; no configuration value, credential or request data is ever included | review rule: never put a credential or real customer data in `example` |
| Resource exhaustion when serving or validating | D | The document is built once at startup and served as fixed bytes; the contract validator is test-only, bounds recursion depth at 256 and only follows references inside the document | `responses_are_checked_against_the_document` |
| Supply chain of the derive | T | `forge-macros` uses only `syn`, `quote`, `proc-macro2`, already locked for `serde_derive`; it runs at compile time with no I/O, no network and no build script | `cargo deny check`, [dependency policy](dependency-policy.md#openapi-dependency-decision) |

Residual risks: `describe` descriptions are claims about a handler and can
still be wrong where tests do not exercise a response; enabling the
document in production publishes the API surface, which is a deliberate
operator decision.

## Two-factor authentication review

Review date: 2026-10-20 ([ADR 0040](adr/0040-totp-multi-factor-authentication.md),
port of Forge-Go ADR 0014 and 0015). Scope: `forge_auth::mfa`,
`forge_tenancy::MfaService`, `FORGE_AUTH_MFA_KEY`, the generated `PgMfaStore`,
migration `00000000000000000004_forge_mfa` and the `/session/mfa` and `/mfa`
pages. Guide: [two-factor authentication](mfa.md).

| Threat | STRIDE | Control | Verification |
| --- | --- | --- | --- |
| A stolen password signs in | S | An account with a factor gets only a five-minute challenge, never a session, from the password; without `FORGE_AUTH_MFA_KEY` it cannot sign in at all | `two_factor_sign_in_takes_a_code_once_and_a_backup_code_once`, `without_the_key_an_enrolled_account_cannot_sign_in`, `without_a_key_every_operation_but_status_fails_closed` |
| Code guessing | S | Five attempts per account and per account and source, 60 per source, in five minutes, counted before any code is checked; `429` with `Retry-After`; one message for every wrong code | `attempts_are_throttled_per_account_and_per_source_before_the_code_is_checked`, `second_factor_attempts_are_throttled` |
| Replay of an observed code or backup code, also concurrently | S | The accepted period is recorded and only a later one is accepted; the challenge and the period (or the backup code) are consumed in one locking transaction | `a_challenge_completes_once_and_its_code_is_never_replayed`, `backup_codes_complete_a_challenge_once`, `two_factor_codes_and_challenges_work_once_and_fail_closed` (two racing requests, one succeeds), generated end to end in `scaffold.rs` |
| A challenge cookie stolen or planted into another browser | S, E | The challenge is bound to the digest of a fresh CSRF token issued with it; the code step compares it in constant time; the cookie is `__Host-`, `HttpOnly`, `Secure`, `SameSite=Lax` | `wrong_codes_expired_challenges_and_foreign_browsers_are_refused`, `a_challenge_cookie_carried_to_another_browser_is_refused` |
| A challenge outliving a password reset, a disabled account, a suspended membership or a factor change | E | Every challenge query requires the credential version the password step verified, an enabled account, an active membership and the same factor; a new sign-in and every factor change void pending challenges | `a_new_challenge_voids_the_old_one_and_password_changes_void_both`, `rotation_keeps_the_old_factor_until_confirmed_and_disable_removes_it`, `two_factor_codes_and_challenges_work_once_and_fail_closed` |
| A hijacked session turns MFA off or swaps the authenticator | E | Turning on and confirming the key need the password; rotation and its confirmation, new backup codes and turning off need the password and a current code (used up), throttled, and a password-only step-up never clears the counters of an account with a factor; the pending key is shown only after step-up; CSRF is checked by the layer and bound to the session; changes sign out everywhere | `changing_the_factor_needs_the_password_and_a_current_code`, `step_up_codes_are_single_use_and_regeneration_replaces_every_backup_code`, `a_session_alone_can_neither_see_nor_confirm_a_pending_key`, `confirming_a_new_authenticator_needs_the_password_and_a_current_code`, `password_only_step_ups_do_not_reset_the_code_throttle` |
| A session holder tests passwords on the two-factor settings of an enrolled account | I | `POST /mfa` and `POST /mfa/confirm` need no code, so a wrong password would answer `422` and a right one `303`. The step-up counts the attempt, then refuses an account that already has a factor before it checks the password: both get the same `303` back to `/mfa`, and both are audited as `denied` (Forge-Go#32) | `enrollment_routes_do_not_reveal_the_password_of_an_enrolled_account` |
| A pending rotation confirmed as an enrollment, or a factor change that misses a factor installed meanwhile | T, E | The store locks the account row, then looks for a factor in a separate statement, so it sees a factor committed by the transaction it waited for; an enrollment of an account with a factor is refused explicitly (`AlreadyEnabled`) and leaves the factor, the pending rotation, the backup codes and the sessions untouched; the pending seed is read only with the kind the account's factor implies (one statement), so an enrollment confirmation never checks, nor shows again, a rotation's seed | `an_enrollment_confirmation_of_an_account_with_a_factor_is_refused`, `a_pending_seed_is_never_checked_or_shown_as_the_other_kind`, `factor_changes_are_refused_under_the_account_lock`, `step_up_backup_codes_are_used_up_only_by_a_change_that_commits` (generated, PostgreSQL) |
| A sign-in racing a factor change deadlocks (`40P01`, a `503`) | D | Every MFA transaction locks the account row before any MFA row: factor changes `FOR UPDATE`, issuing and completing a challenge `FOR NO KEY UPDATE`, so they serialize per account and never wait while holding an MFA row (the argument is in the generated `accounts` module); session rotation (`FOR KEY SHARE`) and reset redemption (`FOR UPDATE`) lock the account row first too | `sign_in_steps_lock_the_account_before_any_mfa_row` (generated, PostgreSQL: the change takes the factor, codes and challenges `NOWAIT` while the step waits), `rotating_a_session_waits_for_a_factor_change_instead_of_deadlocking`, `two_reset_links_of_one_account_redeemed_together_do_not_deadlock` |
| A session issued after a factor change or a password reset revoked every session | S, E | The second-factor session is stored, then checked against the factor the code belonged to and the credential version (the insert waits for the account row those changes lock); a stale one is revoked before its cookie is sent | `a_sign_in_is_stale_once_the_factor_changes`, `a_factor_change_during_a_second_factor_sign_in_ends_its_session` (generated, PostgreSQL) |
| A step-up proof reused for another account, or a change made without one | E | `StepUpProof` comes only from `verify_step_up`, is bound to its account, and is a required argument of every change of an existing factor; a proof of another account is `StepUpInvalid` | `a_step_up_proof_of_another_account_is_refused` |
| A failed factor change spends the backup code given for step-up | D | `verify_step_up` checks a backup code without using it; the change uses it up in its own transaction, so a wrong code of the new key, an expired rotation or a changed account keeps it, and a code used meanwhile refuses the change (`StepUpInvalid`) | `a_failed_change_keeps_the_step_up_backup_code`, `step_up_backup_codes_are_used_up_only_by_a_change_that_commits`, `confirming_a_new_authenticator_needs_the_password_and_a_current_code` |
| A password-only session survives a factor turned on during the sign-in | S, E | The factor change revokes every session, but a sign-in that checked for a factor just before could store its session after that. The sign-in checks again once its session is stored, ends it and asks for the code; on PostgreSQL the session insert's foreign key waits for the account row the change locks, so the second check sees the factor | `a_factor_installed_during_a_sign_in_ends_its_password_only_session`, `factor_changes_are_refused_under_the_account_lock` (the insert waits for the lock) |
| A session holder locks the owner out of `/mfa` | D | Step-ups count per account (five in five minutes) whoever sends them, so a session holder can spend the budget with wrong passwords and make the owner's changes answer `429` until the window ends. **Accepted:** it lasts at most five minutes, blocks only the two-factor changes (sign-in has its own counters), and counting per session instead would give a stolen session unlimited guesses at the password and codes (Forge-Go records the same) | `password_only_step_ups_do_not_reset_the_code_throttle`, `second_factor_attempts_are_throttled` |
| Database disclosure reveals seeds, codes or challenges | I | Seeds are AES-256-GCM ciphertext under a key that is never in the database; backup codes and challenge tokens are SHA-256 digests; the runtime role can only mark codes and challenges used | `sealed_seeds_round_trip_and_fail_closed_on_tamper_or_wrong_key`, migration grants |
| Tampered ciphertext, a swapped seed or a wrong key accepted | T | AEAD authentication plus the seed digest compared after decryption; every failure is the same opaque error and refuses the code | `sealed_seeds_round_trip_and_fail_closed_on_tamper_or_wrong_key`, `a_wrong_key_or_tampered_seed_fails_closed`, the PostgreSQL test that flips a stored byte |
| A weak or malformed key | T, I | `FORGE_AUTH_MFA_KEY` must be canonical unpadded base64url of 32 bytes; otherwise startup stops in every environment; the value is redacted in errors, `Debug` and serialized configuration | `the_mfa_key_is_optional_canonical_and_never_echoed`, `keys_must_be_canonical_unpadded_base64url_of_32_bytes` |
| Seeds, codes or tokens in logs, errors or caches | I | Every secret type redacts `Debug`; errors carry no secret; key and backup-code pages are `no-store` with `Referrer-Policy: no-referrer`; audit events hold no codes; seeds, provisioning links, backup codes and challenge tokens, and the buffers that held a seed, are wiped on drop (best effort: rendered copies are not) | unit tests on `Debug` output, `enroll_ada` (`no-store`), `secret_material_is_wiped_when_dropped` |
| Repudiation of factor changes and second-factor use | R | Forge-Go's audit events (`auth.mfa.enrollment_started`, `enabled`, `rotation_started`, `rotated`, `disabled`, `challenge_succeeded`, `challenge_failed`, `backup_code_used`) plus `backup_codes_regenerated`; refused step-ups record the action as `denied` | `audited(...)` assertions in the generated HTTP tests |

Residual risks: an attacker who knows the password can spend the
account's five attempts and delay the owner's code step for five minutes;
timing on the step-up routes: on the routes that need a code, a right
password with a wrong code costs a factor lookup more than a wrong password,
and on `POST /mfa` and `POST /mfa/confirm` an account without a factor pays
for the password hash while one with a factor does not (which `GET /mfa`
shows its session holder anyway), both bounded by the step-up throttle (five
per account in five minutes); new backup codes neither sign out other
sessions nor void a pending challenge (as in Forge-Go); TOTP codes can be
phished in real time (no origin binding, unlike WebAuthn);
expired enrollments and consumed challenges are not pruned yet; rotating
`FORGE_AUTH_MFA_KEY` needs a custom `SecretCipher`; losing the key locks every
enrolled account out (fail closed by design).

## Credentials and attribute encryption review

Review date: 2026-10-20 ([ADR 0041](adr/0041-encrypted-credentials-and-attribute-encryption.md),
port of Forge-Go ADR 0036). Scope: `forge-credentials`, `forge-encryption`,
`forge credentials init|edit|show|rotate-key` and the development credentials
of `forge new`. Guides: [credentials](credentials.md), [encryption](encryption.md).

| Threat | STRIDE | Control | Verification |
| --- | --- | --- | --- |
| Key committed to the repository or the image | I | `init` and `forge new` add `/config/credentials/*.key` to `.gitignore` and `config/credentials/*.key` to `.dockerignore`; the key file is 0600 and a missing directory is created 0700; `init` never prints a key. **Residual:** `git add -f` is not prevented; rotate a key that was ever committed | `init_creates_a_private_key_the_file_and_ignore_entries_once`, `new_applications_get_development_credentials` (the key appears in no other generated file) |
| Key exposed or replaced through a key file | I, T | Group/other permission bits, symbolic links and non-regular files are refused; a credentials directory that group or others can write is refused (`chmod 700`) for reading and writing keys; files open with `O_NOFOLLOW \| O_NONBLOCK` and are checked with `fstat`, so a FIFO is refused instead of blocking; errors never echo key text. **Residual:** `FORGE_MASTER_KEY` is as visible as any environment variable; parent directories of `config/credentials/` are not checked | `exposed_or_linked_key_files_are_refused`, `writable_credentials_directories_are_refused`, `fifos_at_credentials_paths_are_refused_without_blocking`, `key_lists_parse_like_forge_go_and_never_echo_input`, `show_fails_closed_with_a_wrong_or_exposed_key` |
| Plaintext left by `credentials edit` (temporary, swap, backup, undo, viminfo files) | I | 0600 file in a fresh 0700 directory, `/dev/shm` on Linux; the whole directory is zeroed and removed by a drop guard on success, editor failure, invalid input, write failure and `SIGTERM`/`SIGHUP` (caught for the whole command); Vim-family editors run without swap, backup, undo and viminfo; the editor gets no `FORGE_*` variable and no shell. **Residual:** `SIGKILL`, `SIGQUIT` (default action, core dump where enabled) and power loss; overwriting is best effort on copy-on-write disks; other editors may keep copies; the process memory holds the plaintext while the editor runs | `a_failed_editor_leaves_the_original_intact_and_removes_the_temporary_file`, `termination_during_or_outside_the_editor_cleans_up`, `sigterm_during_the_edit_wipes_the_plaintext_and_keeps_the_credentials` (real process), `edit_runs_the_editor_without_forge_variables_and_wipes_its_file`, `private_directories_are_owner_only_and_wiped` |
| Edit lost or torn by a concurrent change or a crash | T, D | Atomic replace (temporary file, `fsync`, `rename`); a file changed while the editor was open is not overwritten; invalid JSON, duplicate keys and non-objects are refused before writing | `a_file_changed_while_editing_is_not_overwritten`, `invalid_documents_are_refused_without_echoing_them`, `writes_are_atomic_and_exclusive_unless_replacing` |
| Another environment's file swapped in | T, S | Environment, format version and key id are associated data taken from the caller; the swap fails even with a shared key. **Residual:** an older valid file of the same environment decrypts | `sealed_files_open_only_with_their_key_and_environment`, `rust_opens_a_credentials_file_written_by_go` |
| Tampered, truncated or downgraded credentials file | T | AES-256-GCM; strict envelope (known fields, version 1, no trailing data, bounded size); every authentication failure is one opaque error | `tampered_truncated_and_foreign_files_are_refused` |
| Secrets in `show`, logs or errors | I | `show` redacts every value and `--reveal` refuses a non-terminal stdout; `Store` prints `credentials.Store(env, [REDACTED])` through `Debug`, `Display` and serde; errors give a path or a line and column | `show_redacts_by_default_and_reveal_needs_a_terminal`, `stores_never_print_their_content`, `documents_are_validated_before_sealing` |
| Duplicate keys hide a value from review | T | Duplicate keys at any depth are refused on save and on load | `documents_are_one_object_without_duplicates` |
| Missing credential found only at runtime | D | Outside development and test a missing file, key or required path stops startup, listing every missing path | `missing_credentials_fail_closed_outside_development_and_test` |
| Ciphertext copied between rows, columns or tables | T, E | Associated data `(key id, table, column, record id)`; `decrypt` fails; re-encryption stops on a swapped row and names only its id. **Residual:** replaying an old value into the same row is accepted | `a_value_moved_to_another_row_column_or_table_fails`, `a_value_swapped_between_rows_stops_the_rotation_without_leaking_it`, `encrypted_columns_rotate_in_place` (SQLite, PostgreSQL) |
| Equality leaks | I | Always randomized (fresh 96-bit nonce); no deterministic mode; equality only through an opt-in blind index (HMAC-SHA256 under a separately derived key, bound to table and column). **Residual:** equal values and frequencies in an indexed column are visible | `values_round_trip_and_are_randomized`, `blind_indexes_are_keyed_and_bound_to_the_column` |
| Key or nonce misuse, custom cryptography | T, I | `ring` AES-256-GCM, HKDF-SHA256 and HMAC-SHA256 only; 32-byte keys from the OS CSPRNG; one subkey per purpose; random nonces, rotation documented before 2^32 values per key | `derivation_separates_purposes`, Go-compatibility tests (same key ids and blind indexes as Forge-Go) |
| Rotation destroys or reverts data | T, D | Compare-and-set on the value read (concurrent writes win), bounded pages, resumable `last_id`, stops on undecryptable rows; a removed key yields `UnknownKey`, never wrong data. On PostgreSQL an unscoped `EncryptedColumn` checks `relrowsecurity`/`relforcerowsecurity`, ownership and `rolsuper`/`rolbypassrls` before every page and refuses a table whose row-level security hides rows from the role (it would scan 0 rows and report success); `.tenant(id)` runs every page and update in a transaction with `set_config('forge.tenant_id', id, true)`. The guide makes a count of rows not starting with `fg1.<active id>.` (per tenant, or as a role that sees every row) the only proof of completion before an old key is removed; a report never is. Key rings refuse identifiers equal ignoring ASCII case, so the prefix count is exact under MySQL/MariaDB case-insensitive collations. Superusers, `BYPASSRLS` roles and owners of a table without `FORCE` rotate unscoped and see every row. **Residual:** removing a key while that count is not 0 makes the remaining values unreadable; MySQL, MariaDB and SQLite have no row-level security, so their rotation visits every row; a tenant scope sets only `forge.tenant_id`, so on policies that read another setting a scoped run sees no row and reports success (the count catches it) | `rotation_keeps_old_values_readable_then_rewrites_them`, `concurrent_writes_win_and_runs_resume_after_cancellation`, `tenant_tables_rotate_per_tenant_and_refuse_unscoped_runs` (NOBYPASSRLS role, `FORCE` policy), `row_security_applies_only_to_roles_it_binds`, `exempt_roles_rotate_tenant_tables_unscoped`, `identifiers_and_rings_are_validated`, `rotate_key_keep_old_reads_old_data_then_rewrites_it` |
| Key text in configuration errors | I | `KeyRing::parse` names a key by position and the active identifier by length, so a key pasted as an identifier is never echoed | `parse_errors_never_echo_a_key_swapped_with_an_identifier`, `parsed_rings_put_the_active_key_first_and_sort_the_others` |
| Oversized or hostile input | D | Plaintexts 1 MiB, files 2 MiB, key files 4 KiB, at most 8 keys; sizes checked before decoding; `rotate-key` checks the resulting key list before printing or writing anything, and a full list rotates without `--keep-old` | `documents_are_one_object_without_duplicates`, `tampering_truncation_and_wrong_keys_fail_closed`, `rotate_key_with_a_full_key_list_rotates_and_keep_old_fails_cleanly` |
| Memory disclosure | I | Key material, derived keys, decrypted plaintexts and the store's strings are zeroized on drop, as are the buffers `Json<T>` outgrows while encoding and a plaintext whose sealing fails; `String` decoding validates in place. **Residual:** `ring` key schedules, values returned by `open`, `serde_json`'s decoding scratch buffer, application copies, swap and core dumps | unit tests of redacted `Debug`; zeroization is by construction (`Zeroizing`) |

## Deployment review

Review date: 2026-10-20 ([ADR 0042](adr/0042-forge-deploy.md), port of
Forge-Go ADR 0030). Scope: `forge deploy`, `forge logs`, `forge app`
(`forge-cli`, `src/deploy`), the `forge-proxy` crate and image, and the files
they read (`config/deploy.yml`, `.forge/secrets`, `.forge/hooks/*`). Guide:
[deployment](deployment.md#forge-deploy).

Trust boundaries: the operator's machine (configuration, secrets file, local
environment, hooks) is trusted; deploy hosts are reached over SSH with
verified host keys and run Docker as the deploy user (root-equivalent);
clients reach only forge-proxy's ports 80 and 443; the proxy's control socket
is reachable only through `docker exec`; the ACME CA is reached over verified
HTTPS.

| Threat | STRIDE | Control | Verification |
| --- | --- | --- | --- |
| A configuration value injects options or shell syntax into ssh or a remote command | T, E | Local processes run from argument vectors, never a shell; the host follows `--`; hosts, images, service, role, volume, environment and build-arg names are validated against Forge-Go's patterns; every value in a remote script is one single-quoted word | `parse_rejects_invalid_configurations` (option, `;`, `$(...)` and newline injections), `ssh_refuses_invalid_hosts_before_starting_ssh`, `ssh_argv_matches_forge_go_and_never_relaxes_host_keys`, `quoted_words_survive_a_real_shell`, `invalid_versions_are_refused`, `deploys_over_real_ssh` (`it's quoted` reaches the container intact) |
| A man in the middle impersonates a host | S | Host keys always verified (no option relaxes `StrictHostKeyChecking`); `BatchMode=yes`; the operator's `known_hosts` and agent | `ssh_argv_matches_forge_go_and_never_relaxes_host_keys`, the real-sshd test runs with `StrictHostKeyChecking yes` |
| Secrets in argv, `ps`, dry runs, progress, errors or streamed output | I | Secrets reach hosts on stdin only and land in `0600` env files written with `umask 077` and an atomic rename; `.forge/secrets` must be `0600`, has no command substitution and errors never echo values; every printed line and error passes a redactor of all values of 4+ characters | `fails_closed_without_echoing_values`, `remote_errors_are_redacted_on_every_runner_path`, the golden plans (no secret), `deploys_over_real_ssh` (no secret in any docker argv or progress line; env file mode `0600`) |
| A failed health check leaves hosts on mixed versions, or an interrupted deploy is reported as success | T | The switch is all-or-nothing: previous containers keep running until every host passed; on failure every switched host is routed back and new containers removed (exit 4); a failed revert or an interruption exits 7 | `unhealthy_host_reverts_every_switched_host`, `revert_failure_is_inconsistent`, `interrupted_switch_is_inconsistent_without_revert`, `deploys_over_real_ssh` |
| A failed migration or hook still switches traffic | T | Migration failure exits 5 before any container starts; a failing or non-executable hook exits 6 before the next step; the lock is released on every path | `migration_failure_aborts_before_any_switch`, `failing_hook_aborts_before_remote_changes` |
| The schema owner's credential reaches serving containers, or stays on a host | I, E | `migrate.env` overlays the migrate role's environment for the one-off container only; a name in it that is also in `env` or any `servers.*.env` (clear or secret) is a configuration error, as are an empty or null block and `migrate.enabled: false`; the merged environment is written to `~/.forge/SERVICE/env/migrate.step.env` (`0600`, `umask 077`, atomic rename, stdin) on the migrate host only, and removed with its `.tmp` copy after success, a failed migration, a failed or interrupted upload and a signal (the removal is armed before the upload and ignores cancellation, bounded at 30 s); a failed removal is a warning naming the file; a missing secret stops the deploy before any remote command; a failed upload exits 5 before any container runs | `migrate_env_reaches_only_the_migrate_container`, `migrate_env_is_removed_when_the_migration_fails`, `migrate_env_upload_failure_aborts_before_migrating_and_still_removes_the_file`, `migrate_env_is_removed_when_the_deploy_is_cancelled_during_the_migration`, `migrate_env_removal_failure_is_a_warning`, `a_missing_migrate_secret_fails_closed_before_any_remote_command`, `migrate_env_names_may_not_appear_in_any_other_env_block`, `an_explicitly_null_migrate_env_is_rejected`, `dry_run_with_migrate_env_shows_no_value`, `migrate_env_reaches_only_the_migrate_container_over_real_ssh` |
| Two operators deploy at once | T | An atomic `mkdir` lock on every host whose record (holder, time, message) is claimed with `ln` from a noclobber temporary file, so exactly one claimant wins, also when two take over a stale record-less lock; held locks exit 3; a release removes the lock only while it still carries its own record, and never someone else's without `--force` | `held_lock_is_reported_and_partial_locks_released`, `lock_release_refuses_someone_elses_lock_without_force`, `deploys_over_real_ssh`, `lock_tests::*` (real scripts under `sh`, including `stale_recovery_interleaved_race_has_one_winner`) |
| A client spoofs its address or scheme through the proxy | S | Inbound `Forwarded`, `X-Forwarded-*` and `X-Real-IP` are dropped; `X-Forwarded-For` is the connecting address only; the application trusts it only from the forge network | `forwarded_headers_are_set_and_spoofed_ones_dropped`, `proxied_role_trusts_the_forge_network_unless_configured` |
| Slow or oversized requests exhaust the application | D | 413 over `max_body_bytes`; bodies up to 1 MiB read completely within 30 s before contacting the application; larger bodies need data every 30 s; `response_timeout` for response headers; bounded connections, tunnels and control requests; after `-idle-timeout` an idle keep-alive connection, a response write the client does not read and a silent tunnel are closed, releasing the target lease. **Residual:** no total time limit per connection (as with nginx `send_timeout`): a client that reads or sends a byte per idle interval keeps its connection, bounded only by the 10,000 connections per listener, with no per-address limit | `request_body_limits`, `slow_chunked_body_is_cut_off_at_the_deadline`, `chunked_bodies_are_buffered_or_streamed`, `idle_keep_alive_connections_are_closed`, `a_keep_alive_connection_waits_the_idle_timeout_not_the_header_timeout`, `the_next_request_head_has_the_header_timeout_from_its_first_byte`, `the_first_request_has_the_header_timeout_from_the_connection_start`, `a_client_that_stops_reading_is_disconnected_and_releases_the_target`, `idle_tunnels_are_closed_and_active_ones_kept` |
| A switch drops or misroutes requests | D | Targets are swapped atomically; in-flight leases are drained before the deploy returns; a retired target is never used | `switch_under_concurrent_load_drops_no_requests`, `deploy_drains_in_flight_requests_before_returning`, `drain_timeout_reports_remaining_requests`, `request_racing_a_retired_target_gets_503`, `upgrade_through_proxy` |
| An unhealthy container receives traffic | D | `healthy_count` consecutive passes within `deploy_timeout` before the switch; otherwise the previous target keeps serving and the proxy exits 4 | `unhealthy_target_is_rejected_and_previous_keeps_serving`, `health_gate_requires_consecutive_passes` |
| Anyone on the network reconfigures the proxy | E | No network control surface: a Unix socket in a `0700` directory (an existing one restricted at start, a link refused), socket `0600`; strict JSON with unknown fields refused | `control_socket_and_state_persistence`, `an_existing_control_directory_is_restricted_and_a_link_refused`, `client_commands` |
| A tampered or foreign state file reroutes traffic after a restart | T | The state file is written atomically `0600`; an unreadable, unknown-version or invalid state stops the proxy instead of routing from it | `control_socket_and_state_persistence` |
| A certificate is issued or served for a host the operator did not configure, or a wrong certificate is presented | S, T | Certificates only for hosts of TLS services; handshakes for other hosts or before issuance fail; HTTP-01 answers only for TLS hosts, withdrawn when an issuance ends or is cancelled; the key and chain must match and the leaf's subject alternative names must cover the host; ACME over verified HTTPS (plain HTTP only on loopback); account and certificate files `0600`, written atomically and synced, in a `0700` cache (an existing one restricted at start through one handle; a link or another user's directory refused) | `certificates_are_served_only_for_tls_hosts`, `acme_obtains_caches_and_reuses_a_certificate` (every JWS signature and nonce verified, CSR verified by `openssl`), `a_cancelled_authorization_withdraws_its_challenge`, `an_existing_cache_directory_is_restricted_and_a_link_refused`, `acme_client_interoperates_with_pebble`, `run_serves_and_shuts_down` |
| Tokens in access logs | I | Access logs record the path without its query string | `access_log_omits_the_query` |

Residual risks: the deploy user's Docker access is root-equivalent on each
host; `docker inspect` on a host shows container environments, secrets
included; streamed output redaction catches only exact values; the
schema owner's `FORGE_MIGRATION_DATABASE_URL` reaches the one-off migrate
container alone (`migrate.env`, ADR 0042), but its `0600` env file rests on
the migrate host until the step ends, a deploy killed outright (`kill -9`,
power loss) leaves it there until the next deploy that uses `migrate.env` or
until it is deleted by hand (also after `migrate.env` is removed from the
configuration), and `docker inspect` shows the variable while the container
runs; the ACME client is tested against an in-test CA and, where `FORGE_TEST_PEBBLE_DIRECTORY` is set, against Pebble (not yet
run in CI), not Let's Encrypt itself; repeated issuance failures back off up to an hour but
can still exhaust Let's Encrypt's rate limits for a misconfigured host;
interactive sessions (`forge app shell`, `exec --interactive`) are not
filtered; `forge-proxy` must be referenced by digest to be sure every host
runs the reviewed build (`setup` warns otherwise).

## HTTP core review

Scope: `forge-http`'s server (request bodies, panics, response headers,
`HEAD`), `FORGE_HTTP_TRANSPORT` and `FORGE_MAX_IN_FLIGHT_BODY_BYTES` in
`forge-config`.

| Threat | STRIDE | Control | Verification |
| --- | --- | --- | --- |
| Memory exhaustion by many slow uploads | D | Bodies are buffered before dispatch, so every body is charged to one byte-weighted budget shared by all connections (`FORGE_MAX_IN_FLIGHT_BODY_BYTES`, 256 MiB by default instead of `FORGE_MAX_BODY_BYTES` × `FORGE_MAX_CONNECTIONS`, about 10 GiB): bytes are charged as they arrive, for `Content-Length` and chunked bodies alike; a declared length is only checked against `FORGE_MAX_BODY_BYTES` and never reserved, so headers announcing a large body and no bytes after them hold no budget. A body that does not fit is refused at once with `503` and `Retry-After: 5`; the charge is released when the handler returns or on any other way out (rejection, timeout, panic, disconnect). A body that stalls (before or after its first byte) is cut by the request timeout (`FORGE_REQUEST_TIMEOUT_SECS`, which covers reading the body and the handler, like Forge-Go's whole-request `ReadTimeout`); while it stalls it holds only the bytes it sent and its connection slot. **Residual:** to exhaust the budget a client must really send that many bytes and keep them in flight; such a client can hold them for the request timeout, so other uploads get `503` meanwhile (requests without a body are unaffected); there is no per-address share | `content_length_bodies_are_charged_as_they_arrive_and_refused_with_503`, `a_declared_but_unsent_body_holds_no_budget`, `trickling_chunked_uploads_are_charged_as_they_arrive_and_released_on_disconnect`, `the_body_budget_is_returned_after_a_panic_a_timeout_and_a_rejection`, `a_charge_larger_than_u32_is_taken_in_steps`, `the_in_flight_body_budget_defaults_to_256_mib_and_covers_one_body` |
| A handler panic leaks internals or cuts the connection | I, D | The panic is caught around the layers and the handler, logged at `error` with the request id, source location and payload type (`request handler panicked`), its message, which can quote request data, only at `debug`; a panic hook installed once per process (wrapping the previous one, as `forge-jobs` does for job attempts) keeps request panic messages off stderr and passes every other panic on; a panic the request code catches itself (a `catch_unwind` in a handler or a metric observer) is logged at `warn` (`request code caught a panic`) with the same fields and no message. It is answered with a generic `500` (`internal server error`) carrying `x-request-id`; the connection stays usable and telemetry records the `500`. A panic inside a streamed body after the headers were sent still ends the connection | `a_handler_panic_is_a_500_with_the_request_id_and_is_logged`, `the_panic_message_is_logged_only_at_debug`, `a_panic_caught_inside_the_request_is_logged_at_warn_without_its_message`, `panic_payload_types_are_named_without_their_text` |
| Protocol downgrade (SSL stripping) after the first visit | T, I | With `FORGE_HTTP_TRANSPORT=trusted-proxy` (the production default; production refuses `plain`) every response carries `Strict-Transport-Security: max-age=31536000; includeSubDomains`, including `404`s and recovered panics; a handler's own value (for `preload`) is kept. **Residual:** `preload` is opt-in, so the very first visit can still be downgraded | `the_server_sends_hsts_only_behind_tls_termination`, `hsts_is_added_when_clients_use_https_and_a_handler_value_is_kept`, `the_transport_follows_the_environment_and_production_refuses_plain` |
| `HEAD` answered differently from `GET`, or with a body | T | `HEAD` runs the `GET` route (layers included, so guards apply equally) unless a `HEAD` route exists; the server drops the body without polling a stream and keeps the `GET` length | `head_is_served_by_the_get_route_with_the_head_method`, `head_answers_with_the_get_headers_and_no_body`, `a_keep_alive_connection_continues_cleanly_after_head` |

## Serving role and row-level security review

Scope: `PgDatabase::require_row_security` (forge-db) and its wiring in the
`forge::App` lifecycle, `forge::db::testing::RlsTestDatabase`, the PostgreSQL
tests `forge generate scaffold` writes
([ADR 0044](adr/0044-refused-row-level-security-bypass-and-runtime-role-tests.md),
port of Forge-Go ADR 0046).

| Threat | STRIDE | Control | Verification |
| --- | --- | --- | --- |
| Serving role bypasses row-level security, or can switch it off | E, I, T | Generated PostgreSQL applications serve as a runtime role (`NOSUPERUSER NOBYPASSRLS NOCREATEROLE`, owns nothing, data privileges only, no temporary tables) and migrate as a separate owner role (`FORGE_MIGRATION_DATABASE_URL`; the development roles come from `docker/postgres/init.sql`). Outside development and test, `serve`, `jobs work` and the `jobs` operator commands refuse to start (exit 1), before telemetry starts and before any composition hook receives the pool, when the login role (`session_user`) or the role in effect (`current_user`) is a superuser, has `BYPASSRLS`, `CREATEROLE` or `REPLICATION` (logical decoding reads every tenant's changes), is a member, directly or through other roles and whatever the grant's `INHERIT`/`SET` options, of a superuser, `BYPASSRLS` or `REPLICATION` role, is a member of `pg_execute_server_program`, `pg_write_server_files` or `pg_read_server_files`, or owns, or is a member of the owner of, a table with row-level security (an owner can `ALTER TABLE ... NO FORCE ROW LEVEL SECURITY`), or when the two differ (a role setting such as `options=-c role=` or `ALTER ROLE ... SET role`, which `SET ROLE NONE` undoes) (`DbError::RowSecurityBypass`). There is no override variable | `require_row_security_refuses_bypassing_roles_outside_development_and_test`, `serve_in_production_refuses_a_superuser_and_serves_the_runtime_role` (no composition hook runs before the refusal), `job_commands_in_production_refuse_a_superuser`, the scaffold end-to-end test (`jobs status` of a generated binary in production) |
| The check itself is fooled | T, E | The role query runs in a `READ ONLY` transaction after `SET LOCAL search_path = pg_catalog`, so operators resolve to PostgreSQL's own whatever `search_path` the URL or role sets, and names `pg_catalog.pg_roles` and `pg_catalog.pg_has_role`. It fails closed: no row for the login role, rows that disagree on the session's roles, or any error (an unreachable server, a failed `SET`) refuse. The message names the role (quoted, so a role name cannot forge log lines) and the fix, never the URL | `require_row_security_ignores_a_shadowing_pg_roles_view`, `require_row_security_ignores_a_shadowing_operator` (both fail when the pin or the qualification is removed), `no_row_fails_closed`, `inconsistent_rows_fail_closed`, `an_unreachable_server_fails_closed`, `serve_in_production_fails_closed_when_the_role_cannot_be_checked`, `role_names_are_quoted_so_they_cannot_forge_log_lines` |
| Tests prove isolation as a role production never uses | T | `RlsTestDatabase` applies the migrations as a fresh owner role and runs the generated repository tests as a fresh runtime role with `init.sql`'s default privileges; every PostgreSQL scaffold gets a test that this role cannot run `CREATE TABLE`, `CREATE TEMPORARY TABLE`, `ALTER TABLE`, `TRUNCATE`, `DROP TABLE` and, for tenant resources, `DISABLE`/`NO FORCE ROW LEVEL SECURITY` or `DROP POLICY`, and a tenant scaffold's RLS test runs as it. Roles and database are dropped afterwards, also on panic; each login is verified, so a `user=` query parameter cannot leave a test running as the administrator | `the_runtime_role_is_confined_by_row_level_security`, the generated `postgres_runtime_role_cannot_change_the_schema` and `postgres_row_level_security_isolates_tenants` (run by `postgresql_application_with_scaffolds_passes_its_gates_and_migrations_revert`) |
| The database container cannot read `init.sql`, so the roles are never created | D | `forge new` makes `docker/postgres/init.sql` `0644` under `0755` directories whatever the umask, as the observability configuration | `bind_mounted_files_are_world_readable_whatever_the_umask` (umask 077 in a child shell) |

**Residual:** row-level security stops a query that forgets its tenant
predicate, not SQL injected into the serving process, which can call
`set_config('forge.tenant_id', ...)` itself; ownership is judged only on
tables with row-level security enabled when the process starts; it runs at
startup only, and not in development or test;
`routes` and `openapi` do not connect and are not checked; migrations that
grant to `app_runtime` by name (the generated identity tables) do not reach
the per-test runtime role, which keeps the broader default privileges on
those tables, and the generated sign-in tests still run as the test server's
user.

## Review cadence

Update this model when adding a trust boundary, privileged capability, protocol,
provider, sensitive data class, or deployment target. Phase 3, Phase 7, Phase 8,
and production certification require explicit threat-model review with linked
security tests. Security fixes receive a regression test unless disclosure or
environment constraints make that unsafe, in which case the exception is recorded.

Related decisions: [tenancy and RLS](adr/0004-tenancy-and-rls.md),
[AI execution boundary](adr/0005-ai-provider-and-execution-boundary.md), and
[dependency policy](dependency-policy.md). Container defaults are recorded in
[ADR 0008](adr/0008-docker-by-default.md).
