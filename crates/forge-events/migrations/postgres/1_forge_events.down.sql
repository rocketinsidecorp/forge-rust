DROP FUNCTION IF EXISTS forge_outbox_prune(bigint, bigint, integer);
DROP FUNCTION IF EXISTS forge_outbox_dead(integer, uuid);
DROP FUNCTION IF EXISTS forge_outbox_retry(uuid);
DROP FUNCTION IF EXISTS forge_outbox_release(uuid, uuid);
DROP FUNCTION IF EXISTS forge_outbox_fail(uuid, uuid, text, bigint, integer, boolean);
DROP FUNCTION IF EXISTS forge_outbox_published(uuid, uuid);
DROP FUNCTION IF EXISTS forge_outbox_claim(integer, uuid, bigint, integer);

DROP TABLE IF EXISTS forge_inbox;
DROP TABLE IF EXISTS forge_outbox_events;
