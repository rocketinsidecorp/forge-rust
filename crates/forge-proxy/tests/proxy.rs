//! forge-proxy over real sockets: switches under load, health gating,
//! draining, routing, forwarding headers, body limits, upgrades, the
//! control socket and persisted state (ports Forge-Go's proxy tests).

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use bytes::Bytes;
use forge_proxy::log::Logger;
use forge_proxy::{ControlRequest, DeployRequest, GoDuration, Server, call, listen_control};
use http::Request;
use http_body_util::{BodyExt as _, Full};
use support::{
    Handler, TEST_HOST, backend, fast_request, front, front_with, front_with_limits, get, limits,
    post_chunked, send, status, text,
};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio_util::sync::CancellationToken;

fn handler<F, Fut>(f: F) -> Option<Handler>
where
    F: Fn(Request<hyper::body::Incoming>) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = http::Response<Full<Bytes>>> + Send + 'static,
{
    Some(Arc::new(move |request| Box::pin(f(request))))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn switch_under_concurrent_load_drops_no_requests() {
    let counter = Arc::new(AtomicU64::new(0));
    let slow = |name: &'static str| {
        let counter = Arc::clone(&counter);
        handler(move |request: Request<hyper::body::Incoming>| {
            let n = counter.fetch_add(1, Ordering::SeqCst);
            async move {
                let _ = request.into_body().collect().await;
                // 0-3ms, spread deterministically.
                tokio::time::sleep(Duration::from_micros(n.wrapping_mul(7919) % 3000)).await;
                text(name)
            }
        })
    };
    let a = backend("a", slow("a")).await;
    let b = backend("b", slow("b")).await;
    let front = front(None).await;
    front
        .server
        .deploy(fast_request(&a.addr))
        .await
        .expect("first deploy");
    let total = Arc::new(AtomicU64::new(0));
    let failures = Arc::new(AtomicU64::new(0));
    let stop = CancellationToken::new();
    let mut workers = Vec::new();
    for _ in 0..16 {
        let (total, failures, stop, addr) = (
            Arc::clone(&total),
            Arc::clone(&failures),
            stop.clone(),
            front.addr,
        );
        workers.push(tokio::spawn(async move {
            while !stop.is_cancelled() {
                total.fetch_add(1, Ordering::SeqCst);
                let request = Request::post("/work")
                    .header("host", TEST_HOST)
                    .body(Full::new(Bytes::from_static(b"payload")))
                    .expect("request");
                let (code, _, body) = send(addr, request).await;
                if code != 200 || (body != "a" && body != "b") {
                    failures.fetch_add(1, Ordering::SeqCst);
                    eprintln!("failure: {code} {body:?}");
                }
            }
        }));
    }
    for i in 0..20 {
        let next = if i % 2 == 1 { &a } else { &b };
        let result = front
            .server
            .deploy(fast_request(&next.addr))
            .await
            .expect("deploy");
        assert!(
            result.drained && result.in_flight == 0,
            "deploy {i} not drained: {result:?}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    stop.cancel();
    for worker in workers {
        worker.await.expect("worker");
    }
    assert_eq!(
        failures.load(Ordering::SeqCst),
        0,
        "requests failed during switches (of {})",
        total.load(Ordering::SeqCst)
    );
    assert!(
        total.load(Ordering::SeqCst) >= 100
            && a.hits.load(Ordering::SeqCst) > 0
            && b.hits.load(Ordering::SeqCst) > 0,
        "load too small: total={} a={} b={}",
        total.load(Ordering::SeqCst),
        a.hits.load(Ordering::SeqCst),
        b.hits.load(Ordering::SeqCst)
    );
}

#[tokio::test]
async fn unhealthy_target_is_rejected_and_previous_keeps_serving() {
    let old = backend("old", None).await;
    let bad = backend("bad", None).await;
    bad.healthy.store(false, Ordering::SeqCst);
    let front = front(None).await;
    front
        .server
        .deploy(fast_request(&old.addr))
        .await
        .expect("deploy");
    let mut request = fast_request(&bad.addr);
    request.deploy_timeout = GoDuration::from_millis(100);
    let error = front.server.deploy(request).await.expect_err("unhealthy");
    assert!(
        error.unhealthy && error.message.contains("status 503"),
        "{error:?}"
    );
    assert!(
        error
            .message
            .starts_with("target failed its health check: "),
        "{error:?}"
    );
    let (code, _, body) = get(front.addr, TEST_HOST, "/").await;
    assert_eq!((code.as_u16(), body.as_str()), (200, "old"));
    assert_eq!(
        bad.hits.load(Ordering::SeqCst),
        0,
        "unhealthy target got traffic"
    );
    let mut unreachable = fast_request("127.0.0.1:1");
    unreachable.deploy_timeout = GoDuration::from_millis(50);
    let error = front
        .server
        .deploy(unreachable)
        .await
        .expect_err("unreachable");
    assert!(error.unhealthy, "{error:?}");
}

#[tokio::test]
async fn health_gate_requires_consecutive_passes() {
    let probes = Arc::new(AtomicU64::new(0));
    // pass, fail, pass, fail ... never two in a row
    let counter = Arc::clone(&probes);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let flapping = listener.local_addr().expect("addr").to_string();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let counter = Arc::clone(&counter);
            tokio::spawn(async move {
                let service = hyper::service::service_fn(move |_request| {
                    let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
                    async move {
                        Ok::<_, std::convert::Infallible>(status(if n.is_multiple_of(2) {
                            500
                        } else {
                            200
                        }))
                    }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(hyper_util::rt::TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    let front = front(None).await;
    let mut request = fast_request(&flapping);
    request.deploy_timeout = GoDuration::from_millis(150);
    let error = front
        .server
        .deploy(request)
        .await
        .expect_err("flapping accepted");
    assert!(error.unhealthy);
    assert!(
        probes.load(Ordering::SeqCst) >= 4,
        "only {} probes",
        probes.load(Ordering::SeqCst)
    );
    let stable = backend("stable", None).await;
    let mut request = fast_request(&stable.addr);
    request.healthy_count = 3;
    let result = front.server.deploy(request).await.expect("stable");
    assert_eq!(result.health_checks, 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deploy_drains_in_flight_requests_before_returning() {
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let (e, r) = (Arc::clone(&entered), Arc::clone(&release));
    let old = backend(
        "old",
        handler(move |request: Request<hyper::body::Incoming>| {
            let (e, r) = (Arc::clone(&e), Arc::clone(&r));
            async move {
                if request.uri().path() != "/slow" {
                    return text("old");
                }
                e.notify_one();
                r.notified().await;
                text("old-finished")
            }
        }),
    )
    .await;
    let next = backend("new", None).await;
    let front = front(None).await;
    front
        .server
        .deploy(fast_request(&old.addr))
        .await
        .expect("deploy");
    let addr = front.addr;
    let slow = tokio::spawn(async move { get(addr, TEST_HOST, "/slow").await.2 });
    entered.notified().await;
    let server = front.server.clone();
    let next_addr = next.addr.clone();
    let mut deployed = tokio::spawn(async move { server.deploy(fast_request(&next_addr)).await });
    // New requests reach the new target while the old one drains.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        if get(front.addr, TEST_HOST, "/").await.2 == "new" {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "traffic never switched"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut deployed)
            .await
            .is_err(),
        "deploy returned before the old request finished"
    );
    release.notify_one();
    let result = deployed.await.expect("join").expect("deploy");
    assert!(result.drained, "{result:?}");
    assert_eq!(result.previous, old.addr);
    assert_eq!(slow.await.expect("slow"), "old-finished");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn drain_timeout_reports_remaining_requests() {
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let (e, r) = (Arc::clone(&entered), Arc::clone(&release));
    let old = backend(
        "old",
        handler(move |_request| {
            let (e, r) = (Arc::clone(&e), Arc::clone(&r));
            async move {
                e.notify_one();
                r.notified().await;
                text("late")
            }
        }),
    )
    .await;
    let next = backend("new", None).await;
    let front = front(None).await;
    front
        .server
        .deploy(fast_request(&old.addr))
        .await
        .expect("deploy");
    let addr = front.addr;
    let stuck = tokio::spawn(async move { get(addr, TEST_HOST, "/stuck").await });
    entered.notified().await;
    let mut request = fast_request(&next.addr);
    request.drain_timeout = GoDuration::from_millis(30);
    let result = front.server.deploy(request).await.expect("deploy");
    assert!(!result.drained && result.in_flight == 1, "{result:?}");
    release.notify_one();
    stuck.await.expect("stuck request");
}

#[tokio::test]
async fn maintenance_mode_serves_a_static_page() {
    let app = backend("app", None).await;
    let front = front(None).await;
    front
        .server
        .deploy(fast_request(&app.addr))
        .await
        .expect("deploy");
    front.server.set_maintenance("app", true).await.expect("on");
    let (code, headers, body) = get(front.addr, TEST_HOST, "/").await;
    assert_eq!(code.as_u16(), 503);
    assert_eq!(headers["retry-after"], "60");
    assert!(body.contains("maintenance"));
    assert_eq!(app.hits.load(Ordering::SeqCst), 0);
    front
        .server
        .set_maintenance("app", false)
        .await
        .expect("off");
    assert_eq!(get(front.addr, TEST_HOST, "/").await.0.as_u16(), 200);
    assert!(front.server.set_maintenance("missing", true).await.is_err());
}

#[tokio::test]
async fn forwarded_headers_are_set_and_spoofed_ones_dropped() {
    let (seen_tx, seen_rx) = std::sync::mpsc::channel();
    let seen_tx = Arc::new(std::sync::Mutex::new(seen_tx));
    let app = backend(
        "app",
        handler(move |request: Request<hyper::body::Incoming>| {
            let headers = request.headers().clone();
            let _ = seen_tx.lock().expect("lock").send(headers);
            async { text("ok") }
        }),
    )
    .await;
    let front = front(None).await;
    front
        .server
        .deploy(fast_request(&app.addr))
        .await
        .expect("deploy");
    let request = Request::get("/x")
        .header("host", TEST_HOST)
        .header("x-forwarded-for", "203.0.113.66")
        .header("x-forwarded-proto", "https")
        .header("x-real-ip", "203.0.113.66")
        .header("forwarded", "for=203.0.113.66;proto=https")
        .body(Full::new(Bytes::new()))
        .expect("request");
    let (code, _, _) = send(front.addr, request).await;
    assert_eq!(code.as_u16(), 200);
    let headers = seen_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("headers");
    assert_eq!(
        headers["x-forwarded-for"], "127.0.0.1",
        "only the connecting address"
    );
    assert_eq!(headers.get_all("x-forwarded-for").iter().count(), 1);
    assert_eq!(headers["x-forwarded-proto"], "http");
    assert_eq!(headers["x-forwarded-host"], TEST_HOST);
    assert_eq!(headers["host"], TEST_HOST, "original Host kept");
    assert!(headers.get("forwarded").is_none());
    assert!(headers.get("x-real-ip").is_none());
}

#[tokio::test]
async fn routing_by_host() {
    let one = backend("one", None).await;
    let two = backend("two", None).await;
    let front = front(None).await;
    front
        .server
        .deploy(fast_request(&one.addr))
        .await
        .expect("one");
    let mut second = fast_request(&two.addr);
    second.service = "other".to_owned();
    second.hosts = vec!["other.example.com".to_owned()];
    front.server.deploy(second).await.expect("other");
    assert_eq!(get(front.addr, "APP.example.com:80", "/").await.2, "one");
    assert_eq!(get(front.addr, "other.example.com", "/").await.2, "two");
    assert_eq!(
        get(front.addr, "unknown.example.com", "/").await.0.as_u16(),
        404
    );
    let mut conflict = fast_request(&two.addr);
    conflict.service = "third".to_owned();
    let error = front.server.deploy(conflict).await.expect_err("conflict");
    assert!(error.message.contains("already routed"), "{error:?}");
    front
        .server
        .remove("other", Duration::from_secs(1))
        .await
        .expect("remove");
    assert_eq!(
        get(front.addr, "other.example.com", "/").await.0.as_u16(),
        404
    );
}

fn counting_backend() -> Option<Handler> {
    handler(|request: Request<hyper::body::Incoming>| async move {
        let length = request
            .headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .map_or(-1, |v| v.parse::<i64>().unwrap_or(-1));
        match request.into_body().collect().await {
            Ok(body) => text(format!("{length}/{}", body.to_bytes().len())),
            Err(_) => status(499),
        }
    })
}

#[tokio::test]
async fn request_body_limits() {
    let app = backend("app", counting_backend()).await;
    let front = front(None).await;
    let mut request = fast_request(&app.addr);
    request.max_body_bytes = 1024;
    request.buffer_bytes = 512;
    front.server.deploy(request).await.expect("deploy");
    let post = |size: usize| {
        Request::post("/upload")
            .header("host", TEST_HOST)
            .body(Full::new(Bytes::from(vec![b'x'; size])))
            .expect("request")
    };
    let (code, _, body) = send(front.addr, post(100)).await;
    assert_eq!(
        (code.as_u16(), &body[..]),
        (200, &b"100/100"[..]),
        "buffered small body"
    );
    let (code, _, body) = send(front.addr, post(900)).await;
    assert_eq!(
        (code.as_u16(), &body[..]),
        (200, &b"900/900"[..]),
        "streamed body under limit"
    );
    assert_eq!(
        send(front.addr, post(2048)).await.0.as_u16(),
        413,
        "declared oversize body"
    );
    let (code, _) =
        post_chunked(front.addr, vec![vec![b'x'; 2048]], Duration::from_millis(1)).await;
    assert_eq!(code, 413, "chunked oversize body");
}

#[tokio::test]
async fn slow_chunked_body_is_cut_off_at_the_deadline() {
    let app = backend("app", counting_backend()).await;
    let front = front(None).await;
    front.server.set_body_timeout(Duration::from_millis(200));
    let mut request = fast_request(&app.addr);
    request.buffer_bytes = 1024;
    front.server.deploy(request).await.expect("deploy");
    // 40 bytes, one every 50ms: well under the buffer, but 2s in total.
    let (code, elapsed) =
        post_chunked(front.addr, vec![vec![b'x']; 40], Duration::from_millis(50)).await;
    assert_eq!(code, 408, "after {elapsed:?}");
    assert!(elapsed < Duration::from_millis(1500), "{elapsed:?}");
    assert_eq!(
        app.hits.load(Ordering::SeqCst),
        0,
        "slow body reached the application"
    );
}

#[tokio::test]
async fn chunked_bodies_are_buffered_or_streamed() {
    let app = backend("app", counting_backend()).await;
    let front = front(None).await;
    front.server.set_body_timeout(Duration::from_millis(200));
    let mut request = fast_request(&app.addr);
    request.buffer_bytes = 1024;
    front.server.deploy(request).await.expect("deploy");
    let small = vec![b"hello ".to_vec(), b"world".to_vec()];
    assert_eq!(
        post_chunked(front.addr, small, Duration::from_millis(1))
            .await
            .0,
        200
    );
    let big = vec![vec![b'a'; 1000], vec![b'b'; 1000], vec![b'c'; 1000]];
    assert_eq!(
        post_chunked(front.addr, big, Duration::from_millis(50))
            .await
            .0,
        200
    );
    let stalled = vec![vec![b'd'; 2000], b"late".to_vec()];
    let (code, elapsed) = post_chunked(front.addr, stalled, Duration::from_secs(2)).await;
    assert_eq!(code, 408, "stalled streamed body after {elapsed:?}");
    assert!(elapsed < Duration::from_millis(1500), "{elapsed:?}");
}

#[tokio::test]
async fn buffered_bodies_arrive_with_a_length() {
    let app = backend("app", counting_backend()).await;
    let front = front(None).await;
    let mut request = fast_request(&app.addr);
    request.buffer_bytes = 1024;
    front.server.deploy(request).await.expect("deploy");
    // A small chunked body reaches the application with Content-Length; a
    // large one is streamed without.
    let listener_reply = |chunks: Vec<Vec<u8>>| async move {
        let mut stream = tokio::net::TcpStream::connect(front.addr)
            .await
            .expect("connect");
        let mut head = format!(
            "POST /upload HTTP/1.1\r\nHost: {TEST_HOST}\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
        )
        .into_bytes();
        for chunk in chunks {
            head.extend_from_slice(format!("{:x}\r\n", chunk.len()).as_bytes());
            head.extend_from_slice(&chunk);
            head.extend_from_slice(b"\r\n");
        }
        head.extend_from_slice(b"0\r\n\r\n");
        stream.write_all(&head).await.expect("write");
        let mut response = String::new();
        stream.read_to_string(&mut response).await.expect("read");
        response
    };
    let small = listener_reply(vec![b"hello ".to_vec(), b"world".to_vec()]).await;
    assert!(small.ends_with("11/11"), "{small}");
    let large = listener_reply(vec![vec![b'a'; 1000], vec![b'b'; 1000], vec![b'c'; 1000]]).await;
    assert!(large.ends_with("-1/3000"), "{large}");
}

#[tokio::test]
async fn request_racing_a_retired_target_gets_503() {
    let app = backend("app", None).await;
    let front = front(None).await;
    front
        .server
        .deploy(fast_request(&app.addr))
        .await
        .expect("deploy");
    // The window between routing and acquiring: the target is retired while
    // the service still points at it.
    front.server.retire_current_target("app");
    let answer = tokio::time::timeout(Duration::from_secs(2), get(front.addr, TEST_HOST, "/"))
        .await
        .expect("request spins on a retired target");
    assert_eq!(answer.0.as_u16(), 503);
    assert_eq!(answer.2, "service has no target\n");
}

#[tokio::test]
async fn tls_service_redirects_plain_http() {
    let app = backend("app", None).await;
    let front = front(None).await;
    let mut request = fast_request(&app.addr);
    request.tls = true;
    front.server.deploy(request).await.expect("deploy");
    let (code, headers, _) = get(front.addr, TEST_HOST, "/a/b?c=d").await;
    assert_eq!(code.as_u16(), 308);
    assert_eq!(headers["location"], "https://app.example.com/a/b?c=d");
    assert!(front.server.tls_host(TEST_HOST));
    assert!(!front.server.tls_host("other.example.com"));
    assert_eq!(
        app.hits.load(Ordering::SeqCst),
        0,
        "plain request reached the application"
    );
}

#[tokio::test]
async fn invalid_deploy_requests_are_refused() {
    let front = front(None).await;
    let base = DeployRequest {
        service: "app".to_owned(),
        target: "a:80".to_owned(),
        ..DeployRequest::default()
    };
    let cases = [
        DeployRequest {
            service: "Bad Name".to_owned(),
            ..base.clone()
        },
        DeployRequest {
            target: "a".to_owned(),
            ..base.clone()
        },
        DeployRequest {
            target: "a:0".to_owned(),
            ..base.clone()
        },
        DeployRequest {
            hosts: vec!["10.0.0.1".to_owned()],
            ..base.clone()
        },
        DeployRequest {
            hosts: vec!["*.example.com".to_owned()],
            ..base.clone()
        },
        DeployRequest {
            tls: true,
            ..base.clone()
        },
        DeployRequest {
            health_path: "health".to_owned(),
            ..base.clone()
        },
        DeployRequest {
            healthy_count: 1000,
            ..base.clone()
        },
        DeployRequest {
            deploy_timeout: GoDuration::from_secs(-1),
            ..base.clone()
        },
    ];
    for request in cases {
        let error = front
            .server
            .deploy(request.clone())
            .await
            .expect_err("invalid");
        assert!(!error.unhealthy, "{request:?}: {error:?}");
    }
}

#[tokio::test]
async fn control_socket_and_state_persistence() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = tempfile::Builder::new()
        .prefix("fp")
        .tempdir_in("/tmp")
        .expect("tempdir");
    let state = dir.path().join("state.json");
    let socket = dir.path().join("run").join("control.sock");
    let app = backend("app", None).await;
    let first = front(Some(state.clone())).await;
    let listener = listen_control(&socket).expect("listen");
    let stop = CancellationToken::new();
    let served = tokio::spawn({
        let server = first.server.clone();
        let stop = stop.clone();
        async move { server.serve_control(listener, stop).await }
    });
    let mode = |path: &std::path::Path| {
        std::fs::metadata(path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777
    };
    assert_eq!(mode(&socket), 0o600);
    assert_eq!(mode(socket.parent().expect("dir")), 0o700);
    let timeout = Duration::from_secs(10);
    let deploy = ControlRequest {
        op: "deploy".to_owned(),
        deploy: Some(fast_request(&app.addr)),
        ..ControlRequest::default()
    };
    let response = call(&socket, &deploy, timeout).await.expect("deploy");
    assert!(response.ok, "{response:?}");
    assert_eq!(response.deploy.expect("result").target, app.addr);
    let maintenance = ControlRequest {
        op: "maintenance".to_owned(),
        service: "app".to_owned(),
        maintenance: true,
        ..ControlRequest::default()
    };
    assert!(
        call(&socket, &maintenance, timeout)
            .await
            .expect("maintenance")
            .ok
    );
    let status = ControlRequest {
        op: "status".to_owned(),
        ..ControlRequest::default()
    };
    let response = call(&socket, &status, timeout).await.expect("status");
    assert_eq!(response.services.len(), 1);
    assert!(response.services[0].maintenance);
    assert_eq!(response.services[0].target, app.addr);
    let bad = ControlRequest {
        op: "deploy".to_owned(),
        deploy: Some(DeployRequest {
            service: "app".to_owned(),
            target: "127.0.0.1:1".to_owned(),
            deploy_timeout: GoDuration::from_millis(20),
            health_interval: GoDuration::from_millis(1),
            ..DeployRequest::default()
        }),
        ..ControlRequest::default()
    };
    let response = call(&socket, &bad, timeout)
        .await
        .expect("unhealthy deploy");
    assert!(!response.ok && response.code == "unhealthy", "{response:?}");
    let unknown = ControlRequest {
        op: "explode".to_owned(),
        ..ControlRequest::default()
    };
    assert!(!call(&socket, &unknown, timeout).await.expect("unknown").ok);
    // Unknown fields are refused.
    let mut raw = tokio::net::UnixStream::connect(&socket)
        .await
        .expect("connect");
    raw.write_all(b"{\"op\":\"status\",\"extra\":1}\n")
        .await
        .expect("write");
    let mut answer = String::new();
    raw.read_to_string(&mut answer).await.expect("read");
    assert!(answer.contains("invalid control request"), "{answer}");

    assert_eq!(mode(&state), 0o600);
    let saved = std::fs::read_to_string(&state).expect("state");
    assert!(
        saved.contains("\"version\": 1") && saved.contains("\"maintenance\": true"),
        "{saved}"
    );
    // A restarted proxy resumes routing from the saved state.
    let restarted = front(Some(state.clone())).await;
    let statuses = restarted.server.status();
    assert_eq!(statuses.len(), 1);
    assert_eq!(statuses[0].target, app.addr);
    assert!(statuses[0].maintenance);
    restarted
        .server
        .set_maintenance("app", false)
        .await
        .expect("off");
    assert_eq!(get(restarted.addr, TEST_HOST, "/").await.2, "app");
    stop.cancel();
    served.await.expect("control loop");
    // A stale socket is replaced; a regular file is not.
    listen_control(&socket).expect("stale socket replaced");
    let plain = dir.path().join("plain");
    std::fs::write(&plain, b"").expect("write");
    assert!(listen_control(&plain).is_err());
    // A corrupt state file stops the proxy from starting.
    std::fs::write(&state, b"{\"version\":2,\"services\":[]}").expect("write");
    assert!(Server::new(Logger::discard(), Some(state.clone())).is_err());
}

#[derive(Clone, Default)]
struct Captured(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("lock").extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Captured {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().expect("lock")).into_owned()
    }
}

#[tokio::test]
async fn access_log_omits_the_query() {
    let app = backend("app", None).await;
    let logs = Captured::default();
    let server = Server::new(Logger::new(Box::new(logs.clone())), None).expect("server");
    let front = front_with(server).await;
    front
        .server
        .deploy(fast_request(&app.addr))
        .await
        .expect("deploy");
    get(front.addr, TEST_HOST, "/reset?token=s3cr3t").await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let text = logs.text();
    let entry: serde_json::Value = text
        .lines()
        .filter(|line| line.contains("\"msg\":\"request\""))
        .find_map(|line| serde_json::from_str(line).ok())
        .expect("request log line");
    assert_eq!(entry["path"], "/reset");
    assert_eq!(entry["status"], 200);
    assert_eq!(entry["service"], "app");
    assert_eq!(entry["target"], app.addr);
    assert_eq!(entry["bytes"], 3);
    assert_eq!(entry["level"], "INFO");
    assert!(!text.contains("s3cr3t"), "query string logged");
}

/// A backend whose `Upgrade: echo` connections echo what they receive.
async fn echo_backend() -> support::Backend {
    backend(
        "app",
        handler(|mut request: Request<hyper::body::Incoming>| async move {
            let upgrade = hyper::upgrade::on(&mut request);
            tokio::spawn(async move {
                if let Ok(upgraded) = upgrade.await {
                    let mut io = hyper_util::rt::TokioIo::new(upgraded);
                    let mut buffer = [0_u8; 64];
                    while let Ok(n) = io.read(&mut buffer).await {
                        if n == 0 || io.write_all(&buffer[..n]).await.is_err() {
                            break;
                        }
                    }
                }
            });
            let mut response = status(101);
            response
                .headers_mut()
                .insert("connection", "Upgrade".parse().expect("value"));
            response
                .headers_mut()
                .insert("upgrade", "echo".parse().expect("value"));
            response
        }),
    )
    .await
}

/// Opens an `echo` tunnel through the proxy at `addr`.
async fn open_tunnel(addr: std::net::SocketAddr) -> tokio::net::TcpStream {
    let mut stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
    stream
        .write_all(format!("GET /ws HTTP/1.1\r\nHost: {TEST_HOST}\r\nConnection: Upgrade\r\nUpgrade: echo\r\n\r\n").as_bytes())
        .await
        .expect("write");
    let mut head = Vec::new();
    let mut byte = [0_u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).await.expect("read head");
        head.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&head).to_ascii_lowercase();
    assert!(head.starts_with("http/1.1 101"), "{head}");
    assert!(head.contains("upgrade: echo"), "{head}");
    stream
}

#[tokio::test]
async fn upgrade_through_proxy() {
    let app = echo_backend().await;
    let front = front(None).await;
    front
        .server
        .deploy(fast_request(&app.addr))
        .await
        .expect("deploy");
    let mut stream = open_tunnel(front.addr).await;
    stream.write_all(b"hello").await.expect("write");
    let mut echoed = [0_u8; 5];
    tokio::time::timeout(Duration::from_secs(2), stream.read_exact(&mut echoed))
        .await
        .expect("echo in time")
        .expect("echo");
    assert_eq!(&echoed, b"hello");
    // The tunnel holds the target: status shows it in flight.
    assert_eq!(front.server.status()[0].in_flight, 1);
    drop(stream);
}

/// Limits with a short idle timeout.
fn idle_limits(idle: Duration) -> forge_proxy::HttpLimits {
    forge_proxy::HttpLimits {
        idle_timeout: idle,
        ..limits()
    }
}

/// Reads until the peer closes; `None` if it stays open for `wait`.
async fn closed_within(stream: &mut tokio::net::TcpStream, wait: Duration) -> Option<usize> {
    let mut total = 0;
    let mut buffer = vec![0_u8; 64 << 10];
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        match tokio::time::timeout_at(deadline, stream.read(&mut buffer)).await {
            Err(_) => return None,
            Ok(Ok(0) | Err(_)) => return Some(total),
            Ok(Ok(n)) => total += n,
        }
    }
}

async fn wait_for_in_flight(server: &Server, expected: i64, wait: Duration) {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let in_flight = server.status().first().map_or(0, |status| status.in_flight);
        if in_flight == expected {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "in flight stayed {in_flight}, expected {expected}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn idle_keep_alive_connections_are_closed() {
    let app = backend("app", None).await;
    let front = front_with_limits(
        Server::new(Logger::discard(), None).expect("server"),
        idle_limits(Duration::from_millis(300)),
    )
    .await;
    front
        .server
        .deploy(fast_request(&app.addr))
        .await
        .expect("deploy");
    let mut stream = tokio::net::TcpStream::connect(front.addr)
        .await
        .expect("connect");
    stream
        .write_all(format!("GET / HTTP/1.1\r\nHost: {TEST_HOST}\r\n\r\n").as_bytes())
        .await
        .expect("request");
    let started = std::time::Instant::now();
    let mut response = Vec::new();
    let mut buffer = [0_u8; 1024];
    while !String::from_utf8_lossy(&response).ends_with("app") {
        let n = stream.read(&mut buffer).await.expect("response");
        assert!(n > 0, "closed before the response");
        response.extend_from_slice(&buffer[..n]);
    }
    assert!(String::from_utf8_lossy(&response).starts_with("HTTP/1.1 200"));
    // Kept alive, then closed once idle for the idle timeout (shorter than
    // the 10 s header timeout here).
    assert_eq!(
        closed_within(&mut stream, Duration::from_secs(5)).await,
        Some(0),
        "idle connection kept open"
    );
    assert!(started.elapsed() >= Duration::from_millis(300));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_client_that_stops_reading_is_disconnected_and_releases_the_target() {
    const BODY: usize = 64 << 20;
    let app = backend(
        "app",
        handler(|_request| async { text(Bytes::from(vec![b'x'; BODY])) }),
    )
    .await;
    let front = front_with_limits(
        Server::new(Logger::discard(), None).expect("server"),
        idle_limits(Duration::from_millis(300)),
    )
    .await;
    front
        .server
        .deploy(fast_request(&app.addr))
        .await
        .expect("deploy");
    let mut stream = tokio::net::TcpStream::connect(front.addr)
        .await
        .expect("connect");
    stream
        .write_all(format!("GET /large HTTP/1.1\r\nHost: {TEST_HOST}\r\n\r\n").as_bytes())
        .await
        .expect("request");
    // The response is in flight while the client reads nothing...
    wait_for_in_flight(&front.server, 1, Duration::from_secs(5)).await;
    // ...until the proxy gives up on the stalled write: the lease (which a
    // drain would wait for) and the connection are released.
    wait_for_in_flight(&front.server, 0, Duration::from_secs(10)).await;
    let received = closed_within(&mut stream, Duration::from_secs(10))
        .await
        .expect("connection closed");
    assert!(received < BODY, "the whole body arrived ({received} bytes)");
}

#[tokio::test]
async fn idle_tunnels_are_closed_and_active_ones_kept() {
    let app = echo_backend().await;
    let front = front_with_limits(
        Server::new(Logger::discard(), None).expect("server"),
        idle_limits(Duration::from_millis(400)),
    )
    .await;
    front
        .server
        .deploy(fast_request(&app.addr))
        .await
        .expect("deploy");
    let mut stream = open_tunnel(front.addr).await;
    // Traffic every 100 ms for twice the idle timeout keeps it open.
    for round in 0..8_u8 {
        stream.write_all(&[round]).await.expect("write");
        let mut echoed = [0_u8; 1];
        tokio::time::timeout(Duration::from_secs(2), stream.read_exact(&mut echoed))
            .await
            .expect("echo in time")
            .expect("echo");
        assert_eq!(echoed, [round]);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(front.server.status()[0].in_flight, 1);
    // Silence in both directions closes it and releases the target.
    let silent = std::time::Instant::now();
    assert_eq!(
        closed_within(&mut stream, Duration::from_secs(5)).await,
        Some(0),
        "idle tunnel kept open"
    );
    // The last echo was 100 ms before `silent`: about 300 ms remain.
    assert!(silent.elapsed() >= Duration::from_millis(200));
    wait_for_in_flight(&front.server, 0, Duration::from_secs(5)).await;
}

#[tokio::test]
async fn an_existing_control_directory_is_restricted_and_a_link_refused() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = tempfile::Builder::new()
        .prefix("fp")
        .tempdir_in("/tmp")
        .expect("tempdir");
    let mode = |path: &std::path::Path| {
        std::fs::symlink_metadata(path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o7777
    };
    let run = dir.path().join("run");
    std::fs::create_dir(&run).expect("mkdir");
    std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o777)).expect("chmod");
    let socket = run.join("control.sock");
    let listener = listen_control(&socket).expect("listen");
    assert_eq!(mode(&run), 0o700);
    assert_eq!(mode(&socket) & 0o777, 0o600);
    drop(listener);

    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&run, &link).expect("symlink");
    std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let error = listen_control(&link.join("control.sock")).expect_err("link refused");
    assert!(error.contains("is not a directory"), "{error}");
    assert_eq!(mode(&run), 0o755);
}

#[tokio::test]
async fn a_restarted_proxy_removes_temporary_files_of_interrupted_writes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state = dir.path().join("state.json");
    let leftover = dir.path().join(".state.json.0123456789ab.tmp");
    std::fs::write(&leftover, b"{").expect("write");
    let other = dir.path().join("notes.txt");
    std::fs::write(&other, b"keep").expect("write");
    let server = Server::new(Logger::discard(), Some(state)).expect("server");
    assert!(!leftover.exists(), "stale temporary file kept");
    assert!(other.exists());
    drop(server);
}
