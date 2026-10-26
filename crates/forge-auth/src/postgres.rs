//! [`PgLoginThrottle`]: the attempt throttle shared by every replica,
//! persisted in PostgreSQL (Forge-Go's `auth/postgres` `LoginThrottler`).
//!
//! [`MemoryLoginThrottle`](crate::MemoryLoginThrottle) counts per process:
//! with R replicas a guesser gets R times the budget, and every restart or
//! deploy resets it. This store keeps the same three counters (account,
//! source, account and source together; Forge-Go's scopes 1, 2 and 3) in the
//! `forge_login_throttle` table that a generated application's migration
//! `00000000000000000005_forge_login_throttle` creates. Keys are the SHA-256
//! digests [`ThrottleKey`] already holds, so the table stores no login,
//! address or tenant. All time comes from the database clock, so replicas
//! with skewed clocks agree.
//!
//! An attempt runs in one transaction: it creates any missing counter empty,
//! locks the counters in a fixed order (scope, then digest), refuses without
//! counting anything when one of them is exhausted (so an attacker who is
//! already blocked cannot burn the counters other people depend on), and
//! otherwise counts all of them. Two concurrent first attempts serialize on
//! the empty rows, so neither can exceed a limit. Each attempt also deletes
//! up to [`INLINE_PRUNE_BATCH`] expired counters while creating at most three,
//! so the table stays at the size of the live windows without a scheduled
//! job; [`PgLoginThrottle::prune_expired`] empties a table that stopped
//! receiving attempts.

use std::{fmt, time::Duration};

use async_trait::async_trait;
use sqlx::{PgPool, Row as _};

use crate::{
    LoginThrottle, ThrottleConfig, ThrottleConfigError, ThrottleDecision, ThrottleError,
    ThrottleKey,
};

/// Expired counters each attempt deletes: more than the three it can create.
pub const INLINE_PRUNE_BATCH: i64 = 8;

/// The longest window the store accepts (a year), far inside PostgreSQL's
/// timestamp range.
const MAX_WINDOW: Duration = Duration::from_secs(366 * 24 * 3600);

/// A [`LoginThrottle`] on PostgreSQL, shared by every process that uses the
/// same database. Safe for concurrent use; starts no task.
pub struct PgLoginThrottle {
    pool: PgPool,
    config: ThrottleConfig,
}

impl fmt::Debug for PgLoginThrottle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PgLoginThrottle")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl PgLoginThrottle {
    /// A throttle with `config`'s window and limits on `pool`, which must
    /// reach the `forge_login_throttle` table. `max_entries` bounds only the
    /// in-memory throttle: here expired rows are pruned instead.
    ///
    /// # Errors
    ///
    /// [`ThrottleConfigError`] for a zero window or limit, a window longer
    /// than a year, or a limit above `i32::MAX` (the counter column).
    pub fn new(pool: PgPool, config: ThrottleConfig) -> Result<Self, ThrottleConfigError> {
        config.validate()?;
        let limits = [config.pair_limit, config.account_limit, config.source_limit];
        if config.window > MAX_WINDOW || limits.iter().any(|limit| i32::try_from(*limit).is_err()) {
            return Err(ThrottleConfigError);
        }
        Ok(Self { pool, config })
    }

    /// Deletes at most `limit` expired counters and reports how many. Only
    /// needed to empty a table that stopped receiving attempts; replicas
    /// may call it concurrently (`SKIP LOCKED`).
    ///
    /// # Errors
    ///
    /// [`ThrottleError`] when the database fails.
    pub async fn prune_expired(&self, limit: u32) -> Result<u64, ThrottleError> {
        prune(&self.pool, i64::from(limit))
            .await
            .map_err(|_| ThrottleError)
    }

    fn window_micros(&self) -> i64 {
        // At most a year (checked in `new`), so it fits.
        i64::try_from(self.config.window.as_micros()).unwrap_or(i64::MAX)
    }

    /// Decides one attempt in `transaction`, which the caller commits only
    /// when it is allowed.
    async fn decide(
        &self,
        transaction: &mut sqlx::PgConnection,
        key: &ThrottleKey,
    ) -> Result<ThrottleDecision, sqlx::Error> {
        let counters = key.stored_counters(&self.config);
        let scopes: Vec<i16> = counters.iter().map(|counter| counter.scope).collect();
        let digests: Vec<Vec<u8>> = counters
            .iter()
            .map(|counter| counter.digest.to_vec())
            .collect();

        // One instant for the whole decision, from the database clock, in
        // microseconds since the epoch.
        let now: i64 =
            sqlx::query_scalar("SELECT (extract(epoch FROM clock_timestamp()) * 1000000)::bigint")
                .fetch_one(&mut *transaction)
                .await?;
        let resets_at = now.saturating_add(self.window_micros());

        // Every counter row exists, and is locked, before the next statement
        // reads them, so two first attempts for one key serialize here
        // instead of both seeing no counter and both being allowed. The
        // no-op update locks an existing row: DO NOTHING would not, and a
        // concurrent prune could delete an expired counter in between and
        // let this attempt through uncounted.
        sqlx::query(
            "INSERT INTO forge_login_throttle (scope, key_hash, attempt_count, reset_at) \
             SELECT scope, key_hash, 0, timestamptz 'epoch' + $3::bigint * interval '1 microsecond' \
             FROM unnest($1::smallint[], $2::bytea[]) AS attempt(scope, key_hash) \
             ORDER BY scope \
             ON CONFLICT (scope, key_hash) DO UPDATE \
               SET attempt_count = forge_login_throttle.attempt_count",
        )
        .bind(&scopes)
        .bind(&digests)
        .bind(resets_at)
        .execute(&mut *transaction)
        .await?;

        let rows = sqlx::query(
            "SELECT scope, attempt_count, \
               (extract(epoch FROM reset_at) * 1000000)::bigint AS reset_at \
             FROM forge_login_throttle \
             WHERE (scope, key_hash) IN (SELECT * FROM unnest($1::smallint[], $2::bytea[])) \
             ORDER BY scope, key_hash \
             FOR UPDATE",
        )
        .bind(&scopes)
        .bind(&digests)
        .fetch_all(&mut *transaction)
        .await?;

        let mut retry_after: i64 = 0;
        for row in &rows {
            let scope: i16 = row.try_get("scope")?;
            let count: i32 = row.try_get("attempt_count")?;
            let reset_at: i64 = row.try_get("reset_at")?;
            let Some(counter) = counters.iter().find(|counter| counter.scope == scope) else {
                continue;
            };
            let live = reset_at > now;
            if live && i64::from(count) >= i64::from(counter.limit) {
                retry_after = retry_after.max(reset_at - now);
            }
        }
        if retry_after > 0 {
            return Ok(ThrottleDecision::Throttled {
                retry_after: Duration::from_micros(u64::try_from(retry_after).unwrap_or(0)),
            });
        }

        // An expired counter starts a new window at 1; a live one counts on.
        // No count can pass its limit (checked above), and every limit fits
        // the column.
        sqlx::query(
            "UPDATE forge_login_throttle SET \
               attempt_count = CASE WHEN reset_at <= timestamptz 'epoch' + $3::bigint * interval '1 microsecond' \
                 THEN 1 ELSE attempt_count + 1 END, \
               reset_at = CASE WHEN reset_at <= timestamptz 'epoch' + $3::bigint * interval '1 microsecond' \
                 THEN timestamptz 'epoch' + $4::bigint * interval '1 microsecond' ELSE reset_at END \
             WHERE (scope, key_hash) IN (SELECT * FROM unnest($1::smallint[], $2::bytea[]))",
        )
        .bind(&scopes)
        .bind(&digests)
        .bind(now)
        .bind(resets_at)
        .execute(&mut *transaction)
        .await?;
        prune(&mut *transaction, INLINE_PRUNE_BATCH).await?;
        Ok(ThrottleDecision::Allowed)
    }
}

