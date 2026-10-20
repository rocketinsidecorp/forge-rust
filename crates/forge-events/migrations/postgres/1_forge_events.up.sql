-- Forge transactional outbox and consumer inbox (ADR 0025). Applied by the
-- schema-owning migration role; bookkeeping lives in
-- `_forge_events_migrations`.
--
-- The runtime role (`forge.runtime_role`, default `app_runtime`) may append
-- events under row-level security and record processed events in the inbox;
-- dispatching and the operator commands go through SECURITY DEFINER
-- functions granted to the worker role (`forge.worker_role`, default: the
-- runtime role).

CREATE TABLE forge_outbox_events (
    id uuid PRIMARY KEY,
    tenant_id uuid,
    event_type text NOT NULL,
    version integer NOT NULL,
    payload jsonb NOT NULL,
    occurred_at timestamptz NOT NULL,
    state text NOT NULL DEFAULT 'pending',
    attempts integer NOT NULL DEFAULT 0,
    available_at timestamptz NOT NULL DEFAULT now(),
    lease_token uuid,
    locked_until timestamptz,
    last_error text,
    published_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    CONSTRAINT forge_outbox_events_type_format
        CHECK (event_type ~ '^[a-z0-9]([a-z0-9_.:-]{0,126}[a-z0-9])?$'),
    CONSTRAINT forge_outbox_events_version_range CHECK (version BETWEEN 1 AND 65535),
    -- Measured on the stored (canonical) JSON text; mirrors MAX_PAYLOAD_BYTES.
    CONSTRAINT forge_outbox_events_payload_size
        CHECK (octet_length(payload::text) <= 262144),
    CONSTRAINT forge_outbox_events_state_known
        CHECK (state IN ('pending', 'publishing', 'published', 'dead')),
    CONSTRAINT forge_outbox_events_attempts_positive CHECK (attempts >= 0),
    CONSTRAINT forge_outbox_events_last_error_length
        CHECK (last_error IS NULL OR octet_length(last_error) <= 2048),
    CONSTRAINT forge_outbox_events_lease_only_while_publishing
        CHECK (
            (state = 'publishing') = (lease_token IS NOT NULL)
            AND (lease_token IS NULL) = (locked_until IS NULL)
        ),
    CONSTRAINT forge_outbox_events_published_at
        CHECK ((state = 'published') = (published_at IS NOT NULL))
);

CREATE INDEX forge_outbox_events_pending_idx
ON forge_outbox_events (available_at, id)
WHERE state = 'pending';

CREATE INDEX forge_outbox_events_lease_idx
ON forge_outbox_events (locked_until)
WHERE state = 'publishing';

CREATE INDEX forge_outbox_events_published_idx
ON forge_outbox_events (published_at)
WHERE state = 'published';

CREATE INDEX forge_outbox_events_dead_idx
ON forge_outbox_events (id)
WHERE state = 'dead';

ALTER TABLE forge_outbox_events ENABLE ROW LEVEL SECURITY;

CREATE POLICY forge_outbox_events_tenant_scope ON forge_outbox_events
    USING (
        tenant_id IS NOT DISTINCT FROM
            nullif(current_setting('forge.tenant_id', true), '')::uuid
    )
    WITH CHECK (
        tenant_id IS NOT DISTINCT FROM
            nullif(current_setting('forge.tenant_id', true), '')::uuid
    );

-- Idempotent consumers: one row per (consumer, event) processed. Ids only,
-- no tenant data.
CREATE TABLE forge_inbox (
    consumer text NOT NULL,
    event_id uuid NOT NULL,
    processed_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (consumer, event_id),
    CONSTRAINT forge_inbox_consumer_format
        CHECK (consumer ~ '^[a-z0-9]([a-z0-9_.:-]{0,126}[a-z0-9])?$')
);

CREATE INDEX forge_inbox_processed_idx ON forge_inbox (processed_at);

-- Cross-tenant operations are refused inside a tenant-scoped (request
-- serving) transaction: defense in depth against application code calling
-- worker or operator functions on behalf of a tenant.
CREATE FUNCTION forge_outbox_require_worker_scope()
RETURNS void
LANGUAGE plpgsql
AS $$
BEGIN
    IF nullif(current_setting('forge.tenant_id', true), '') IS NOT NULL THEN
        RAISE EXCEPTION 'forge outbox functions are not available in a tenant-scoped transaction'
            USING ERRCODE = '42501';
    END IF;
END;
$$;

