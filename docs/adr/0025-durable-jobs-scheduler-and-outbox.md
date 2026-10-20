# ADR 0025: Durable PostgreSQL jobs, cron schedules, transactional outbox and idempotent consumers

- Status: Accepted
- Date: 2026-10-19
- Refines: [ADR 0006](0006-tokio-and-supervised-concurrency.md), [ADR 0011](0011-postgresql-runtime-migration-roles-and-rls-context.md), [ADR 0021](0021-selectable-database-backends.md)

## Context

Phase 4 delivers background work: jobs, recurring schedules, integration
events through an outbox, and idempotent consumers. Rails 8 ships Solid Queue
(a database-backed queue with `FOR UPDATE SKIP LOCKED`, recurring tasks and
mission control). Forge needs the same productivity with stronger guarantees
and its own invariants:

- async work is bounded, cancellable and owned by an explicit lifecycle
  (ADR 0006): no detached tasks, unbounded queues or hidden background work;
- the serving process holds only the runtime role, which cannot bypass RLS
  (ADR 0011), yet a worker must claim every tenant's jobs;
- tenant-sensitive operations require an authorized `TenantContext`;
- other backends refuse PostgreSQL-only capabilities explicitly (ADR 0021);
- SQLx stays in adapters; IDs are UUIDv7 generated in Rust.

Known weaknesses of database queues we want to avoid: a slow worker whose
lease expired completing a job another worker already re-ran; recurring
tasks firing twice when several schedulers run; poison jobs blocking a
queue; unbounded retention; payload data leaking into logs.

## Decision

1. **Crates follow the architecture crate map.** `forge-jobs` holds the job
   contract, the PostgreSQL adapter, the worker runtime and the cron
   scheduler (the map names no separate scheduler crate, and schedules only
   enqueue jobs). `forge-events` holds the integration-event contract, the
   outbox, the dispatcher and the inbox; it depends on `forge-jobs` only for
   the shared `Backoff` policy and name grammar. The `forge` facade exposes
   them behind the additive features `jobs` and `events` (which implies
   `jobs` and `postgres`).

2. **Contract.** A job is a `Serialize + DeserializeOwned` type with
   associated constants `NAME`, `QUEUE`, `MAX_ATTEMPTS` (1–1000), `TIMEOUT`,
   `PRIORITY` (lower first) and `BACKOFF`; a `JobHandler` performs it.
   Enqueue takes the caller's `DbTransaction` (`postgres::enqueue`,
   `enqueue_for_tenant`), so jobs commit or roll back with the business
   writes. Options: run time or delay (database clock), priority, and an
   idempotency key unique per tenant scope. Payloads are JSON, at most
   256 KiB, and never logged. Times use `forge_core::Timestamp`.

3. **Schema and roles.** Each crate embeds one reversible migration with
   its own bookkeeping table (`_forge_jobs_migrations`,
   `_forge_events_migrations`), so framework schemas never collide with
   application migration versions. `migrate` applies them before the
   application's migrations; `rollback` reverts only application
   migrations. Grants go to the role named by the database setting
   `forge.runtime_role` (default `app_runtime`, the generated role); an
   explicitly named but missing role fails the migration.

4. **Row-level security plus narrow definer functions.** `forge_jobs` and
   `forge_outbox_events` carry `tenant_id` (and the enqueuing
   `principal_id` for jobs) with an RLS policy equal to ADR 0011's: a
   transaction sees and writes exactly the rows of its
   `forge.tenant_id` scope (global rows when unset); a malformed setting
   fails the statement. The runtime role has only `SELECT` and
   column-restricted `INSERT` on these tables (state, attempts and leases
   always start from their defaults; payloads are capped at 256 KiB of
   stored JSON text by a `CHECK`). Cross-tenant operations (claim,
   heartbeat, complete, fail, release, retry, discard, dead listing, stats,
   prune, schedule ticks) are `SECURITY DEFINER` functions owned by the
   schema owner, with `EXECUTE` revoked from `PUBLIC` and `search_path`
   pinned to the schema with `pg_temp` last. They are the explicit
   capability ADR 0011 requires for cross-tenant access, and they are
   cross-tenant reads: claims return payloads, `stats`/`dead` metadata.
   They are granted to the worker role (`forge.worker_role`, default the
   runtime role); with a separate worker role the request-serving runtime
   role keeps only the lease-fenced `forge_jobs_complete`. All except
   `forge_jobs_complete` refuse to run in a transaction carrying
   `forge.tenant_id`.

5. **Claiming, leases and fencing.** Workers claim due jobs of one queue and
   of the names they can run with `FOR UPDATE SKIP LOCKED`, in
   `(priority, run_at, id)` order. Each claim sets `state = running`,
   increments `attempts` and stores a fresh lease token (UUIDv7 from Rust)
   and `locked_until`. Every later transition must present the token, so a
   worker whose lease expired and was reclaimed cannot complete, fail,
   release or renew the job. Workers renew leases every third of the lease
   and cancel attempts whose lease was lost. Expired leases are reaped at
   claim time: back to the queue (the lost attempt counts), or dead when the
   budget is spent, which bounds crash-looping poison jobs.

