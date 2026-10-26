//! Settings, their limits and the builder, without a database.

use super::*;

#[test]
fn defaults_are_the_documented_ones() {
    let settings = DispatcherSettings::default();
    assert_eq!(settings.batch, 10);
    assert_eq!(settings.poll_interval, Duration::from_secs(1));
    assert_eq!(settings.publish_timeout, Duration::from_secs(10));
    assert_eq!(settings.max_attempts, 20);
    assert_eq!(settings.backoff, Backoff::DEFAULT);
    // Ten 10 s publications plus 30 s.
    assert_eq!(settings.lease(), Duration::from_secs(130));
}

#[test]
fn every_setter_changes_only_its_setting() {
    let backoff = Backoff::fixed(Duration::from_secs(3));
    let settings = DispatcherSettings::default()
        .batch(7)
        .poll_interval(Duration::from_millis(250))
        .publish_timeout(Duration::from_secs(2))
        .max_attempts(4)
        .backoff(backoff);
    assert_eq!(
        settings,
        DispatcherSettings {
            batch: 7,
            poll_interval: Duration::from_millis(250),
            publish_timeout: Duration::from_secs(2),
            max_attempts: 4,
            backoff,
            ..DispatcherSettings::default()
        }
    );
    assert!(settings.validate().is_ok());
}

#[test]
fn each_limit_has_its_own_message() {
    let defaults = DispatcherSettings::default();
    for (settings, said) in [
        (defaults.batch(0), "batch must be between 1 and 1000"),
        (defaults.batch(1001), "batch must be between 1 and 1000"),
        (
            defaults.poll_interval(Duration::from_millis(9)),
            "poll_interval must be at least 10 ms",
        ),
        (
            defaults.publish_timeout(Duration::from_millis(9)),
            "publish_timeout must be between 10 ms and 10 minutes",
        ),
        (
            defaults.publish_timeout(Duration::from_secs(601)),
            "publish_timeout must be between 10 ms and 10 minutes",
        ),
        (defaults.max_attempts(0), "max_attempts must be at least 1"),
    ] {
        let error = settings.validate().expect_err(said);
        assert_eq!(error.to_string(), format!("invalid events setting: {said}"));
    }
    // The limits themselves are allowed.
    for settings in [
        defaults.batch(1),
        defaults.batch(1000),
        defaults.poll_interval(Duration::from_millis(10)),
        defaults.publish_timeout(Duration::from_millis(10)),
        defaults.publish_timeout(Duration::from_secs(600)),
        defaults.max_attempts(1),
    ] {
        assert!(settings.validate().is_ok(), "{settings:?}");
    }
}

#[test]
fn the_lease_covers_a_whole_batch_and_is_capped_at_a_day() {
    let settings = DispatcherSettings::default()
        .batch(1000)
        .publish_timeout(Duration::from_secs(600));
    assert_eq!(settings.lease(), Duration::from_secs(86_400));
    let small = DispatcherSettings::default()
        .batch(1)
        .publish_timeout(Duration::from_millis(10));
    assert_eq!(small.lease(), Duration::from_millis(30_010));
}

#[test]
fn control_characters_are_replaced_and_multibyte_text_is_never_split() {
    assert_eq!(sanitize("a\tb\r\nc\u{0}d"), "a b  c d");
    assert_eq!(sanitize("plain ünïcode"), "plain ünïcode");
    let long = "é".repeat(600);
    let cut = sanitize(&long);
    assert_eq!(cut.len(), 1024);
    assert!(cut.chars().all(|character| character == 'é'));
    assert_eq!(sanitize(""), "");
}
