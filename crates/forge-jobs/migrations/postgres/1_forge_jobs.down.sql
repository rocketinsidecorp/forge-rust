DROP FUNCTION IF EXISTS forge_jobs_now_us();
DROP FUNCTION IF EXISTS forge_jobs_fire_tick(text, bigint, bigint, uuid, text, text, text, smallint, integer, text);
DROP FUNCTION IF EXISTS forge_jobs_prune(bigint, bigint, integer);
DROP FUNCTION IF EXISTS forge_jobs_stats();
DROP FUNCTION IF EXISTS forge_jobs_dead(integer, uuid);
DROP FUNCTION IF EXISTS forge_jobs_discard(uuid);
DROP FUNCTION IF EXISTS forge_jobs_retry(uuid);
DROP FUNCTION IF EXISTS forge_jobs_release(uuid, uuid);
DROP FUNCTION IF EXISTS forge_jobs_fail(uuid, uuid, text, bigint, boolean);
DROP FUNCTION IF EXISTS forge_jobs_complete(uuid, uuid);
DROP FUNCTION IF EXISTS forge_jobs_heartbeat(uuid[], uuid[], bigint);
DROP FUNCTION IF EXISTS forge_jobs_claim(text, text[], integer, uuid, bigint);

DROP TABLE IF EXISTS forge_job_schedules;
DROP TABLE IF EXISTS forge_jobs;
