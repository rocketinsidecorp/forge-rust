//! The `forge-proxy` command line: client commands against a live control
//! socket, exit codes, and `run` starting and stopping.

mod support;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use forge_proxy::cli::{EXIT_FAILED, EXIT_OK, EXIT_UNHEALTHY, EXIT_USAGE, main};
use forge_proxy::{ControlRequest, call, listen_control};
use support::{TEST_HOST, backend, front, get};
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
async fn client_commands() {
    let dir = tempfile::Builder::new()
        .prefix("fpm")
        .tempdir_in("/tmp")
        .expect("tempdir");
    let socket = dir.path().join("c.sock");
    let socket_text = socket.to_str().expect("utf-8").to_owned();
    let front = front(None).await;
    let listener = listen_control(&socket).expect("listen");
    let stop = CancellationToken::new();
    tokio::spawn({
        let (server, stop) = (front.server.clone(), stop.clone());
        async move { server.serve_control(listener, stop).await }
    });
    let app = backend("app", None).await;

    let (code, out, err) = run(&[
        "deploy",
        "-service",
        "app",
        "-target",
        &app.addr,
        "-hosts",
        TEST_HOST,
        "-health-interval",
        "1ms",
        "-healthy-count",
        "1",
        "-control",
        &socket_text,
    ])
    .await;
    assert_eq!(code, EXIT_OK, "{out} {err}");
    let response: serde_json::Value = serde_json::from_str(out.trim()).expect("json");
    assert_eq!(response["ok"], true);
    assert_eq!(response["deploy"]["target"], app.addr.as_str());
    assert_eq!(response["deploy"]["drained"], true);
    assert_eq!(
        run(&["healthcheck", "-control", &socket_text]).await.0,
        EXIT_OK
    );

    let (code, _, err) = run(&[
        "deploy",
        "-service",
        "app",
        "-target",
        "127.0.0.1:1",
        "-deploy-timeout",
        "20ms",
        "-health-interval",
        "1ms",
        "-control",
        &socket_text,
    ])
    .await;
    assert_eq!(code, EXIT_UNHEALTHY, "{err}");
    assert!(err.contains("not healthy"), "{err}");

    let (code, _, err) = run(&[
        "maintenance",
        "-control",
        &socket_text,
        "-service",
        "app",
        "on",
    ])
    .await;
    assert_eq!(code, EXIT_OK, "{err}");
    assert_eq!(get(front.addr, TEST_HOST, "/").await.0.as_u16(), 503);
    let (code, out, _) = run(&["status", "-control", &socket_text]).await;
    assert_eq!(code, EXIT_OK);
    assert!(out.contains("\"maintenance\":true"), "{out}");

    assert_eq!(run(&["bogus"]).await.0, EXIT_USAGE);
    assert_eq!(
        run(&[
            "maintenance",
            "-control",
            &socket_text,
            "-service",
            "app",
            "sideways"
        ])
        .await
        .0,
        EXIT_USAGE
    );
    assert_eq!(
        run(&["status", "-control", &socket_text, "extra"]).await.0,
        EXIT_USAGE
    );
    assert_eq!(run(&["deploy", "-nonsense"]).await.0, EXIT_USAGE);
    assert_eq!(
        run(&[
            "deploy",
            "-control",
            &socket_text,
            "-health-interval",
            "soon"
        ])
        .await
        .0,
        EXIT_USAGE
    );
    let missing = dir.path().join("missing.sock");
    let (code, _, err) = run(&["healthcheck", "-control", missing.to_str().expect("utf-8")]).await;
    assert_eq!(code, EXIT_FAILED, "{err}");

    let (code, _, err) = run(&[
        "remove",
        "-control",
        &socket_text,
        "-service",
        "app",
        "-drain-timeout",
        "1s",
    ])
    .await;
    assert_eq!(code, EXIT_OK, "{err}");
    assert_eq!(get(front.addr, TEST_HOST, "/").await.0.as_u16(), 404);
    stop.cancel();
}

