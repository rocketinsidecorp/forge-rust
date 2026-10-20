# Jobs, schedules, outbox and consumers

Status: Phase 4 library implemented in `forge-jobs`, `forge-events` and the
`forge` facade (features `jobs` and `events`), with PostgreSQL integration
tests. `forge new` wires the job queue into every PostgreSQL application (see
[Generated applications](#generated-applications)); the outbox and other
applications opt in by hand as shown below. Design and trade-offs:
[ADR 0025](adr/0025-durable-jobs-scheduler-and-outbox.md).

| Capability | Where |
| --- | --- |
| Typed jobs, handlers, retries, timeouts, priorities, idempotency keys | `forge::jobs` |
| Enqueue in the caller's transaction | `forge::jobs::postgres::{enqueue, enqueue_for_tenant}` |
| Workers, lease renewal, graceful drain, pruning | `forge::jobs::Jobs`, run by `serve` or `<app> jobs work` |
| Cron schedules, exactly once per tick across replicas | `forge::jobs::{Cron, Schedule}` |
| Versioned integration events in a transactional outbox | `forge::events::postgres::{append, append_for_tenant}` |
| Dispatcher with leases, retries and dead letters | `forge::events::Dispatcher` (run with the workers) |
| Idempotent consumers (inbox) | `forge::events::postgres::record_delivery` |
| Operator commands | `<app> jobs status|dead|retry|discard|dead-events|retry-event` |

Only PostgreSQL is supported. Declaring jobs with MySQL/MariaDB or SQLite
fails at startup with an explicit error; nothing is emulated.

## Enable it

```toml
# Cargo.toml of the application
forge = { version = "...", features = ["jobs", "events"] }   # events is optional
```

```rust
use forge::{App, ComposeError, WorkerMode};

fn main() -> std::process::ExitCode {
    App::new("shop")
        .postgres(sqlx::migrate!())
        .jobs(WorkerMode::InServe) // or WorkerMode::Dedicated
        .events()                  // optional: the outbox
        .compose(bootstrap::compose)
        .run()
}
```

`App::jobs` makes `migrate` apply the framework job schema (and the outbox
schema with `App::events`) before the application's migrations, runs the
workers inside `serve` (`InServe`) and in `jobs work`, and enables the
operator commands.

## Generated applications

`forge new` (PostgreSQL, full stack or `--api`) enables the `jobs` feature
and generates the lifecycle wiring, as Forge-Go's `forge new` generates
`bootstrap.JobsConfigure`:

```rust
// src/bootstrap/mod.rs
forge::App::new(env!("CARGO_PKG_NAME"))
    .postgres(sqlx::migrate!())
    .jobs(forge::WorkerMode::InServe)
    // ...
    .compose(jobs_configure)
    .run()

fn jobs_configure(composition: &mut forge::Composition<'_>) -> Result<(), forge::ComposeError> {
    // At most 4 attempts of each queue run at once.
    composition
        .jobs()?
        .settings(forge::jobs::WorkerSettings::default().default_concurrency(4));
    // Register application jobs here (`forge generate job NAME --queue`).
    Ok(())
}
```

No job is generated; until one is registered the runtime only prunes
finished jobs. `forge generate job NAME --queue` prints the `register` line
for the end of `jobs_configure`:

```sh
forge migrate                                         # app migrations + the job schema
forge generate job send_welcome user_id:uuid --queue  # then add the printed lines
forge test
forge dev                                             # serve: HTTP + workers + schedules
cargo run -- jobs status
```

## Define a job and its handler

`forge generate job NAME [field:type...] --queue` writes the job, its
handler, `register`, `enqueue` and tests, and prints the wiring below
([generators](generators.md#job)). By hand:

```rust
use std::time::Duration;

use forge::jobs::{Backoff, Job, JobContext, JobError, JobHandler};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub struct SendWelcomeEmail {
    pub user_id: uuid::Uuid, // a reference, never a secret
}

impl Job for SendWelcomeEmail {
    const NAME: &'static str = "accounts.send_welcome_email"; // stable: stored in the database
    const QUEUE: &'static str = "mailers";                     // default "default"
    const MAX_ATTEMPTS: u16 = 8;                               // default 10, then `dead`
    const TIMEOUT: Duration = Duration::from_secs(30);         // default 5 minutes
    const PRIORITY: i16 = 0;                                   // lower runs first
    const BACKOFF: Backoff =
        Backoff::exponential(Duration::from_secs(10), Duration::from_secs(600));
}

pub struct WelcomeEmailHandler { /* use cases, adapters */ }

#[async_trait::async_trait]
impl JobHandler for WelcomeEmailHandler {
    type Job = SendWelcomeEmail;

    async fn perform(&self, context: &JobContext, job: SendWelcomeEmail) -> Result<(), JobError> {
        // `?` turns any std::error::Error into a retryable failure.
        // JobError::permanent("...") skips the remaining attempts.
        // JobError::retry("...").retry_after(d) overrides the backoff.
        let _ = (context.attempt(), job.user_id);
        Ok(())
    }
}
```

Handlers are adapters, like HTTP handlers: they call application use cases.
A panicking handler is recorded as a failed attempt (`the job panicked`);
its panic message is kept out of the database, the logs and stderr
(`forge::jobs::panics`, a process-wide hook that wraps the existing one and
only affects job attempts and outbox publications).
Delivery is **at-least-once**, so make handlers idempotent. Long handlers
should watch `context.cancellation()` (timeout, lost lease or shutdown).

Register handlers, queues and schedules in a composition hook:

```rust
use forge::jobs::{Schedule, WorkerSettings};

pub fn compose(composition: &mut forge::Composition<'_>) -> Result<(), ComposeError> {
    let database = composition.postgres()?.clone();
    let jobs = composition.jobs()?;
    jobs.register(WelcomeEmailHandler { /* ... */ }).map_err(ComposeError::other)?
        .concurrency("mailers", 10).map_err(ComposeError::other)?
        .schedule(
            Schedule::new("nightly.purge-sessions", "0 3 * * *".parse().map_err(ComposeError::other)?, &PurgeSessions)
                .map_err(ComposeError::other)?,
        )
        .map_err(ComposeError::other)?;
    jobs.settings(WorkerSettings::default().drain_timeout(std::time::Duration::from_secs(25)));
    let _ = database;
    Ok(())
}
```

## Enqueue

Enqueue inside the transaction that writes the business state, from
infrastructure code (a repository or unit-of-work adapter behind an
application port):

```rust
use forge::jobs::{EnqueueOptions, postgres::enqueue};

let mut transaction = database.begin().await?;
// ... INSERT the user ...
enqueue(
    &mut transaction,
    &SendWelcomeEmail { user_id },
    EnqueueOptions::new()
        .delay(std::time::Duration::from_secs(60))
        .unique_key(format!("welcome:{user_id}")), // duplicate enqueues return Enqueued::Duplicate
).await?;
transaction.commit().await?; // rolled back => no job
```

Options: `.at(Timestamp)` or `.delay(Duration)` (database clock),
`.priority(i16)`, `.unique_key(..)` (1–255 bytes, unique per tenant scope
until the finished job is pruned). Payloads are JSON, at most 256 KiB, and
are never logged; put references in them, not secrets.

Tenant-owned jobs use `enqueue_for_tenant(&mut tx, &tenant_context, &job,
options)` inside the tenant's transaction (the one with the
transaction-local `forge.tenant_id`/`forge.principal_id` settings of ADR
0011); RLS rejects it anywhere else. The worker reinstalls that scope for the
handler:

```rust
use forge::jobs::postgres::{begin_job_transaction, complete_in};

let mut transaction = begin_job_transaction(&database, context).await?; // RLS scope of the job
// ... tenant-scoped writes ...
complete_in(&mut transaction, context).await?; // completion commits with the effects
transaction.commit().await?;
```

`complete_in` makes database-only effects exactly-once: the effects and the
job's completion commit together, and a worker that lost its lease gets
`JobsError::LeaseLost` and must roll back.

## Run the workers

```sh
cargo run -- migrate        # FORGE_MIGRATION_DATABASE_URL: app + framework schemas
cargo run -- serve          # WorkerMode::InServe: HTTP + workers + schedules + dispatcher
cargo run -- jobs work      # workers, schedules and dispatcher without HTTP (also `jobs`)
```

Both use the runtime `FORGE_DATABASE_URL`. `SIGTERM`/`SIGINT` stops claiming,
waits for running attempts (`drain_timeout`, default 20 s), cancels the rest
(`cancel_grace`, default 5 s), aborts them and releases their leases without
consuming an attempt. Keep the orchestrator's stop timeout above the sum.

Each queue runs at most `concurrency` attempts (default 5,
`Jobs::concurrency`, at most 256). Idle queues poll every `poll_interval`
(default 1 s, jittered). Leases last `lease` (default 30 s) and are renewed
every 10 s; a worker that stops renewing loses its jobs to other workers
after at most one lease. An attempt whose lease has not been renewed for two
thirds of it (database unreachable, for example) is cancelled before the
lease can expire, and aborted after `cancel_grace` if it ignores the
cancellation, so it never runs alongside a reclaimed copy for long nor keeps
its concurrency slot until `TIMEOUT`.

## Failures and dead jobs

| Outcome | Result |
| --- | --- |
| `Ok(())` | `completed`; pruned after `Pruning::completed_after` (default 1 day) |
| retryable error, timeout, panic | back to `queued` after the backoff (capped exponential, equal jitter) |
| attempts exhausted, or `JobError::permanent` | `dead` until an operator acts |
| worker crashed / lease expired | reclaimed by another worker; the attempt counts |
| shutdown interrupted the attempt | released; the attempt does not count |

```sh
cargo run -- jobs status               # counts per queue and state, oldest due time
cargo run -- jobs dead 20              # dead jobs, newest first (no payloads)
cargo run -- jobs retry <JOB_ID>       # requeue with a fresh attempt budget
cargo run -- jobs discard <JOB_ID>     # delete a dead job
cargo run -- jobs dead-events          # dead outbox events
cargo run -- jobs retry-event <EVENT_ID>
```

## Cron schedules

`Schedule::new(name, cron, &job)` enqueues a copy of `job` at every tick of
a five-field UTC expression: `minute hour day-of-month month day-of-week`,
with `*`, values, ranges, steps, lists, `jan-dec`, `sun-sat` (0 and 7 are
Sunday) and `@hourly`, `@daily`, `@weekly`, `@monthly`, `@yearly`. When both
day fields are restricted, a day matches either (Vixie cron).

Every replica may run the scheduler: a tick fires **exactly once** because
the replica that advances the schedule's `last_tick_at` row enqueues the job
in the same transaction. Missed ticks are not replayed; the latest due tick
is skipped when it is older than `max_lateness` (default 5 minutes).

## Outbox and idempotent consumers

```rust
use forge::events::{IntegrationEvent, EventEnvelope, EventSubscriber, PublishError, Subscribers};
use forge::events::postgres::{append, record_delivery, Delivery};

#[derive(serde::Serialize, serde::Deserialize)]
struct InvoicePaid { invoice_id: uuid::Uuid }

impl IntegrationEvent for InvoicePaid {
    const TYPE: &'static str = "billing.invoice_paid";
    const VERSION: u16 = 1; // publish a new version instead of changing meaning
}

// In the use case's transaction:
append(&mut transaction, &InvoicePaid { invoice_id }).await?;

// A consumer applies each event once:
struct Ledger { database: forge::db::PgDatabase }

#[async_trait::async_trait]
impl EventSubscriber for Ledger {
    async fn handle(&self, event: &EventEnvelope) -> Result<(), PublishError> {
        let paid: InvoicePaid = event.decode().map_err(PublishError::permanent)?;
        let mut transaction = self.database.begin().await?;
        if record_delivery(&mut transaction, "billing.ledger", event.id()).await.map_err(PublishError::retry)?
            == Delivery::First
        {
            // ... effects with `transaction` ...
            transaction.commit().await?;
        }
        let _ = paid;
        Ok(())
    }
}

// In the composition hook:
let mut subscribers = Subscribers::new();
subscribers.subscribe::<InvoicePaid>(Ledger { database }).map_err(ComposeError::other)?;
composition.events()?.publish_to(subscribers);
```

The dispatcher publishes committed events in id order (best effort; retries
and concurrent dispatchers can reorder), with a per-event timeout, retries
with backoff (default 20 attempts) and a dead state. `Subscribers` delivers an
event to all subscribers of its type and retries all of them if one fails, so
every consumer must use the inbox. Any broker adapter can implement
`EventPublisher` instead.

## Database, roles and RLS

The schemas are embedded migrations with their own bookkeeping tables
(`_forge_jobs_migrations`, `_forge_events_migrations`); `rollback` reverts
only application migrations. Grants (ADR 0011):

| Object | Runtime role (serves requests) | Worker role (runs `jobs work`, operator commands) |
| --- | --- | --- |
| `forge_jobs`, `forge_outbox_events` | `SELECT` and `INSERT` on the enqueue/append columns only, filtered by RLS to the transaction's tenant scope | same |
| `forge_inbox` | `SELECT`, `INSERT (consumer, event_id)` | same |
| `forge_job_schedules` | none (only through `forge_jobs_fire_tick`) | none |
| `forge_jobs_*`, `forge_outbox_*` functions | `forge_jobs_complete` only | all (`SECURITY DEFINER`, pinned `search_path`) |
| bookkeeping tables | none | none |

By default there is a single role, `app_runtime`, which then also gets the
worker column. The worker functions are cross-tenant: claims return every
tenant's payloads, `stats`/`dead` other tenants' metadata. They refuse to run
in a transaction that carries `forge.tenant_id` (a request-serving scope),
and a separate worker role keeps them away from the serving process
entirely:

```sql
-- as the migration role, before `migrate`
ALTER DATABASE shop SET forge.runtime_role = 'shop_runtime'; -- default app_runtime
ALTER DATABASE shop SET forge.worker_role  = 'shop_worker';  -- default: the runtime role
-- the worker role usually also needs the application's own table grants:
-- GRANT shop_runtime TO shop_worker;
```

Then run `serve` with `App::jobs(WorkerMode::Dedicated)` and the runtime
role's `FORGE_DATABASE_URL`, and `jobs work` / the operator commands with
the worker role's `FORGE_DATABASE_URL`. Both settings must name existing
roles, or the migration fails.

## Tests

PostgreSQL tests are `#[ignore]`d unless a server is available:

```sh
FORGE_TEST_DATABASE_URL=postgres://postgres:postgres@127.0.0.1:5432/postgres \
  cargo test -p forge-jobs -p forge-events -p forge --all-features -- --include-ignored
```

They run as a non-superuser, non-owner runtime role created per test
(`forge_jobs::testing::RlsTestDatabase`, feature `testing`), and cover
transactional enqueue, many concurrent workers without double execution,
lease expiry and fencing, retries and dead jobs, timeouts, panics, graceful
drain, exactly-once schedule ticks across two runtimes, RLS tenant isolation,
outbox atomicity and consumer idempotency.

## Limits and follow-ups

- Tenant jobs do not re-check membership at execution time (ADR 0025).
- Polling only; `LISTEN/NOTIFY` wake-ups are not implemented.
- No per-key concurrency limits or rate limits yet; use queues and their
  concurrency.
- `forge new` wires the job queue but not the outbox (`App::events`), and
  MySQL, MariaDB and SQLite applications get no job queue (Forge-Go runs its
  in-process scheduler on MySQL and MariaDB too).
