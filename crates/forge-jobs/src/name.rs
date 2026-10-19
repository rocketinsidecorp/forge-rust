//! Validation of queue, job and schedule names.
//!
//! Names are stable identifiers stored in the database and used in logs:
//! lowercase ASCII letters, digits and `_ . : -`, starting and ending with a
//! letter or digit. The database enforces the same grammar with `CHECK`
//! constraints.

use crate::JobsError;

/// Longest queue name, in bytes.
pub const MAX_QUEUE_NAME: usize = 64;
/// Longest job or schedule name, in bytes.
pub const MAX_JOB_NAME: usize = 128;

/// Returns whether `value` is a valid name of at most `max` bytes.
#[must_use]
pub(crate) fn is_valid(value: &str, max: usize) -> bool {
    let bytes = value.as_bytes();
    let edge = |byte: &u8| byte.is_ascii_lowercase() || byte.is_ascii_digit();
    !bytes.is_empty()
        && bytes.len() <= max
        && bytes.first().is_some_and(edge)
        && bytes.last().is_some_and(edge)
        && bytes
            .iter()
            .all(|byte| edge(byte) || matches!(byte, b'_' | b'.' | b':' | b'-'))
}

pub(crate) fn queue(value: &str) -> Result<(), JobsError> {
    check(value, MAX_QUEUE_NAME, "queue")
}

pub(crate) fn job(value: &str) -> Result<(), JobsError> {
    check(value, MAX_JOB_NAME, "job")
}

pub(crate) fn schedule(value: &str) -> Result<(), JobsError> {
    check(value, MAX_JOB_NAME, "schedule")
}

fn check(value: &str, max: usize, kind: &'static str) -> Result<(), JobsError> {
    if is_valid(value, max) {
        Ok(())
    } else {
        Err(JobsError::InvalidName {
            kind,
            name: value.chars().take(max).collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_follow_the_database_grammar() {
        for valid in [
            "default",
            "a",
            "mail.send",
            "billing:invoice-paid",
            "q_1",
            "0",
        ] {
            assert!(is_valid(valid, MAX_QUEUE_NAME), "{valid}");
        }
        for invalid in [
            "", "Mail", "-a", "a-", ".a", "a b", "a/b", "é", "a\n", "_a", "a_",
        ] {
            assert!(!is_valid(invalid, MAX_QUEUE_NAME), "{invalid:?}");
        }
        assert!(is_valid(&"a".repeat(64), MAX_QUEUE_NAME));
        assert!(!is_valid(&"a".repeat(65), MAX_QUEUE_NAME));
        assert!(is_valid(&"a".repeat(128), MAX_JOB_NAME));
    }

    #[test]
    fn invalid_names_are_reported_with_their_kind() {
        let error = job("Bad Name").expect_err("invalid");
        assert_eq!(error.to_string(), "invalid job name \"Bad Name\"");
        assert!(queue("default").is_ok());
        assert!(schedule("nightly.cleanup").is_ok());
    }
}