#[tokio::test]
async fn run_serves_and_shuts_down() {
    let dir = tempfile::Builder::new()
        .prefix("fpr")
        .tempdir_in("/tmp")
        .expect("tempdir");
    let socket = dir.path().join("c.sock");
    let args: Vec<String> = [
        "run",
        "-http-addr",
        "127.0.0.1:0",
        "-https-addr",
        "127.0.0.1:0",
        "-control",
        socket.to_str().expect("utf-8"),
        "-state",
        dir.path().join("state.json").to_str().expect("utf-8"),
        "-acme-cache",
        dir.path().join("acme").to_str().expect("utf-8"),
        "-acme-directory",
        "http://127.0.0.1:1/directory",
        "-max-header-bytes",
        "4096",
        "-max-connections=0x10",
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
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if call(&socket, &status, Duration::from_secs(1))
            .await
            .is_ok_and(|response| response.ok)
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "proxy did not start: {}",
            stderr.text()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    shutdown.cancel();
    let code = tokio::time::timeout(Duration::from_secs(10), running)
        .await
        .expect("stops in time")
        .expect("join");
    assert_eq!(code, EXIT_OK, "{}", stderr.text());
    let logs = stdout.text();
    assert!(logs.contains("\"msg\":\"forge-proxy started\""), "{logs}");
    assert!(logs.contains("\"msg\":\"forge-proxy stopped\""), "{logs}");
    assert!(!socket.exists(), "control socket left behind");

    // Plain HTTP ACME directories are refused off loopback: fail closed.
    let (code, _, err) = run(&[
        "run",
        "-http-addr",
        "127.0.0.1:0",
        "-https-addr",
        "127.0.0.1:0",
        "-control",
        dir.path().join("d.sock").to_str().expect("utf-8"),
        "-state",
        "",
        "-acme-cache",
        dir.path().join("acme").to_str().expect("utf-8"),
        "-acme-directory",
        "http://acme.example.com/directory",
    ])
    .await;
    assert_eq!(code, EXIT_FAILED);
    assert!(
        err.contains("plain HTTP is accepted only on loopback"),
        "{err}"
    );
    assert_eq!(run(&["run", "extra"]).await.0, EXIT_FAILED);
}

#[tokio::test]
async fn run_refuses_a_non_positive_idle_timeout() {
    for value in ["0s", "-1s"] {
        let (code, _, err) = run(&["run", "-idle-timeout", value, "-state", ""]).await;
        assert_eq!(code, EXIT_FAILED, "{value}: {err}");
        assert!(err.contains("-idle-timeout must be positive"), "{err}");
    }
}

#[tokio::test]
async fn run_validates_the_header_and_connection_limits() {
    for (flag, value, message) in [
        (
            "-max-header-bytes",
            "4095",
            "-max-header-bytes must be between 4096 and 1048576",
        ),
        (
            "-max-header-bytes",
            "1048577",
            "-max-header-bytes must be between 4096 and 1048576",
        ),
        (
            "-max-header-bytes",
            "0",
            "-max-header-bytes must be between 4096 and 1048576",
        ),
        (
            "-max-header-bytes",
            "-1",
            "-max-header-bytes must be between 4096 and 1048576",
        ),
        (
            "-max-header-bytes",
            "64k",
            "invalid value \"64k\" for flag -max-header-bytes",
        ),
        (
            "-max-connections",
            "0",
            "-max-connections must be at least 1",
        ),
        (
            "-max-connections",
            "-5",
            "-max-connections must be at least 1",
        ),
        (
            "-max-connections",
            "many",
            "invalid value \"many\" for flag -max-connections",
        ),
    ] {
        let (code, _, err) = run(&["run", flag, value, "-state", ""]).await;
        assert_eq!(code, EXIT_FAILED, "{flag} {value}: {err}");
        assert!(err.contains(message), "{flag} {value}: {err}");
    }
    // The usage lists both flags with their ranges.
    let (_, _, usage) = run(&["run", "-h"]).await;
    assert!(usage.contains("-max-header-bytes value"), "{usage}");
    assert!(usage.contains("4096 to 1048576"), "{usage}");
    assert!(usage.contains("-max-connections value"), "{usage}");
}
