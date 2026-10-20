-- Shared application cache (ADR 0028). Rows are addressed by the SHA-256 of
-- the cache key, so keys, which may name users or sessions, are never stored.
-- Times are unix microseconds (UTC).
CREATE TABLE forge_cache_entries (
    key_hash BLOB NOT NULL PRIMARY KEY CHECK (length(key_hash) = 32),
    value BLOB NOT NULL CHECK (length(value) <= 8388608),
    -- Time of the last write: eviction removes the oldest writes first.
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL
) STRICT;

-- Bounded pruning of expired entries, and size-bound eviction.
CREATE INDEX forge_cache_entries_expires_idx ON forge_cache_entries (expires_at);
CREATE INDEX forge_cache_entries_created_idx ON forge_cache_entries (created_at);
