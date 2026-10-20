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
| Resource exhaustion | request/body/header limits, bounded queues/concurrency, deadlines, backpressure, per-principal quotas, streaming byte limits, circuit breakers | load and saturation tests |
| Replay and duplicate mutation | scoped idempotency keys with request digest and expiry, signed webhook timestamp/nonce, persistent deduplication for jobs/events, single-use execution-plan ID | replay integration tests |
| Webhook forgery | per-source signature verification over raw bytes, constant-time comparison, timestamp window, secret rotation overlap, replay store | signature vector and replay tests |
| Sensitive-data leakage | classified fields, response allowlists, error redaction, telemetry filtering, secrets as opaque references, no prompt/body capture by default | snapshot/redaction tests and log scans |
| Audit tampering | append-only restricted writer, actor/tenant/action/outcome/policy/version/request linkage, integrity chaining or external immutable sink, retention policy | authorization and integrity-verification tests |
| Dependency/build compromise | pinned lockfile, advisory/license/source policy, minimal features, reviewed build scripts, SBOM, isolated CI, provenance and signed release artifacts | `cargo deny`, audit, reproducibility and signature gates |
| Container escape or image secret leakage | multi-stage build, immutable base digests, non-root numeric user, minimal final stage, read-only filesystem compatibility, dropped capabilities, no layer/build-arg secrets, image scanning | image inspection and container E2E tests |
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
throttle, password-reset delivery, MFA, audit integrity chaining/retention and
privileged reader workflows, and administrative cross-tenant role separation
still require implementation and abuse tests.

## Server-rendered views review

Review date: 2026-10-19 ([ADR 0022](adr/0022-server-rendered-views.md),
[ADR 0023](adr/0023-form-csrf-flash-and-method-override.md)).

Full-stack generated applications register three request layers before
routing: the strict Content-Security-Policy, `CsrfProtection` and
`FlashLayer`.

| Threat | Control | Verification |
| --- | --- | --- |
| Cross-site form or `fetch` submission | Every unsafe request needs exactly one 256-bit token (header or form field) equal to the `__Host-forge_csrf` cookie (`Secure`, `HttpOnly`, `SameSite=Lax`, no `Domain`); constant-time comparison; repeated/malformed values refused; `403` with a generic body | missing, wrong, repeated, malformed, query-string and header/field-mismatch tests in `forge-views`; generated `tests/http.rs`; e2e |
| Cookie tossing from a sibling subdomain or plain-HTTP attacker | `__Host-` prefix; `Sec-Fetch-Site: same-site` refused | cookie attribute tests; cross-site tests |
| Login CSRF / anonymous forms | Token issued before any session, independent of the database | first-visit issuance test |
| Tunnelled `DELETE` without CSRF | `_method` honored only on `POST`, only `PATCH`/`PUT`/`DELETE`, only after verification (`CsrfVerified` witness), repeated field refused, `GET` never overridden | method-override negative tests |
| Token leakage through caches | Token-bearing pages are `Cache-Control: no-store`; asset and probe responses never receive `Set-Cookie` | cache and lazy-issuance tests |
| Flash injection | Messages rendered escaped; cookie `__Host-`, bounded, shape-checked, removed when tampered | flash tests |
| Form parser abuse | Exactly one UTF-8 form `Content-Type`, body bounded by `FORGE_MAX_BODY_BYTES`, at most 1000 pairs, strict grammar, duplicate scalar fields refused, caller-safe errors | `forge-http` form tests |
| Asset path traversal or MIME confusion | Exact digested routes only, closed extension allowlist, `nosniff`, SVG sandbox CSP | existing asset tests |

