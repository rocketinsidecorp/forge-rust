//! The control protocol and the routing rules with requests that are wrong
//! in each way a client can get them wrong, and the time parsers with text
//! that is not a time.

mod support;

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use forge_proxy::clock::Timestamp;
use forge_proxy::{ControlRequest, DeployRequest, call, listen_control};
use support::{backend, fast_request, front};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio_util::sync::CancellationToken;

async fn control() -> (
    tempfile::TempDir,
    std::path::PathBuf,
    support::Front,
    CancellationToken,
) {
    let dir = tempfile::Builder::new()
        .prefix("fpe")
        .tempdir_in("/tmp")
        .expect("tempdir");
    let socket = dir.path().join("c.sock");
    let front = front(None).await;
    let listener = listen_control(&socket).expect("listen");
    let stop = CancellationToken::new();
    tokio::spawn({
        let (server, stop) = (front.server.clone(), stop.clone());
        async move { server.serve_control(listener, stop).await }
    });
    (dir, socket, front, stop)
}

async fn send(socket: &std::path::Path, line: &[u8]) -> serde_json::Value {
    let mut stream = tokio::net::UnixStream::connect(socket)
        .await
        .expect("connect");
    stream.write_all(line).await.expect("write");
    let mut answer = Vec::new();
    stream.read_to_end(&mut answer).await.expect("read");
    serde_json::from_slice(&answer).expect("a JSON answer")
}

fn request(op: &str) -> ControlRequest {
    ControlRequest {
        op: op.to_owned(),
        ..ControlRequest::default()
    }
}

#[tokio::test]
async fn the_control_socket_answers_every_malformed_request_with_an_error() {
    let (_dir, socket, _front, stop) = control().await;
    for line in [&b"not json\n"[..], b"[1,2]\n", b"\n"] {
        let answer = send(&socket, line).await;
        assert_eq!(answer["ok"], false, "{line:?}");
        assert_eq!(answer["error"], "invalid control request", "{line:?}");
    }
    let unknown = call(&socket, &request("explode"), Duration::from_secs(5))
        .await
        .expect("call");
    assert!(!unknown.ok);
    assert_eq!(unknown.error, "unknown op");
    let missing = call(&socket, &request("deploy"), Duration::from_secs(5))
        .await
        .expect("call");
    assert_eq!(missing.error, "deploy request missing");

    for op in ["remove", "maintenance"] {
        let mut ask = request(op);
        ask.service = "ghost".to_owned();
        let answer = call(&socket, &ask, Duration::from_secs(5))
            .await
            .expect("call");
        assert!(!answer.ok, "{op}");
        assert!(
            answer.error.contains("unknown service \"ghost\""),
            "{}",
            answer.error
        );
    }
    let status = call(&socket, &request("status"), Duration::from_secs(5))
        .await
        .expect("call");
    assert!(status.ok && status.services.is_empty());
    stop.cancel();
}

#[tokio::test]
async fn a_second_service_cannot_take_the_unmatched_hosts_or_a_taken_host() {
    let (_dir, socket, front, stop) = control().await;
    let app = backend("app", None).await;
    let other = backend("other", None).await;
    let deploy = |service: &str, target: &str, hosts: &[&str]| {
        let mut request = fast_request(target);
        request.service = service.to_owned();
        request.hosts = hosts.iter().map(|host| (*host).to_owned()).collect();
        ControlRequest {
            op: "deploy".to_owned(),
            deploy: Some(request),
            ..ControlRequest::default()
        }
    };
    let first = call(
        &socket,
        &deploy("one", &app.addr, &[]),
        Duration::from_secs(10),
    )
    .await
    .expect("call");
    assert!(first.ok, "{}", first.error);
    let second = call(
        &socket,
        &deploy("two", &other.addr, &[]),
        Duration::from_secs(10),
    )
    .await
    .expect("call");
    assert!(!second.ok);
    assert_eq!(second.code, "invalid");
    assert!(
        second
            .error
            .contains("service one already receives requests for unmatched hosts"),
        "{}",
        second.error
    );
    // The service that already owns the unmatched hosts can be deployed again.
    let again = call(
        &socket,
        &deploy("one", &app.addr, &[]),
        Duration::from_secs(10),
    )
    .await
    .expect("call");
    assert!(again.ok, "{}", again.error);

    // The error type prints its message.
    let refused: DeployRequest = DeployRequest {
        service: String::new(),
        ..fast_request(&app.addr)
    };
    let error = front
        .server
        .deploy(refused)
        .await
        .expect_err("a service name is required");
    assert_eq!(error.to_string(), error.message);
    stop.cancel();
}

#[test]
fn text_that_is_not_a_time_is_not_parsed() {
    for text in [
        "",
        "2026-10-25",
        "2026-10-25X12:00:00Z",
        "2026-13-25T12:00:00Z",
        "2026-02-30T12:00:00Z",
        "2026-10-25T25:00:00Z",
        "2026-10-25T12:61:00Z",
        "2026-10-25T12:00:61Z",
        "2026-10-25T12:00:00",
        "2026-10-25T12:00:00+0100",
        "2026-10-25T12:00:00+25:00",
        "2026-10-25T12:00:00.Z",
        "2026-1-25T12:00:00Z",
        "abcd-10-25T12:00:00Z",
    ] {
        assert_eq!(Timestamp::parse_rfc3339(text), None, "{text:?}");
    }
    for text in [
        "",
        "2026-10-25 12:00:00",
        "2026-10-25 12:00:00 +0100",
        "2026-10-25 12:00:00 +0100 CET extra",
        "2026-10-25 12:00:00 01:00 CET",
        "2026-10-25 12:00:00 +01 CET",
        "2026-10-25 12:00:00 +0100 ",
        "2026-02-30 12:00:00 +0100 CET",
    ] {
        assert_eq!(Timestamp::parse_docker(text), None, "{text:?}");
    }
    // Months of every length, a leap day and a time zone ahead and behind.
    for text in [
        "2026-04-30T00:00:00Z",
        "2026-06-30T00:00:00Z",
        "2026-09-30T00:00:00Z",
        "2026-11-30T00:00:00Z",
        "2024-02-29T23:59:59.5-03:30",
    ] {
        assert!(Timestamp::parse_rfc3339(text).is_some(), "{text:?}");
    }
    assert_eq!(Timestamp::parse_rfc3339("2026-04-31T00:00:00Z"), None);
    assert_eq!(Timestamp::parse_rfc3339("2026-02-29T00:00:00Z"), None);
}

#[test]
fn times_before_the_epoch_have_whole_and_fractional_seconds() {
    let whole = Timestamp::from_system(UNIX_EPOCH - Duration::from_secs(90));
    assert_eq!(whole.rfc3339(), "1969-12-31T23:58:30Z");
    let fractional = Timestamp::from_system(UNIX_EPOCH - Duration::from_millis(1500));
    assert_eq!(fractional.rfc3339_nano(), "1969-12-31T23:59:58.5Z");
    assert!(Timestamp::from_system(SystemTime::now()).total_nanos() > whole.total_nanos());
    // The same instant prints in the forms Docker and people read.
    let parsed = Timestamp::parse_rfc3339("2026-10-25T12:30:45+02:00").expect("valid");
    assert_eq!(parsed.utc().rfc3339(), "2026-10-25T10:30:45Z");
    assert_eq!(parsed.docker(), "2026-10-25 12:30:45 +0200 +0200");
    assert!(parsed.human().contains("2026"));
}
