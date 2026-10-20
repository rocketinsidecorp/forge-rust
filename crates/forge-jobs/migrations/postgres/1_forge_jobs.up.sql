-- Forge durable jobs (ADR 0025). Applied by the schema-owning migration role;
-- bookkeeping lives in `_forge_jobs_migrations`, separate from the
-- application's own migrations.
--
-- The runtime role (ADR 0011) receives SELECT and column-restricted INSERT
-- on forge_jobs through row-level security. EXECUTE on the SECURITY DEFINER
-- functions below, the only cross-tenant paths (claiming, lease renewal,
-- completion/failure, schedules and the operator commands), goes to the
-- worker role. Names come from the `forge.runtime_role` (default
-- `app_runtime`) and `forge.worker_role` (default: the runtime role)
-- settings; with a separate worker role the runtime role keeps only
-- forge_jobs_complete, for transactional completion.

CREATE TABLE forge_jobs (
    id uuid PRIMARY KEY,
    tenant_id uuid,
    principal_id uuid,
    queue text NOT NULL,
    job_name text NOT NULL,
    payload jsonb NOT NULL,
    priority smallint NOT NULL DEFAULT 0,
    state text NOT NULL DEFAULT 'queued',
    attempts integer NOT NULL DEFAULT 0,
    max_attempts integer NOT NULL,
    run_at timestamptz NOT NULL,
    unique_key text,
    lease_token uuid,
    locked_until timestamptz,
    last_error text,
    created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    updated_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    finished_at timestamptz,
    CONSTRAINT forge_jobs_queue_format
        CHECK (queue ~ '^[a-z0-9]([a-z0-9_.:-]{0,62}[a-z0-9])?$'),
    CONSTRAINT forge_jobs_name_format
        CHECK (job_name ~ '^[a-z0-9]([a-z0-9_.:-]{0,126}[a-z0-9])?$'),
    CONSTRAINT forge_jobs_state_known
        CHECK (state IN ('queued', 'running', 'completed', 'dead')),
    CONSTRAINT forge_jobs_attempt_budget
        CHECK (max_attempts BETWEEN 1 AND 1000 AND attempts BETWEEN 0 AND max_attempts),
    -- Measured on the stored (canonical) JSON text; mirrors MAX_PAYLOAD_BYTES.
    CONSTRAINT forge_jobs_payload_size
        CHECK (octet_length(payload::text) <= 262144),
    CONSTRAINT forge_jobs_unique_key_length
        CHECK (unique_key IS NULL OR octet_length(unique_key) BETWEEN 1 AND 255),
    CONSTRAINT forge_jobs_last_error_length
        CHECK (last_error IS NULL OR octet_length(last_error) <= 2048),
    CONSTRAINT forge_jobs_lease_only_while_running
        CHECK (
            (state = 'running') = (lease_token IS NOT NULL)
            AND (lease_token IS NULL) = (locked_until IS NULL)
        ),
    CONSTRAINT forge_jobs_finished_when_terminal
        CHECK ((state IN ('completed', 'dead')) = (finished_at IS NOT NULL)),
    CONSTRAINT forge_jobs_principal_requires_tenant
        CHECK (principal_id IS NULL OR tenant_id IS NOT NULL)
);

-- Claiming: the next due jobs of a queue in priority, time and id order.
CREATE INDEX forge_jobs_ready_idx
ON forge_jobs (queue, priority, run_at, id)
WHERE state = 'queued';

-- Reaping leases whose worker stopped renewing them.
CREATE INDEX forge_jobs_lease_idx
ON forge_jobs (queue, locked_until)
WHERE state = 'running';

-- Bounded pruning and the dead-letter listing.
CREATE INDEX forge_jobs_completed_idx
ON forge_jobs (finished_at)
WHERE state = 'completed';

CREATE INDEX forge_jobs_dead_idx
ON forge_jobs (finished_at, id)
WHERE state = 'dead';

-- Idempotency keys are unique per tenant scope (global jobs share one scope).
CREATE UNIQUE INDEX forge_jobs_unique_key_idx
ON forge_jobs ((coalesce(tenant_id, '00000000-0000-0000-0000-000000000000'::uuid)), unique_key)
WHERE unique_key IS NOT NULL;

CREATE INDEX forge_jobs_tenant_idx
ON forge_jobs (tenant_id, id)
WHERE tenant_id IS NOT NULL;