/// Deletes at most `limit` expired counters. `SKIP LOCKED` keeps concurrent
/// cleaners from blocking each other or the attempts holding a counter.
async fn prune<'e, E>(executor: E, limit: i64) -> Result<u64, sqlx::Error>
where
    E: sqlx::PgExecutor<'e>,
{
    sqlx::query(
        "WITH doomed AS ( \
           SELECT scope, key_hash FROM forge_login_throttle \
           WHERE reset_at <= clock_timestamp() \
           ORDER BY reset_at LIMIT $1 \
           FOR UPDATE SKIP LOCKED) \
         DELETE FROM forge_login_throttle target USING doomed \
         WHERE target.scope = doomed.scope AND target.key_hash = doomed.key_hash",
    )
    .bind(limit)
    .execute(executor)
    .await
    .map(|result| result.rows_affected())
}

#[async_trait]
impl LoginThrottle for PgLoginThrottle {
    async fn attempt(&self, key: &ThrottleKey) -> Result<ThrottleDecision, ThrottleError> {
        let mut transaction = self.pool.begin().await.map_err(|_| ThrottleError)?;
        let decision = self
            .decide(&mut transaction, key)
            .await
            .map_err(|_| ThrottleError)?;
        match decision {
            // Count nothing, and leave nothing behind: the empty rows of a
            // refused attempt roll back too. A failed rollback closes the
            // connection; the refusal stands either way.
            ThrottleDecision::Throttled { .. } => {
                let _ = transaction.rollback().await;
            }
            ThrottleDecision::Allowed => {
                transaction.commit().await.map_err(|_| ThrottleError)?;
            }
        }
        Ok(decision)
    }

    async fn success(&self, key: &ThrottleKey) -> Result<(), ThrottleError> {
        let (scopes, digests): (Vec<i16>, Vec<Vec<u8>>) = key
            .cleared_on_success()
            .into_iter()
            .map(|(scope, digest)| (scope, digest.to_vec()))
            .unzip();
        sqlx::query(
            "DELETE FROM forge_login_throttle \
             WHERE (scope, key_hash) IN (SELECT * FROM unnest($1::smallint[], $2::bytea[]))",
        )
        .bind(&scopes)
        .bind(&digests)
        .execute(&self.pool)
        .await
        .map(|_| ())
        .map_err(|_| ThrottleError)
    }
}
