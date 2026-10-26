//! The `forge-proxy` executable itself: its exit codes and its handling of
//! SIGTERM and SIGINT (what `docker stop` and Ctrl-C send), which shut the
//! proxy down gracefully and remove its control socket.

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use forge_proxy::{ControlRequest, call};

fn proxy() -> Command {
    Command::new(env!("CARGO_BIN_EXE_forge-proxy"))
}

fn start(dir: &Path) -> Child {
    proxy()
        .args([
            "run",
            "-http-addr",
            "127.0.0.1:0",
            "-https-addr",
            "",
            "-state",
            "",
        ])
        .arg("-control")
        .arg(dir.join("c.sock"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start forge-proxy")
}

/// Waits until the proxy answers on its control socket.
async fn ready(dir: &Path) {
    let status = ControlRequest {
        op: "status".to_owned(),
        ..ControlRequest::default()
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while !call(&dir.join("c.sock"), &status, Duration::from_secs(1))
        .await
        .is_ok_and(|response| response.ok)
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "forge-proxy did not start"
        );
        // real-clock: polls a real TCP listener or connection, which a fake clock cannot advance, until a deadline
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn finish(child: Child) -> (std::process::ExitStatus, String, String) {
    let output = tokio::time::timeout(
        Duration::from_secs(30),
        tokio::task::spawn_blocking(move || child.wait_with_output()),
    )
    .await
    .expect("forge-proxy exits in time")
    .expect("join")
    .expect("wait");
    (
        output.status,
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn signal(child: &Child, name: &str) {
    let sent = Command::new("kill")
        .arg(format!("-{name}"))
        .arg(child.id().to_string())
        .status()
        .expect("kill");
    assert!(sent.success(), "kill -{name}");
}

#[tokio::test]
async fn an_unknown_command_exits_with_the_usage_code() {
    let output = proxy().arg("bogus").output().expect("run");
    assert_eq!(output.status.code(), Some(2));
    assert!(!output.stderr.is_empty());
}

#[tokio::test]
async fn sigterm_and_sigint_stop_a_running_proxy_gracefully() {
    for name in ["TERM", "INT"] {
        let dir = tempfile::Builder::new()
            .prefix("fpb")
            .tempdir_in("/tmp")
            .expect("tempdir");
        let child = start(dir.path());
        ready(dir.path()).await;
        signal(&child, name);
        let (status, stdout, stderr) = finish(child).await;
        assert_eq!(status.code(), Some(0), "{name}: {stderr}");
        assert!(
            stdout.contains("\"msg\":\"forge-proxy started\""),
            "{stdout}"
        );
        assert!(
            stdout.contains("\"msg\":\"forge-proxy stopped\""),
            "{stdout}"
        );
        assert!(
            !dir.path().join("c.sock").exists(),
            "{name}: socket left behind"
        );
    }
}