-- One row per recurring schedule: the latest tick that has been enqueued.
-- The conditional upsert on this row is what makes a tick fire exactly once
-- across replicas, and it stays correct after finished jobs are pruned.
CREATE TABLE forge_job_schedules (
    name text PRIMARY KEY,
    last_tick_at timestamptz NOT NULL,
    updated_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    CONSTRAINT forge_job_schedules_name_format
        CHECK (name ~ '^[a-z0-9]([a-z0-9_.:-]{0,126}[a-z0-9])?$')
);

-- Row-level security (ADR 0004/0011): a transaction sees and writes exactly
-- the jobs of its tenant scope. Without `forge.tenant_id` that scope is the
-- global (tenant-less) jobs; a malformed setting fails the statement.
-- Idempotency keys starting with `forge.schedule:` are reserved for
-- forge_jobs_fire_tick, so the runtime role cannot pre-empt a schedule tick.
ALTER TABLE forge_jobs ENABLE ROW LEVEL SECURITY;

CREATE POLICY forge_jobs_tenant_scope ON forge_jobs
    USING (
        tenant_id IS NOT DISTINCT FROM
            nullif(current_setting('forge.tenant_id', true), '')::uuid
    )
    WITH CHECK (
        tenant_id IS NOT DISTINCT FROM
            nullif(current_setting('forge.tenant_id', true), '')::uuid
        AND (
            tenant_id IS NULL
            OR principal_id IS NOT DISTINCT FROM
                nullif(current_setting('forge.principal_id', true), '')::uuid
        )
        AND (unique_key IS NULL OR unique_key NOT LIKE 'forge.schedule:%')
    );

-- Cross-tenant operations are refused inside a tenant-scoped (request
-- serving) transaction: defense in depth against application code calling
-- worker or operator functions on behalf of a tenant.
CREATE FUNCTION forge_jobs_require_worker_scope()
RETURNS void
LANGUAGE plpgsql
AS $$
BEGIN
    IF nullif(current_setting('forge.tenant_id', true), '') IS NOT NULL THEN
        RAISE EXCEPTION 'forge job worker functions are not available in a tenant-scoped transaction'
            USING ERRCODE = '42501';
    END IF;
END;
$$;

-- Claims up to p_limit due jobs of one queue whose names the worker can run.
-- Leases that expired first go back to the queue, or to the dead state when
-- the attempt budget is spent. Every claimed job receives p_lease_token; all
-- later transitions must present it (fencing).
CREATE FUNCTION forge_jobs_claim(
    p_queue text,
    p_job_names text[],
    p_limit integer,
    p_lease_token uuid,
    p_lease_ms bigint
)
RETURNS TABLE (
    id uuid,
    tenant_id uuid,
    principal_id uuid,
    job_name text,
    payload text,
    attempts integer,
    max_attempts integer
)
LANGUAGE plpgsql
SECURITY DEFINER
AS $$
#variable_conflict use_column
BEGIN
    PERFORM forge_jobs_require_worker_scope();
    IF p_limit IS NULL OR p_limit < 1 OR p_limit > 1000
        OR p_lease_ms IS NULL OR p_lease_ms < 1000 OR p_lease_ms > 86400000
        OR p_lease_token IS NULL
    THEN
        RAISE EXCEPTION 'invalid forge_jobs_claim arguments' USING ERRCODE = '22023';
    END IF;

    WITH expired AS MATERIALIZED (
        SELECT e.id
        FROM forge_jobs AS e
        WHERE e.queue = p_queue
          AND e.state = 'running'
          AND e.locked_until < now()
        ORDER BY e.locked_until
        LIMIT 100
        FOR UPDATE SKIP LOCKED
    )
    UPDATE forge_jobs AS j
    SET state = CASE WHEN j.attempts >= j.max_attempts THEN 'dead' ELSE 'queued' END,
        run_at = now(),
        lease_token = NULL,
        locked_until = NULL,
        finished_at = CASE WHEN j.attempts >= j.max_attempts THEN now() END,
        last_error = 'lease expired before the job finished',
        updated_at = now()
    FROM expired
    WHERE j.id = expired.id;

    RETURN QUERY
    WITH candidates AS MATERIALIZED (
        SELECT c.id
        FROM forge_jobs AS c
        WHERE c.queue = p_queue
          AND c.state = 'queued'
          AND c.run_at <= now()
          AND c.job_name = ANY (p_job_names)
        ORDER BY c.priority, c.run_at, c.id
        LIMIT p_limit
        FOR UPDATE SKIP LOCKED
    )
    UPDATE forge_jobs AS j
    SET state = 'running',
        attempts = j.attempts + 1,
        lease_token = p_lease_token,
        locked_until = now() + p_lease_ms * interval '1 millisecond',
        updated_at = now()
    FROM candidates
    WHERE j.id = candidates.id
    RETURNING j.id, j.tenant_id, j.principal_id, j.job_name, j.payload::text,
              j.attempts, j.max_attempts;