Residual risks: tokens are not masked per response, so HTML compression by a
proxy combined with reflected input could enable BREACH-style recovery; the
flash cookie is unsigned (display text only); the `Origin`/`Host` fallback can
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
| Online password guessing and credential stuffing | Attempts counted before any credential check against the canonical login (whatever organization or letter case is typed), the source (`Request::client_ip`: the socket peer unless it is a proxy listed in `FORGE_HTTP_TRUSTED_PROXIES`, ADR 0032; IPv6 per /64) and the pair: 5 per login and source, 50 per login, 300 per source in 15 minutes (`MemoryLoginThrottle`); `429` with `Retry-After`; success clears the pair and login counters, not the source; Argon2id | `forge-auth` throttle tests; generated `attempts_are_throttled_even_with_the_right_password`, `letter_case_and_unicode_variants_do_not_get_fresh_guesses`, `clients_behind_one_trusted_proxy_have_their_own_sign_in_counters` |
| Throttle as a denial of service | One source cannot lock a login out for others (the tight limit is per login and source); a full table evicts the oldest idle counters instead of refusing everyone, and keeps blocking ones | `one_source_cannot_lock_a_login_out_for_other_sources`, `a_full_table_evicts_idle_counters_and_keeps_blocking_ones` |
| Look-alike (Unicode) logins | `CanonicalLogin` refuses non-ASCII logins at the boundary (storage, lookup, throttle keys); reset links are delivered to the login as stored, never as typed | `forge-auth` login tests; `reset_links_go_to_the_stored_login_and_look_alikes_get_nothing` |
| Timing oracle on reset requests and sign-in | Reset requests are queued to a bounded lifecycle-owned worker and answered after at least 100 ms; sign-in runs the tenant and membership lookups with random identifiers for malformed or unknown organizations; unknown reset tokens are refused before Argon2id | `reset_requests_take_the_same_minimum_time`, `sign_in_looks_up_a_membership_whatever_the_organization`, `unknown_reset_tokens_are_refused_before_hashing` |
| Account and tenant enumeration | One `422` message for a wrong organization, email or password, a disabled account or an inactive membership; unknown logins still cost one Argon2id hash; reset requests answer the same `303` and flash whether or not the account exists or the link was delivered | `every_wrong_field_gets_the_same_answer`, `reset_requests_answer_the_same_whether_or_not_the_account_exists` |
| Login CSRF and forged sign-out | Handlers refuse (`403`) unless `CsrfProtection` verified the request (`csrf_verified`); the sign-in form's token is issued before any session; sign-out also checks the session-bound CSRF token (ADR 0014) | `sign_in_without_the_csrf_token_is_forbidden`, `signing_out_needs_the_csrf_token_and_revokes_the_session` |
| Session fixation and token planting | Sign-in issues a new server-side session and a new CSRF token in fresh cookies and revokes the session the browser held; sign-out and reset clear the session cookie and rotate the CSRF token | `signing_in_issues_a_fresh_session_and_csrf_token_and_returns_to_the_page`, `signing_in_again_revokes_the_previous_session` |
| Open redirect through `return_to` | `return_path` accepts only local paths (single leading `/`, visible ASCII, no backslash, at most 2048 bytes) when rendering, posting and redirecting; `see_other` re-checks | `forge-views` return-path tests; `return_paths_off_the_site_are_ignored` |
| Reset token theft or replay | 256-bit CSPRNG token, SHA-256 digest persisted, single use (row lock, every outstanding token of the account used up), 15-minute expiry, bound to principal and tenant membership, never logged or in `Debug`; reset pages `no-store` and `Referrer-Policy: no-referrer`; every session revoked on reset | `forge-auth` token tests; `a_reset_link_works_once_changes_the_password_and_revokes_sessions`; PostgreSQL `reset_tokens_work_once_expire_and_revoke_every_session` |
| Takeover through `setup` | Runs only with the migration credential, one transaction under an advisory lock, refuses once any tenant exists; password never an argument (`--password-stdin`, `FORGE_SETUP_PASSWORD` or an echo-off prompt), at least 12 characters, never printed; audited | `setup_creates_one_owner_who_can_sign_in_and_refuses_afterwards`, `concurrent_setups_create_exactly_one_tenant`, `setup_refuses_password_arguments_and_short_passwords_before_touching_a_database` |
| Signed-out browser leaks a protected page | Access policies answer `401`; `SignInRedirect` turns it into a `303` to sign in for HTML clients only; JSON keeps `401` | `forge-views` sign-in redirect tests; `signed_out_browsers_are_sent_to_sign_in_and_api_clients_get_401`; slow scaffold e2e |

Residual risks: throttle counters live in one process (a restart or several
replicas multiply the budget; a shared store is a follow-up); behind a
reverse proxy clients share one source budget until `FORGE_HTTP_TRUSTED_PROXIES`
lists the proxy, and anything inside a listed network can choose its source
(see the trusted-proxy review below); an attacker who controls 50 sources (or /64s)
can still keep one known login throttled for 15 minutes at a time, and one
with very many sources can fill the table with blocking counters, after
which new attempts are refused until counters expire; a full reset queue
drops requests (logged), so a flood can delay legitimate resets; the reset
worker's database load still depends on whether accounts exist, observable
only through server load; sign-in still does slightly more work (session
issuance) when it succeeds; until a mailer adapter replaces
`UndeliverablePasswordResets`, reset links are not delivered and the failure is
only logged; MFA is not implemented; a session lasts 24 hours without
idle expiry.

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
| Upload abuse: oversized bodies, many files or fields | D | `receive` caps the body, bounds files, text-field count (every repetition counts) and bytes, and header sizes; the server body limit bounds buffering and `FORGE_STORAGE_MAX_UPLOAD_BYTES` cannot exceed it. **Residual:** bodies are buffered (up to `FORGE_MAX_BODY_BYTES`) until `forge-http` streams; disk space is not metered per user | oversized, body-cap, repeated-field (1500 values) and header-budget tests |
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
| Missing or shared signing key | S, E | `FORGE_STREAMS_KEY` is validated (base64 of at least 32 bytes), redacted and never echoed; production refuses to start without it; development and test use a random per-process key | config and `from_config` tests, generated `production_refuses_to_start_without_a_signing_key` |

Residual risks beyond the rows above: delivery is at most once without
replay; the in-process broker reaches one process only; each open stream
holds a connection slot; stream limits are per process; a vanished client
keeps its slot until a write fails (at most 30 minutes); rotating
`FORGE_STREAMS_KEY` stops live updates on open pages until reloaded (no
previous-key setting yet, a parity follow-up); `sqlx::PgListener` buffers
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

Residual risks: with `FORGE_TELEMETRY_LOGS_EXPORTER=otlp` every log line goes
to the collector, which must be protected like the log store; a raw secret
logged by application code leaks there as it does to standard output.
Attributes added by application code are the application's responsibility.
The metrics listener has no authentication: confirming it public relies on
the network keeping it private. Trusting `traceparent` is safe only behind a
gateway that sets or sanitizes it. Dropped spans and log records under load
are counted, not retried.

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
