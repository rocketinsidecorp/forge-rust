//! Every output the subscriber can be asked for, installed or not.

use super::*;

#[test]
fn every_format_and_stream_builds_whether_or_not_a_subscriber_exists() {
    for format in [LogFormat::Json, LogFormat::Text] {
        let config = LogConfig {
            filter: "info,forge=debug".to_owned(),
            format,
        };
        init(&config, None).expect("standard output");
        init_stderr(&config).expect("standard error");
    }
}

#[test]
fn a_filter_is_checked_before_any_output_is_chosen() {
    for format in [LogFormat::Json, LogFormat::Text] {
        let config = LogConfig {
            filter: "forge=loud".to_owned(),
            format,
        };
        let error = init_stderr(&config).expect_err("invalid level");
        let message = error.to_string();
        assert!(
            message.contains("FORGE_LOG") && message.contains("forge=loud"),
            "{message}"
        );
    }
}
