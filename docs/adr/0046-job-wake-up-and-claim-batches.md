# ADR 0046: Job wake-up on enqueue, worker claim batches and the outbox batch default

- Status: Accepted
- Date: 2026-10-26
- Refines: [ADR 0025](0025-durable-jobs-scheduler-and-outbox.md) (decision 14,
  polling; the outbox batch)
- Parity with: Forge-Go ADR 0047 (batch claims, `LISTEN/NOTIFY` wake-up and
  recorded scheduler ticks)
- Issue: [forge-rust#80](https://github.com/rocketinsidecorp/forge-rust/issues/80)

## Context

Forge-Go ADR 0047 added an opt-in PostgreSQL `LISTEN/NOTIFY` wake-up for idle
job workers (`queuepostgres.WithNotify()`), a per-claim batch bound
(`WorkerOptions.BatchSize`, default 10, at most 100) and a 10-event outbox
batch. Forge-Rust already claimed up to a queue's free slots in one statement
and recorded each scheduler tick once, but had no wake-up and no batch knob,
and its outbox claimed 50 events per transaction (`DispatcherSettings::batch`).
The two frameworks must keep the same capabilities, names and defaults.

Two Forge-Rust facts shape the port. Jobs are enqueued by free functions on
the caller's transaction (`postgres::enqueue`, `enqueue_for_tenant`) and by the
`forge_jobs_fire_tick` definer function, not through a store object that could
carry an option. And PostgreSQL serializes the commits of every transaction
that sent a notification (a global lock held through the commit), so
announcing unconditionally would tax every application's enqueue throughput,
listening or not.

## Decision

- **Opt-in by session setting.** `forge.jobs_notify` (`NOTIFY_SETTING`) is the
  one switch, with the meaning of Go's `WithNotify`: when the connections of
  every process carry it on (`PGOPTIONS='-c forge.jobs_notify=on'`, or
  `?options=-c%20forge.jobs_notify%3Don` in the database URL; `on`, `true`,
  `yes` or `1`), enqueues announce due jobs and `Jobs::run` listens. Unset,
  nothing is announced and no `LISTEN` connection is opened. A transaction can
  also turn it on or off for itself with `set_config('forge.jobs_notify', ..., true)`.
- **Announcement.** Job migration 2 (`2_forge_jobs_notify`, in
  `_forge_jobs_migrations`; version 1 is released history and is not edited)
  adds the row trigger `forge_jobs_announce` `AFTER INSERT ON forge_jobs`,
  whose `WHEN` clause selects queued rows due at insert time
  (`run_at <= clock_timestamp()`) of sessions with the setting on, and which
  runs `pg_notify('forge_jobs', queue)`. A trigger covers every enqueue path,
  including schedule ticks; `ON CONFLICT DO NOTHING` duplicates insert no row
  and fire nothing. Notifications are delivered at commit, never after a
  rollback, and PostgreSQL merges identical ones, so a transaction announces
  each queue once. The function runs with the inserter's rights, pins its
  `search_path` and has no `EXECUTE` grant (a trigger needs none to fire); the
  worker-grant check skips trigger functions.
- **Listener.** `Jobs::run` reads the setting from its store's session at
  start and, when it is on and some queue has handlers, spawns one listener
  task in its own `JoinSet` next to the queue workers: no detached task. The
  listener opens one connection on a one-connection pool built from the
  store's connect options (outside the application's pool, like Go's hijacked
  connection), runs `LISTEN "forge_jobs"`, and forwards each payload that is a
  valid queue name of this runtime; anything else is ignored. On every
  (re)subscription it wakes every queue once (catch-up claim). When the
  connection fails it reconnects after a jittered `[b/2, b]`, `b` starting at
  1 s and doubling to 30 s while subscribing fails, resetting after a
  subscription that worked (Go's 0.5 s to 1 s, doubling to 15 s to 30 s);
  polling never stops. On shutdown, and after every lost connection, the
  subscription is closed and its pool closed with a one-second bound, so no
  `LISTEN` session outlives `Jobs::run`.
- **Wake-up.** Each queue worker has a capacity-one channel: announcements
  merge while the worker is busy. An idle worker below capacity claims when
  its signal fires, but no sooner than 10 ms after its previous claim (Go's
  `minWakeGap`), so a flood of notifications cannot make it spin.
- **Claim batches.** `WorkerSettings::batch` (default 10, 1 to
  `MAX_CLAIM_BATCH` = 100, Go's `BatchSize`) bounds every claim, which never
  asks for more than the queue's free slots. A claim that returns a full batch
  is followed at once by another while slots remain, instead of an idle poll.
- **Outbox batch.** `DispatcherSettings::batch` defaults to 10 (was 50), like
  Go's `DispatcherOptions.BatchSize`. Events of a batch are published one
  after another, so a slow handler holds up the rest of its batch; 10 keeps
  that wait short and still claims a tenth as often as one event at a time.
  The batch lease shrinks with it (default 130 s).

### Rejected

- **Always announcing** (the trigger without the setting): every
  application would pay PostgreSQL's commit serialization whether or not a
  worker listens.
- **A per-call or per-store option** (`EnqueueOptions::notify`, a store
  wrapper for transactions): every enqueue site and generated job would need
  to pass it, and `forge_jobs_fire_tick` could not see it.
- **SQLx's automatic reconnect** (`PgListener` with `eager_reconnect`): it
  reconnects immediately and without backoff, and gives no point at which to
  run the catch-up claims.
- **Listening on the application's pool**: the subscription would hold one of
  the pool's connections for the process lifetime.

## Consequences

- With the setting on, an idle worker claims a newly enqueued due job within
  milliseconds instead of at its next poll (default 1 s); without it nothing
  changes.
- Costs of turning it on, as in Go: one extra connection per job runtime;
  `LISTEN` needs a direct connection or session pooling, not PgBouncer
  transaction pooling (a startup option cannot pass through it either; set it
  per transaction there); notifying commits serialize, which matters at
  thousands of enqueues per second.
- Anyone who can `NOTIFY` on the database can wake workers; each wakes at
  most once per 10 ms and claims only what is due.
- Announcements are hints: a lost one (listener down, a job due later, a job
  requeued by a retry or released at shutdown) is found by polling.
- `sqlx::PgListener` buffers notifications received while a statement runs
  on its connection in an internal unbounded channel and returns its
  connection from a task it spawns on drop. The listener runs no statement
  after `LISTEN`, and closing the subscription's own pool awaits that task
  (bounded), the accepted exception already recorded for
  `forge-broadcast` in the [dependency policy](../dependency-policy.md).
- Remaining differences from Go: the switch is a session setting instead of a
  store option (Rust enqueues through free functions and a definer function);
  Rust has no `Listening()` accessor (the listener logs `listening for job
  announcements` and each disconnect); Rust idle workers poll at a fixed
  `poll_interval` instead of Go's doubling to `MaxPollInterval`.
- Tests (PostgreSQL, `crates/forge-jobs/src/pg_notify_tests.rs`): only due,
  committed jobs of opted-in sessions are announced, once per queue and
  transaction, schedule ticks included; an idle worker polling every minute
  starts announced jobs in under 2 s; after `pg_terminate_backend` of the
  listener, polling still runs jobs and a new `LISTEN` backend appears; a
  shutdown with an active listener returns in under 2 s and leaves no `LISTEN`
  backend, and a runtime without the setting never listens; claims never
  exceed the batch or the free slots and a full batch is followed by another
  claim at once.