-- Claims up to p_limit committed, due events in id (time) order. Expired
-- leases go back to pending first, or to dead when p_max_attempts is spent.
CREATE FUNCTION forge_outbox_claim(
    p_limit integer,
    p_lease_token uuid,
    p_lease_ms bigint,
    p_max_attempts integer
)
RETURNS TABLE (
    id uuid,
    tenant_id uuid,
    event_type text,
    version integer,
    payload text,
    occurred_at_us bigint,
    attempts integer
)
LANGUAGE plpgsql
SECURITY DEFINER
AS $$
#variable_conflict use_column
BEGIN
    PERFORM forge_outbox_require_worker_scope();
    IF p_limit IS NULL OR p_limit < 1 OR p_limit > 1000
        OR p_lease_ms IS NULL OR p_lease_ms < 1000 OR p_lease_ms > 86400000
        OR p_max_attempts IS NULL OR p_max_attempts < 1
        OR p_lease_token IS NULL
    THEN
        RAISE EXCEPTION 'invalid forge_outbox_claim arguments' USING ERRCODE = '22023';
    END IF;

    WITH expired AS MATERIALIZED (
        SELECT e.id
        FROM forge_outbox_events AS e
        WHERE e.state = 'publishing' AND e.locked_until < now()
        ORDER BY e.locked_until
        LIMIT 100
        FOR UPDATE SKIP LOCKED
    )
    UPDATE forge_outbox_events AS o
    SET state = CASE WHEN o.attempts >= p_max_attempts THEN 'dead' ELSE 'pending' END,
        available_at = now(),
        lease_token = NULL,
        locked_until = NULL,
        last_error = 'lease expired before the event was published'
    FROM expired
    WHERE o.id = expired.id;

    RETURN QUERY
    WITH candidates AS MATERIALIZED (
        SELECT c.id
        FROM forge_outbox_events AS c
        WHERE c.state = 'pending' AND c.available_at <= now()
        ORDER BY c.id
        LIMIT p_limit
        FOR UPDATE SKIP LOCKED
    )
    UPDATE forge_outbox_events AS o
    SET state = 'publishing',
        attempts = o.attempts + 1,
        lease_token = p_lease_token,
        locked_until = now() + p_lease_ms * interval '1 millisecond'
    FROM candidates
    WHERE o.id = candidates.id
    RETURNING o.id, o.tenant_id, o.event_type, o.version, o.payload::text,
              (extract(epoch FROM o.occurred_at) * 1000000)::bigint, o.attempts;
END;
$$;

CREATE FUNCTION forge_outbox_published(p_id uuid, p_lease_token uuid)
RETURNS boolean
LANGUAGE sql
SECURITY DEFINER
AS $$
    SELECT forge_outbox_require_worker_scope();
    WITH done AS (
        UPDATE forge_outbox_events
        SET state = 'published',
            lease_token = NULL,
            locked_until = NULL,
            last_error = NULL,
            published_at = now()
        WHERE id = p_id AND lease_token = p_lease_token AND state = 'publishing'
        RETURNING 1
    )
    SELECT EXISTS (SELECT 1 FROM done);
$$;

-- Returns 'pending' (retry later), 'dead', or NULL when fenced out.
CREATE FUNCTION forge_outbox_fail(
    p_id uuid,
    p_lease_token uuid,
    p_error text,
    p_retry_in_ms bigint,
    p_max_attempts integer,
    p_discard boolean
)
RETURNS text
LANGUAGE sql
SECURITY DEFINER
AS $$
    SELECT forge_outbox_require_worker_scope();
    UPDATE forge_outbox_events
    SET state = CASE WHEN p_discard OR attempts >= p_max_attempts THEN 'dead' ELSE 'pending' END,
        available_at = now() + least(greatest(p_retry_in_ms, 0), 31536000000) * interval '1 millisecond',
        lease_token = NULL,
        locked_until = NULL,
        last_error = left(p_error, 1024)
    WHERE id = p_id AND lease_token = p_lease_token AND state = 'publishing'
    RETURNING state;
$$;

-- Returns a claimed event unpublished without consuming an attempt.
CREATE FUNCTION forge_outbox_release(p_id uuid, p_lease_token uuid)
RETURNS boolean
LANGUAGE sql
SECURITY DEFINER
AS $$
    SELECT forge_outbox_require_worker_scope();
    WITH released AS (
        UPDATE forge_outbox_events
        SET state = 'pending',
            attempts = greatest(attempts - 1, 0),
            available_at = now(),
            lease_token = NULL,
            locked_until = NULL
        WHERE id = p_id AND lease_token = p_lease_token AND state = 'publishing'
        RETURNING 1
    )
    SELECT EXISTS (SELECT 1 FROM released);