END;
$$;

-- Extends the leases still held with the presented tokens; returns their ids.
-- A missing id means the lease was lost and the worker must stop the job.
CREATE FUNCTION forge_jobs_heartbeat(p_ids uuid[], p_tokens uuid[], p_lease_ms bigint)
RETURNS SETOF uuid
LANGUAGE sql
SECURITY DEFINER
AS $$
    SELECT forge_jobs_require_worker_scope();
    UPDATE forge_jobs AS j
    SET locked_until = now() + least(greatest(p_lease_ms, 1000), 86400000) * interval '1 millisecond',
        updated_at = now()
    FROM unnest(p_ids, p_tokens) AS held(id, token)
    WHERE j.id = held.id
      AND j.lease_token = held.token
      AND j.state = 'running'
    RETURNING j.id;
$$;

-- Marks a job completed. False when the lease token no longer matches.
CREATE FUNCTION forge_jobs_complete(p_id uuid, p_lease_token uuid)
RETURNS boolean
LANGUAGE sql
SECURITY DEFINER
AS $$
    WITH done AS (
        UPDATE forge_jobs
        SET state = 'completed',
            lease_token = NULL,
            locked_until = NULL,
            last_error = NULL,
            finished_at = now(),
            updated_at = now()
        WHERE id = p_id
          AND lease_token = p_lease_token
          AND state = 'running'
        RETURNING 1
    )
    SELECT EXISTS (SELECT 1 FROM done);
$$;

-- Records a failed attempt: back to the queue after p_retry_in_ms, or dead
-- when p_discard is set or the attempt budget is spent. Returns the new state,
-- or NULL when the lease token no longer matches.
CREATE FUNCTION forge_jobs_fail(
    p_id uuid,
    p_lease_token uuid,
    p_error text,
    p_retry_in_ms bigint,
    p_discard boolean
)
RETURNS text
LANGUAGE sql
SECURITY DEFINER
AS $$
    SELECT forge_jobs_require_worker_scope();
    UPDATE forge_jobs
    SET state = CASE WHEN p_discard OR attempts >= max_attempts THEN 'dead' ELSE 'queued' END,
        run_at = CASE
            WHEN p_discard OR attempts >= max_attempts THEN run_at
            ELSE now() + least(greatest(p_retry_in_ms, 0), 31536000000) * interval '1 millisecond'
        END,
        finished_at = CASE WHEN p_discard OR attempts >= max_attempts THEN now() END,
        lease_token = NULL,
        locked_until = NULL,
        last_error = left(p_error, 1024),
        updated_at = now()
    WHERE id = p_id
      AND lease_token = p_lease_token
      AND state = 'running'
    RETURNING state;
$$;

-- Returns an interrupted job to the queue without consuming an attempt
-- (graceful shutdown). False when the lease token no longer matches.
CREATE FUNCTION forge_jobs_release(p_id uuid, p_lease_token uuid)
RETURNS boolean
LANGUAGE sql
SECURITY DEFINER
AS $$
    SELECT forge_jobs_require_worker_scope();
    WITH released AS (
        UPDATE forge_jobs
        SET state = 'queued',
            attempts = greatest(attempts - 1, 0),
            run_at = now(),
            lease_token = NULL,
            locked_until = NULL,
            updated_at = now()
        WHERE id = p_id
          AND lease_token = p_lease_token
          AND state = 'running'
        RETURNING 1
    )
    SELECT EXISTS (SELECT 1 FROM released);
$$;

-- Operator: requeue a dead job with a fresh attempt budget.
CREATE FUNCTION forge_jobs_retry(p_id uuid)
RETURNS boolean
LANGUAGE sql
SECURITY DEFINER
AS $$
    SELECT forge_jobs_require_worker_scope();
    WITH retried AS (
        UPDATE forge_jobs
        SET state = 'queued',
            attempts = 0,
            run_at = now(),
            finished_at = NULL,
            updated_at = now()
        WHERE id = p_id AND state = 'dead'
        RETURNING 1
    )
    SELECT EXISTS (SELECT 1 FROM retried);
$$;

-- Operator: delete a dead job.
CREATE FUNCTION forge_jobs_discard(p_id uuid)
RETURNS boolean
LANGUAGE sql
SECURITY DEFINER
AS $$
    SELECT forge_jobs_require_worker_scope();
    WITH discarded AS (
        DELETE FROM forge_jobs WHERE id = p_id AND state = 'dead' RETURNING 1
    )
    SELECT EXISTS (SELECT 1 FROM discarded);
