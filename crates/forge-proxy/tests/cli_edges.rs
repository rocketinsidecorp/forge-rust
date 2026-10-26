//! The command line's corners: running without a command word, Go-style
//! listen addresses, help, and the number, boolean and duration syntax of
//! the flags, which follows Go's so either framework's `forge deploy`
//! drives either proxy.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use forge_proxy::cli::{EXIT_FAILED, EXIT_OK, EXIT_USAGE, main};
use forge_proxy::log::Logger;
use forge_proxy::{ControlRequest, GoDuration, call};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Default)]
struct Output(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Output {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("lock").extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Output {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().expect("lock")).into_owned()
    }
}

async fn run(args: &[&str]) -> (i32, String, String) {
    let (stdout, stderr) = (Output::default(), Output::default());
    let args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
    let code = main(
        &args,
        Box::new(stdout.clone()),
        Box::new(stderr.clone()),
        CancellationToken::new(),
    )
    .await;
    (code, stdout.text(), stderr.text())
}

#[tokio::test]
async fn running_without_a_command_word_serves_on_go_style_addresses() {
    let dir = tempfile::Builder::new()
        .prefix("fpc")
        .tempdir_in("/tmp")
        .expect("tempdir");
    let socket = dir.path().join("c.sock");
    let args: Vec<String> = [
        "-http-addr",
        ":0",
        "-https-addr",
        "",
        "-control",
        socket.to_str().expect("utf-8"),
        "-state",
        "",
    ]
    .iter()
    .map(|arg| (*arg).to_owned())
    .collect();
    let (stdout, stderr) = (Output::default(), Output::default());
    let shutdown = CancellationToken::new();
    let running = tokio::spawn({
        let (stdout, stderr, shutdown) = (stdout.clone(), stderr.clone(), shutdown.clone());
        async move { main(&args, Box::new(stdout), Box::new(stderr), shutdown).await }
    });
    let status = ControlRequest {
        op: "status".to_owned(),
        ..ControlRequest::default()
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !call(&socket, &status, Duration::from_secs(1))
        .await
        .is_ok_and(|response| response.ok)
    {
        assert!(tokio::time::Instant::now() < deadline, "{}", stderr.text());
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    shutdown.cancel();
    let code = tokio::time::timeout(Duration::from_secs(10), running)
        .await
        .expect("stops")
        .expect("join");
    assert_eq!(code, EXIT_OK, "{}", stderr.text());
    assert!(
        stdout.text().contains("forge-proxy started"),
        "{}",
        stdout.text()
    );
}

#[tokio::test]
async fn help_and_bad_flags_print_the_usage_of_the_command() {
    let (code, _, err) = run(&["run", "-h"]).await;
    assert_eq!(code, EXIT_FAILED);
    assert!(
        err.contains("help requested") && err.contains("-http-addr"),
        "{err}"
    );
    let (code, _, err) = run(&["run", "-no-such-flag"]).await;
    assert_eq!(code, EXIT_FAILED);
    assert!(
        err.contains("flag provided but not defined: -no-such-flag"),
        "{err}"
    );
    let (code, _, err) = run(&["run", "--http-addr"]).await;
    assert_eq!(code, EXIT_FAILED);
    assert!(err.contains("flag needs an argument"), "{err}");
}

#[tokio::test]
async fn numbers_and_booleans_follow_go_syntax() {
    let dir = tempfile::tempdir().expect("tempdir");
    let missing = dir.path().join("none.sock");
    let missing = missing.to_str().expect("utf-8");
    let deploy = |extra: &[&'static str]| {
        let mut args = vec![
            "deploy",
            "-service",
            "app",
            "-target",
            "127.0.0.1:1",
            "-control",
            missing,
        ];
        args.extend_from_slice(extra);
        args
    };
    // Valid values get as far as the control socket, which is not there.
    for extra in [
        &["-healthy-count", "+3"][..],
        &["-healthy-count", "0x10"],
        &["-healthy-count", "0o7"],
        &["-healthy-count", "0b11"],
        &["-healthy-count", "010"],
        &["-healthy-count", "1_000"],
        &["-tls=false"],
        &["-tls=T"],
        &["-tls", "-hosts", "a.example.com"],
        &["--health-interval=1s"],
    ] {
        let (code, _, err) = run(&deploy(extra)).await;
        assert_eq!(code, EXIT_FAILED, "{extra:?}: {err}");
    }
    // Invalid ones are usage errors.
    for extra in [
        &["-healthy-count", "_1"][..],
        &["-healthy-count", "1__0"],
        &["-healthy-count", "0x"],
        &["-healthy-count", "9999999999999999999999"],
        &["-tls=maybe"],
        &["---tls"],
        &["-=x"],
    ] {
        let (code, _, err) = run(&deploy(extra)).await;
        assert_eq!(code, EXIT_USAGE, "{extra:?}: {err}");
    }
}

#[test]
fn duration_errors_quote_the_text_like_go_does() {
    let error = GoDuration::parse("1\"x\\\n\t\r\u{1}")
        .unwrap_err()
        .to_string();
    assert!(error.contains(r#""1\"x\\\n\t\r\x01""#), "{error}");
    assert!(GoDuration::parse("9223372037s").is_err());
    assert_eq!(
        GoDuration::parse("1.5s").expect("valid").to_std(),
        Duration::from_millis(1500)
    );
    assert!(GoDuration::parse(".s").is_err());
    // The logger prints its kind and writes error lines.
    assert_eq!(format!("{:?}", Logger::discard()), "Logger");
    Logger::discard().error("failed", &[]);
}
