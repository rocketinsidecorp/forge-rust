-- One row per live rate limit bucket (ADR 0027). The bucket name is the
-- opaque, ASCII key chosen by `forge_ratelimit::key` (a SHA-256 digest), never
-- a raw address or identifier.
CREATE TABLE forge_rate_limits (
    bucket varchar(128) PRIMARY KEY CHECK (bucket ~ '^[\x21-\x7e]{1,128}$'),
    hits bigint NOT NULL CHECK (hits >= 1),
    expires_at timestamptz NOT NULL
);

-- Bounded pruning of expired buckets.
CREATE INDEX forge_rate_limits_expires_idx ON forge_rate_limits (expires_at);