6. **Delivery semantics.** Jobs and events are at-least-once. A handler that
   only touches the database can make its effects exactly-once with
   `postgres::complete_in`, which marks the job completed inside the
   handler's own transaction (fenced by the lease). Consumers make events
   idempotent with the inbox: `record_delivery` inserts
   `(consumer, event_id)` in the transaction that applies the effects and
   reports duplicates.

7. **Failures.** A handler returns `JobError::retry` (optionally with
   `retry_after`), `JobError::permanent`, or any `std::error::Error` via
   `?`. Retries wait the capped exponential backoff with equal jitter
   (`[ceiling/2, ceiling]`). Exhausted or permanent failures move the job to
   `dead`, where operators list, retry (fresh budget) or discard it. Timeouts
   cancel the attempt and count as retryable failures. A panicking handler
   is caught at its task boundary and recorded as a failed attempt
   (`the job panicked`, without the panic message); the worker continues.
   Stored error messages are truncated to 1024 bytes with control
   characters replaced.

8. **Lifecycle.** `Jobs::run(store, shutdown)` owns every task it starts in
   a `JoinSet`: one worker loop per queue (bounded by its concurrency, so
   claiming is the backpressure), the scheduler and the pruner. On shutdown
   a worker stops claiming, waits for in-flight attempts up to
   `drain_timeout`, cancels the rest cooperatively through
   `JobContext::cancellation`, aborts what remains after `cancel_grace`
   and releases their leases without consuming an attempt. A failing loop
   stops the others. In the facade the HTTP server and the job runtime share
   one cancellation: a signal, or either part failing, stops both.

9. **Scheduler.** Recurring schedules use a hand-written, property-tested,
   five-field UTC cron grammar (Vixie semantics, no time zones). Each tick
   fires exactly once across replicas: a replica conditionally upserts the
   schedule's `last_tick_at` (`WHERE last_tick_at < tick`) and, only when it
   advanced the row, enqueues the job in the same transaction with the
   idempotency key `forge.schedule:<name>:<tick>`. Missed ticks are not
   replayed; a tick older than the schedule's `max_lateness` (default five
   minutes) is recorded and skipped. Correctness does not depend on clock
   agreement between replicas, only on monotonic `last_tick_at`.

10. **Outbox.** Integration events are versioned (`TYPE`, `VERSION`) and
    appended in the caller's transaction. The dispatcher claims committed
    events in id order with a fenced lease sized for a batch, publishes
    each through an `EventPublisher` (the in-process `Subscribers` registry
    or a broker adapter) in its own task with a timeout, and records
    success, retry with backoff, or dead. Ordering is best effort.

11. **Retention.** A bounded pruning loop deletes completed jobs after
    `completed_after` (default one day; idempotency keys become reusable)
    and optionally dead jobs; every statement deletes at most `batch` rows
    and a run executes at most `max_batches` statements. The outbox store
    prunes published events and old inbox rows on request.

12. **Backends.** Durable jobs, schedules and the outbox require
    PostgreSQL. Declaring them with MySQL/MariaDB or SQLite (or without a
    database) fails at startup with an explicit error; nothing is emulated.
    ADR 0021's matrix lists possible future adapters; they need their own
    review because they lack RLS and definer functions.

13. **Operations.** The facade adds `<app> jobs [work]` (workers, schedules
    and dispatcher without HTTP) and the operator commands `jobs status`,
    `jobs dead [LIMIT]`, `jobs retry ID`, `jobs discard ID`,
    `jobs dead-events [LIMIT]` and `jobs retry-event ID`, all over the
    runtime URL. `App::jobs(WorkerMode::InServe)` also runs workers in
    `serve`; `WorkerMode::Dedicated` keeps web and worker processes apart.

14. **Polling.** Idle workers poll (default 1 s, jittered). `LISTEN/NOTIFY`
    wake-ups are a later optimization; correctness never depends on them.

## Consequences

- Business writes, jobs and events commit atomically; a crashed or
  partitioned worker cannot double-complete a job; schedules fire once per
  tick across replicas; poison jobs end in an inspectable dead state.
- The runtime role cannot read or modify other tenants' jobs through SQL,
  but the definer functions are a deliberate cross-tenant capability of the
  worker role: whoever holds it can read every payload and claim, retry or
  discard any job. In the default single-role setup that is the
  request-serving process; deployments that need stronger isolation run a
  separate worker role and `WorkerMode::Dedicated`.
- Tenant jobs run with the enqueuing tenant and principal reinstalled as RLS
  settings, but authorization is not re-evaluated at execution time: a
  membership revoked after enqueue does not stop an already enqueued job.
  Handlers that must honour revocation re-check membership; a framework
  hook for this is follow-up work.
- The claim and transition SQL lives in migrations; changing it requires a
  new framework migration.
- Two additional framework bookkeeping tables exist; `rollback` does not
  revert framework schemas (operators revert them explicitly with the
  embedded migrators).
- Generated applications do not use these features yet: wiring `forge new`
  (features, roles, compose examples, `jobs work` service) is a separate
  generator change.