$$;

-- Operator: dead jobs, newest first, keyset-paginated by id. Payloads are
-- never returned.
CREATE FUNCTION forge_jobs_dead(p_limit integer, p_before uuid)
RETURNS TABLE (
    id uuid,
    tenant_id uuid,
    queue text,
    job_name text,
    attempts integer,
    max_attempts integer,
    last_error text,
    failed_at_us bigint
)
LANGUAGE sql
SECURITY DEFINER
AS $$
    SELECT forge_jobs_require_worker_scope();
    SELECT j.id, j.tenant_id, j.queue, j.job_name, j.attempts, j.max_attempts, j.last_error,
           (extract(epoch FROM j.finished_at) * 1000000)::bigint
    FROM forge_jobs AS j
    WHERE j.state = 'dead'
      AND (p_before IS NULL OR j.id < p_before)
    ORDER BY j.id DESC
    LIMIT least(greatest(p_limit, 1), 500);
$$;

-- Operator: job counts per queue and state, with the oldest due time of
-- queued jobs (queue latency).
CREATE FUNCTION forge_jobs_stats()
RETURNS TABLE (queue text, state text, jobs bigint, oldest_run_at_us bigint)
LANGUAGE sql
SECURITY DEFINER
AS $$
    SELECT forge_jobs_require_worker_scope();
    SELECT j.queue, j.state, count(*),
           (extract(epoch FROM min(j.run_at) FILTER (WHERE j.state = 'queued')) * 1000000)::bigint
    FROM forge_jobs AS j
    GROUP BY j.queue, j.state
    ORDER BY j.queue, j.state;
$$;

-- Deletes at most p_limit finished jobs: completed ones older than
-- p_completed_age_ms, and dead ones older than p_dead_age_ms when it is not
-- NULL. Returns the number of rows deleted.
CREATE FUNCTION forge_jobs_prune(p_completed_age_ms bigint, p_dead_age_ms bigint, p_limit integer)
RETURNS bigint
LANGUAGE sql
SECURITY DEFINER
AS $$
    SELECT forge_jobs_require_worker_scope();
    WITH doomed AS MATERIALIZED (
        SELECT j.id
        FROM forge_jobs AS j
        WHERE (j.state = 'completed'
               AND j.finished_at < now() - greatest(p_completed_age_ms, 0) * interval '1 millisecond')
           OR (p_dead_age_ms IS NOT NULL
               AND j.state = 'dead'
               AND j.finished_at < now() - greatest(p_dead_age_ms, 0) * interval '1 millisecond')
        LIMIT least(greatest(p_limit, 1), 10000)
        FOR UPDATE SKIP LOCKED
    ),
    deleted AS (
        DELETE FROM forge_jobs AS j USING doomed WHERE j.id = doomed.id RETURNING 1
    )
    SELECT count(*) FROM deleted;
$$;

-- Records a schedule tick and enqueues its job, exactly once across replicas.
-- The database clock decides: a tick later than now() is refused ('early',
-- so a replica with a fast clock can neither fire early nor block the
-- others), a tick older than now() - p_lateness_ms is recorded but not
-- enqueued ('skipped'), and only the caller that advances last_tick_at
-- enqueues ('fired'; 'handled' otherwise). The idempotency key is
-- reserved: row-level security keeps the runtime role from inserting it.
CREATE FUNCTION forge_jobs_fire_tick(
    p_schedule text,
    p_tick_us bigint,
    p_lateness_ms bigint,
    p_id uuid,
    p_queue text,
    p_job_name text,
    p_payload text,
    p_priority smallint,
    p_max_attempts integer,
    p_unique_key text
)
RETURNS text
LANGUAGE plpgsql
SECURITY DEFINER
AS $$
DECLARE
    tick timestamptz := timestamptz 'epoch' + p_tick_us * interval '1 microsecond';
    advanced boolean;
BEGIN
    PERFORM forge_jobs_require_worker_scope();
    IF tick > now() THEN
        RETURN 'early';
    END IF;
    INSERT INTO forge_job_schedules AS s (name, last_tick_at)
    VALUES (p_schedule, tick)
    ON CONFLICT (name) DO UPDATE
        SET last_tick_at = EXCLUDED.last_tick_at, updated_at = clock_timestamp()
        WHERE s.last_tick_at < EXCLUDED.last_tick_at
    RETURNING true INTO advanced;
    IF advanced IS NULL THEN
        RETURN 'handled';
    END IF;
    IF tick < now() - greatest(p_lateness_ms, 0) * interval '1 millisecond' THEN
        RETURN 'skipped';
    END IF;
    INSERT INTO forge_jobs (
        id, queue, job_name, payload, priority, max_attempts, run_at, unique_key
    ) VALUES (
        p_id, p_queue, p_job_name, p_payload::jsonb, p_priority, p_max_attempts, now(),
        p_unique_key
    )
    ON CONFLICT ((coalesce(tenant_id, '00000000-0000-0000-0000-000000000000'::uuid)), unique_key)
        WHERE unique_key IS NOT NULL
    DO NOTHING;
    IF FOUND THEN
        RETURN 'fired';
    END IF;
    RETURN 'handled';
