-- One row per live rate limit bucket (ADR 0027). The bucket name is the
-- opaque, ASCII key chosen by `forge_ratelimit::key` (a SHA-256 digest), never
-- a raw address or identifier. `expires_at` is unix microseconds (UTC).
CREATE TABLE forge_rate_limits (
    bucket TEXT NOT NULL PRIMARY KEY CHECK (length(bucket) BETWEEN 1 AND 128),
    hits INTEGER NOT NULL CHECK (hits >= 1),
    expires_at INTEGER NOT NULL
) STRICT;

-- Bounded pruning of expired buckets.
CREATE INDEX forge_rate_limits_expires_idx ON forge_rate_limits (expires_at);
