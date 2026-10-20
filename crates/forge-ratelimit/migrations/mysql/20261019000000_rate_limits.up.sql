-- One row per live rate limit bucket (ADR 0027). The bucket name is the
-- opaque, ASCII key chosen by `forge_ratelimit::key` (a SHA-256 digest), never
-- a raw address or identifier.
CREATE TABLE forge_rate_limits (
    bucket VARCHAR(128) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,
    hits BIGINT NOT NULL,
    expires_at DATETIME(6) NOT NULL,
    PRIMARY KEY (bucket),
    -- Bounded pruning of expired buckets.
    KEY forge_rate_limits_expires_idx (expires_at),
    CONSTRAINT forge_rate_limits_hits CHECK (hits >= 1),
    CONSTRAINT forge_rate_limits_bucket CHECK (bucket <> '')
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4;
