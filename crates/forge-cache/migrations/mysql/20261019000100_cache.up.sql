-- Shared application cache (ADR 0028). Rows are addressed by the SHA-256 of
-- the cache key, so keys, which may name users or sessions, are never stored.
CREATE TABLE forge_cache_entries (
    key_hash BINARY(32) NOT NULL,
    value MEDIUMBLOB NOT NULL,
    -- Time of the last write: eviction removes the oldest writes first.
    created_at DATETIME(6) NOT NULL,
    expires_at DATETIME(6) NOT NULL,
    PRIMARY KEY (key_hash),
    -- Bounded pruning of expired entries, and size-bound eviction.
    KEY forge_cache_entries_expires_idx (expires_at),
    KEY forge_cache_entries_created_idx (created_at),
    CONSTRAINT forge_cache_entries_size CHECK (LENGTH(value) <= 8388608)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4;