$$;

CREATE FUNCTION forge_outbox_retry(p_id uuid)
RETURNS boolean
LANGUAGE sql
SECURITY DEFINER
AS $$
    SELECT forge_outbox_require_worker_scope();
    WITH retried AS (
        UPDATE forge_outbox_events
        SET state = 'pending', attempts = 0, available_at = now()
        WHERE id = p_id AND state = 'dead'
        RETURNING 1
    )
    SELECT EXISTS (SELECT 1 FROM retried);
$$;

CREATE FUNCTION forge_outbox_dead(p_limit integer, p_before uuid)
RETURNS TABLE (
    id uuid,
    tenant_id uuid,
    event_type text,
    version integer,
    attempts integer,
    last_error text
)
LANGUAGE sql
SECURITY DEFINER
AS $$
    SELECT forge_outbox_require_worker_scope();
    SELECT o.id, o.tenant_id, o.event_type, o.version, o.attempts, o.last_error
    FROM forge_outbox_events AS o
    WHERE o.state = 'dead' AND (p_before IS NULL OR o.id < p_before)
    ORDER BY o.id DESC
    LIMIT least(greatest(p_limit, 1), 500);
$$;

-- Deletes at most p_limit published events older than p_age_ms, and inbox
-- entries older than p_inbox_age_ms when it is not NULL. Inbox retention
-- must exceed the longest possible redelivery delay.
CREATE FUNCTION forge_outbox_prune(p_age_ms bigint, p_inbox_age_ms bigint, p_limit integer)
RETURNS bigint
LANGUAGE plpgsql
SECURITY DEFINER
AS $$
DECLARE
    deleted bigint;
    inbox_deleted bigint := 0;
BEGIN
    PERFORM forge_outbox_require_worker_scope();
    WITH doomed AS MATERIALIZED (
        SELECT o.id
        FROM forge_outbox_events AS o
        WHERE o.state = 'published'
          AND o.published_at < now() - greatest(p_age_ms, 0) * interval '1 millisecond'
        LIMIT least(greatest(p_limit, 1), 10000)
        FOR UPDATE SKIP LOCKED
    )
    DELETE FROM forge_outbox_events AS o USING doomed WHERE o.id = doomed.id;
    GET DIAGNOSTICS deleted = ROW_COUNT;

    IF p_inbox_age_ms IS NOT NULL THEN
        WITH doomed AS MATERIALIZED (
            SELECT i.consumer, i.event_id
            FROM forge_inbox AS i
            WHERE i.processed_at < now() - greatest(p_inbox_age_ms, 0) * interval '1 millisecond'
            LIMIT least(greatest(p_limit, 1), 10000)
            FOR UPDATE SKIP LOCKED
        )
        DELETE FROM forge_inbox AS i
        USING doomed
        WHERE i.consumer = doomed.consumer AND i.event_id = doomed.event_id;
        GET DIAGNOSTICS inbox_deleted = ROW_COUNT;
    END IF;

    RETURN deleted + inbox_deleted;
END;
$$;

REVOKE ALL ON TABLE forge_outbox_events FROM PUBLIC;
REVOKE ALL ON TABLE forge_inbox FROM PUBLIC;

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
        WHERE p.proname LIKE 'forge\_outbox\_%'
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
        EXECUTE format('REVOKE ALL ON TABLE forge_outbox_events, forge_inbox FROM %I', grantee);
        -- Append columns only: delivery state always starts from its defaults.
        EXECUTE format('GRANT SELECT ON TABLE forge_outbox_events, forge_inbox TO %I', grantee);
        EXECUTE format(
            'GRANT INSERT (id, tenant_id, event_type, version, payload, occurred_at) '
            'ON TABLE forge_outbox_events TO %I',
            grantee
        );
        EXECUTE format('GRANT INSERT (consumer, event_id) ON TABLE forge_inbox TO %I', grantee);
        IF to_regclass('_forge_events_migrations') IS NOT NULL THEN
            EXECUTE format('REVOKE ALL ON TABLE _forge_events_migrations FROM %I', grantee);
        END IF;
    END LOOP;

    IF worker_role IS NOT NULL THEN
        FOR fn IN
            SELECT p.oid::regprocedure
            FROM pg_proc AS p
            WHERE p.proname LIKE 'forge\_outbox\_%'
              AND p.pronamespace = schema_name::regnamespace
        LOOP
            EXECUTE format('GRANT EXECUTE ON FUNCTION %s TO %I', fn, worker_role);
        END LOOP;
    END IF;
END
$$;