END;
$$;

-- The database clock, for schedulers on replicas whose clocks disagree.
CREATE FUNCTION forge_jobs_now_us()
RETURNS bigint
LANGUAGE sql
SECURITY DEFINER
AS $$
    SELECT (extract(epoch FROM clock_timestamp()) * 1000000)::bigint;
$$;

REVOKE ALL ON TABLE forge_jobs FROM PUBLIC;
REVOKE ALL ON TABLE forge_job_schedules FROM PUBLIC;

DO $$
DECLARE
    schema_name text := current_schema();
    explicit_runtime text := nullif(current_setting('forge.runtime_role', true), '');
    explicit_worker text := nullif(current_setting('forge.worker_role', true), '');
    runtime_role text := coalesce(explicit_runtime, 'app_runtime');
    worker_role text;
    grantee text;
    fn regprocedure;
BEGIN
    -- SECURITY DEFINER functions run with the owner's rights: pin their
    -- search path to this schema (pg_temp last) and revoke the default
    -- EXECUTE from PUBLIC.
    FOR fn IN
        SELECT p.oid::regprocedure
        FROM pg_proc AS p
        WHERE p.proname LIKE 'forge\_jobs\_%'
          AND p.pronamespace = schema_name::regnamespace
    LOOP
        EXECUTE format('ALTER FUNCTION %s SET search_path = %I, pg_temp', fn, schema_name);
        EXECUTE format('REVOKE ALL ON FUNCTION %s FROM PUBLIC', fn);
    END LOOP;

    IF explicit_runtime IS NOT NULL
        AND NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = explicit_runtime)
    THEN
        RAISE EXCEPTION 'forge.runtime_role names a role that does not exist';
    END IF;
    IF explicit_worker IS NOT NULL
        AND NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = explicit_worker)
    THEN
        RAISE EXCEPTION 'forge.worker_role names a role that does not exist';
    END IF;
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = runtime_role) THEN
        runtime_role := NULL;
    END IF;
    -- Without a separate worker role (`forge.worker_role`), the runtime role
    -- also runs the workers and operator commands.
    worker_role := coalesce(explicit_worker, runtime_role);

    FOREACH grantee IN ARRAY array_remove(ARRAY[runtime_role, worker_role], NULL) LOOP
        EXECUTE format('REVOKE ALL ON TABLE forge_jobs, forge_job_schedules FROM %I', grantee);
        -- Enqueue columns only: state, attempts, leases and timestamps always
        -- start from their defaults. Schedule bookkeeping is reachable only
        -- through forge_jobs_fire_tick.
        EXECUTE format('GRANT SELECT ON TABLE forge_jobs TO %I', grantee);
        EXECUTE format(
            'GRANT INSERT (id, tenant_id, principal_id, queue, job_name, payload, priority, '
            'max_attempts, run_at, unique_key) ON TABLE forge_jobs TO %I',
            grantee
        );
        IF to_regclass('_forge_jobs_migrations') IS NOT NULL THEN
            EXECUTE format('REVOKE ALL ON TABLE _forge_jobs_migrations FROM %I', grantee);
        END IF;
    END LOOP;

    IF worker_role IS NOT NULL THEN
        FOR fn IN
            SELECT p.oid::regprocedure
            FROM pg_proc AS p
            WHERE p.proname LIKE 'forge\_jobs\_%'
              AND p.pronamespace = schema_name::regnamespace
        LOOP
            EXECUTE format('GRANT EXECUTE ON FUNCTION %s TO %I', fn, worker_role);
        END LOOP;
    END IF;
    -- A request-serving runtime role separate from the worker role keeps
    -- only the fenced completion used by `complete_in`.
    IF runtime_role IS NOT NULL AND runtime_role IS DISTINCT FROM worker_role THEN
        EXECUTE format('GRANT EXECUTE ON FUNCTION forge_jobs_complete(uuid, uuid) TO %I', runtime_role);
    END IF;
END
$$;
