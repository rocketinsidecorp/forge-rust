-- Forge job announcements (ADR 0046): a due job announces itself on the
-- `forge_jobs` channel (payload: its queue) when it is inserted by a session
-- whose `forge.jobs_notify` setting is on. It covers every enqueue path
-- (`enqueue`, `enqueue_for_tenant`, `PgJobStore::enqueue` and
-- `forge_jobs_fire_tick`). The notification is delivered when the inserting
-- transaction commits, never after a rollback, once per queue and
-- transaction (PostgreSQL merges identical notifications). A duplicate
-- skipped by `ON CONFLICT DO NOTHING` inserts no row and announces nothing;
-- a job due later is found by polling.
--
-- Opt-in, like Forge-Go's `queuepostgres.WithNotify()`: PostgreSQL serializes
-- the commits of notifying transactions, so sessions without the setting pay
-- nothing.

CREATE FUNCTION forge_jobs_announce()
RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    PERFORM pg_notify('forge_jobs', NEW.queue);
    RETURN NULL;
END;
$$;

-- A trigger function cannot be called directly; firing it needs no grant.
REVOKE ALL ON FUNCTION forge_jobs_announce() FROM PUBLIC;

CREATE TRIGGER forge_jobs_announce
AFTER INSERT ON forge_jobs
FOR EACH ROW
WHEN (
    NEW.state = 'queued'
    AND NEW.run_at <= pg_catalog.clock_timestamp()
    AND pg_catalog.lower(
        coalesce(pg_catalog.current_setting('forge.jobs_notify', true), '')
    ) IN ('on', 'true', 'yes', '1')
)
EXECUTE FUNCTION forge_jobs_announce();
