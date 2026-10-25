//! The ACME client against scripted certificate authorities that fail in
//! every way a real one can: problem documents, a missing nonce or
//! `Location`, `badNonce`, a challenge nobody can answer, invalid orders,
//! oversized and malformed answers, and connections that fail or close.
//! Every failure must surface as an error that names what went wrong and
//! never leaves a challenge published.

mod support;

use std::time::Duration;

use forge_proxy::acme::{Challenges, Client};
use forge_proxy::der::generate_p256;
use serde_json::json;
use support::scripted_ca::{Ca, ca, client, except, happy, pending, problem, raw, reply};
use tokio::net::TcpListener;

const HOST: &str = "app.example.com";

/// Issues for `HOST` and returns the error text, asserting no challenge stays published.
async fn failure(ca: &Ca) -> String {
    let challenges = Challenges::default();
    let key = generate_p256().expect("key");
    let error = client(&ca.base)
        .issue(HOST, &key, &challenges)
        .await
        .expect_err("the issuance must fail");
    assert!(
        challenges.lock().expect("challenges").is_empty(),
        "a challenge is still published after: {error}"
    );
    error
}

#[tokio::test]
async fn a_scripted_ca_that_does_everything_right_issues_the_chain() {
    let ca = ca(happy).await;
    let challenges = Challenges::default();
    let chain = client(&ca.base)
        .issue(HOST, &generate_p256().expect("key"), &challenges)
        .await
        .expect("issues");
    assert!(chain.starts_with("-----BEGIN CERTIFICATE-----"), "{chain}");
}

#[tokio::test]
async fn the_directory_must_answer_with_a_usable_document() {
    let down = ca(except(|ask| {
        (ask.path == "/directory")
            .then(|| reply(503, &problem("urn:test:unavailable", "come back later")))
    }))
    .await;
    assert_eq!(
        failure(&down).await,
        "acme: urn:test:unavailable (503): come back later"
    );

    let plain = ca(except(|ask| {
        (ask.path == "/directory").then(|| raw(500, "oops"))
    }))
    .await;
    assert_eq!(failure(&plain).await, "acme: HTTP 500");

    let typeless = ca(except(|ask| {
        (ask.path == "/directory").then(|| reply(400, &json!({})))
    }))
    .await;
    assert_eq!(failure(&typeless).await, "acme: error (400): no detail");

    let garbage = ca(except(|ask| {
        (ask.path == "/directory").then(|| raw(200, "not json"))
    }))
    .await;
    assert!(
        failure(&garbage)
            .await
            .starts_with("acme: malformed response: ")
    );

    for missing in ["newNonce", "newAccount", "newOrder"] {
        let incomplete = ca(except(move |ask| {
            (ask.path == "/directory").then(|| {
                let mut directory = json!({"newNonce": "http://127.0.0.1/n", "newAccount": "http://127.0.0.1/a", "newOrder": "http://127.0.0.1/o"});
                directory.as_object_mut().expect("object").remove(missing);
                reply(200, &directory)
            })
        }))
        .await;
        assert_eq!(
            failure(&incomplete).await,
            format!("acme: directory lacks {missing}")
        );
    }
}

#[tokio::test]
async fn a_ca_that_gives_no_nonce_or_no_location_is_refused() {
    let no_nonce = ca(except(|ask| {
        (ask.method == "HEAD" && ask.path == "/nonce")
            .then(|| raw(200, "").with("replay-nonce", ""))
    }))
    .await;
    assert_eq!(failure(&no_nonce).await, "acme: no Replay-Nonce");

    let no_account_location = ca(except(|ask| {
        (ask.path == "/account").then(|| reply(201, &json!({"status": "valid"})))
    }))
    .await;
    assert_eq!(
        failure(&no_account_location).await,
        "acme: account without Location"
    );

    let no_order_location = ca(except(|ask| {
        (ask.path == "/order").then(|| {
            reply(
                201,
                &json!({"authorizations": [], "finalize": format!("{}/finalize", ask.base)}),
            )
        })
    }))
    .await;
    assert_eq!(
        failure(&no_order_location).await,
        "acme: order without Location"
    );

    let no_finalize = ca(except(|ask| {
        (ask.path == "/order").then(|| {
            reply(201, &json!({"authorizations": []}))
                .with("location", format!("{}/order/1", ask.base))
        })
    }))
    .await;
    assert_eq!(
        failure(&no_finalize).await,
        "acme: order without finalize URL"
    );
}

#[tokio::test]
async fn bad_nonce_is_retried_a_bounded_number_of_times() {
    let bad_nonce = || {
        reply(
            400,
            &problem("urn:ietf:params:acme:error:badNonce", "stale"),
        )
    };
    // Two stale nonces, then the account is created: the issuance succeeds.
    let recovers = ca(except(move |ask| {
        (ask.path == "/account" && ask.nth < 2).then(bad_nonce)
    }))
    .await;
    let challenges = Challenges::default();
    client(&recovers.base)
        .issue(HOST, &generate_p256().expect("key"), &challenges)
        .await
        .expect("recovers from two stale nonces");

    // A CA that never accepts a nonce is given up on after three attempts.
    let never = ca(except(move |ask| (ask.path == "/account").then(bad_nonce))).await;
    assert_eq!(
        failure(&never).await,
        "acme: urn:ietf:params:acme:error:badNonce (400): stale"
    );
}

#[tokio::test]
async fn a_problem_document_names_its_type_and_detail() {
    let refused = ca(except(|ask| {
        (ask.path == "/account").then(|| {
            reply(
                403,
                &problem(
                    "urn:ietf:params:acme:error:unauthorized",
                    "no account for you",
                ),
            )
        })
    }))
    .await;
    assert_eq!(
        failure(&refused).await,
        "acme: urn:ietf:params:acme:error:unauthorized (403): no account for you"
    );
    let rate_limited = ca(except(|ask| {
        (ask.path == "/order").then(|| {
            reply(
                429,
                &problem("urn:ietf:params:acme:error:rateLimited", "too many orders"),
            )
        })
    }))
    .await;
    assert!(
        failure(&rate_limited)
            .await
            .contains("(429): too many orders")
    );
}

#[tokio::test]
async fn authorizations_without_a_usable_http_challenge_are_refused() {
    let dns_only = ca(except(|ask| {
        (ask.path == "/authz").then(|| {
            reply(200, &json!({"status": "pending", "challenges": [{"type": "dns-01", "token": "t", "url": "http://127.0.0.1/c"}]}))
        })
    }))
    .await;
    assert_eq!(
        failure(&dns_only).await,
        "acme: no http-01 challenge offered"
    );

    let none = ca(except(|ask| {
        (ask.path == "/authz").then(|| reply(200, &json!({"status": "pending"})))
    }))
    .await;
    assert_eq!(failure(&none).await, "acme: no http-01 challenge offered");

    for token in ["", "../etc/passwd", "a b", "tok/en"] {
        let hostile = ca(except(move |ask| {
            (ask.path == "/authz").then(|| pending(ask.base, token))
        }))
        .await;
        assert_eq!(
            failure(&hostile).await,
            "acme: invalid challenge token",
            "token {token:?}"
        );
    }
    let tokenless = ca(except(|ask| {
        (ask.path == "/authz").then(|| {
            reply(200, &json!({"status": "pending", "challenges": [{"type": "http-01", "url": format!("{}/chall", ask.base)}]}))
        })
    }))
    .await;
    assert_eq!(failure(&tokenless).await, "acme: invalid challenge token");

    let urlless = ca(except(|ask| {
        (ask.path == "/authz").then(|| {
            reply(200, &json!({"status": "pending", "challenges": [{"type": "http-01", "token": "tok-1"}]}))
        })
    }))
    .await;
    assert_eq!(failure(&urlless).await, "acme: challenge without URL");
}

#[tokio::test]
async fn a_challenge_the_ca_rejects_ends_the_issuance_with_the_cas_reason() {
    let rejected = ca(except(|ask| match (ask.path, ask.nth) {
        ("/authz", 0) => Some(pending(ask.base, "tok-1")),
        ("/chall", _) => Some(reply(200, &json!({"status": "pending"}))),
        ("/authz", _) => Some(reply(
            200,
            &json!({"status": "invalid", "challenges": [{"type": "dns-01"}, {"type": "http-01", "error": {"detail": "connection refused"}}]}),
        )),
        _ => None,
    }))
    .await;
    assert_eq!(
        failure(&rejected).await,
        "acme: authorization is invalid: connection refused"
    );

    let unexplained = ca(except(|ask| match (ask.path, ask.nth) {
        ("/authz", 0) => Some(pending(ask.base, "tok-1")),
        ("/chall", _) => Some(reply(200, &json!({}))),
        ("/authz", _) => Some(reply(200, &json!({"status": "invalid"}))),
        _ => None,
    }))
    .await;
    assert_eq!(
        failure(&unexplained).await,
        "acme: authorization is invalid: no detail"
    );

    let challenge_refused = ca(except(|ask| match (ask.path, ask.nth) {
        ("/authz", _) => Some(pending(ask.base, "tok-1")),
        ("/chall", _) => Some(reply(
            400,
            &problem("urn:ietf:params:acme:error:malformed", "bad challenge"),
        )),
        _ => None,
    }))
    .await;
    assert!(
        failure(&challenge_refused)
            .await
            .ends_with("(400): bad challenge")
    );
}

#[tokio::test]
async fn an_order_that_ends_invalid_or_without_a_certificate_is_an_error() {
    let invalid = ca(except(|ask| {
        (ask.path == "/order/1").then(|| {
            reply(
                200,
                &json!({"status": "invalid", "error": {"detail": "CSR rejected"}}),
            )
        })
    }))
    .await;
    assert_eq!(
        failure(&invalid).await,
        "acme: order is invalid: CSR rejected"
    );

    let unexplained = ca(except(|ask| {
        (ask.path == "/order/1").then(|| reply(200, &json!({"status": "invalid"})))
    }))
    .await;
    assert_eq!(
        failure(&unexplained).await,
        "acme: order is invalid: no detail"
    );

    let no_certificate = ca(except(|ask| {
        (ask.path == "/order/1").then(|| reply(200, &json!({"status": "valid"})))
    }))
    .await;
    assert_eq!(
        failure(&no_certificate).await,
        "acme: valid order without certificate URL"
    );

    let not_pem = ca(except(|ask| {
        (ask.path == "/cert").then(|| raw(200, vec![0xff, 0xfe, 0x00]))
    }))
    .await;
    assert_eq!(failure(&not_pem).await, "acme: certificate is not PEM");

    let finalize_refused = ca(except(|ask| {
        (ask.path == "/finalize").then(|| {
            reply(
                403,
                &problem("urn:ietf:params:acme:error:orderNotReady", "not ready"),
            )
        })
    }))
    .await;
    assert!(
        failure(&finalize_refused)
            .await
            .ends_with("(403): not ready")
    );
}

#[tokio::test]
async fn polling_honors_retry_after_and_falls_back_when_it_is_not_a_number() {
    let slow = ca(except(|ask| match (ask.path, ask.nth) {
        ("/order/1", 0) => {
            Some(reply(200, &json!({"status": "processing"})).with("retry-after", "1"))
        }
        ("/order/1", 1) => {
            Some(reply(200, &json!({"status": "processing"})).with("retry-after", "soon"))
        }
        _ => None,
    }))
    .await;
    let started = std::time::Instant::now();
    let chain = client(&slow.base)
        .issue(HOST, &generate_p256().expect("key"), &Challenges::default())
        .await
        .expect("issues after two polls");
    assert!(chain.contains("BEGIN CERTIFICATE"));
    // Two waits of one second: the header's value and the default.
    assert!(
        started.elapsed() >= Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn a_certificate_key_that_is_not_p256_is_refused_before_a_csr_is_made() {
    let ca = ca(happy).await;
    let error = client(&ca.base)
        .issue(HOST, b"not a key", &Challenges::default())
        .await
        .expect_err("refused");
    assert_eq!(error, "acme: the certificate key is not a P-256 key");
}

#[tokio::test]
async fn clients_refuse_unusable_keys_urls_and_trust_anchors() {
    let key = generate_p256().expect("key");
    let error = Client::new("http://127.0.0.1:1/dir", b"not a key", "").expect_err("key");
    assert_eq!(error, "acme: the account key is not a P-256 key");
    for url in [
        "not a url",
        "ftp://127.0.0.1/dir",
        "http://acme.example.com/dir",
    ] {
        assert!(Client::new(url, &key, "").is_err(), "{url}");
    }
    let bad_root = rustls::pki_types::CertificateDer::from(b"not a certificate".to_vec());
    let error =
        Client::with_roots("http://127.0.0.1:1/dir", &key, "", &[bad_root]).expect_err("root");
    assert!(error.starts_with("acme: trust anchor: "), "{error}");
}

#[tokio::test]
async fn network_failures_are_reported_by_stage() {
    // Nothing listens.
    let closed = {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        format!("http://{}", listener.local_addr().expect("addr"))
    };
    let error = client(&closed)
        .issue(HOST, &generate_p256().expect("key"), &Challenges::default())
        .await
        .expect_err("refused");
    assert!(error.starts_with("acme: connect 127.0.0.1:"), "{error}");

    // A server that hangs up without answering.
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    let hangup = tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            drop(stream);
        }
    });
    let error = client(&base)
        .issue(HOST, &generate_p256().expect("key"), &Challenges::default())
        .await
        .expect_err("hung up");
    assert!(error.starts_with("acme: http: "), "{error}");
    hangup.abort();

    // An https directory served as plain HTTP fails the handshake.
    let plain = ca(happy).await;
    let https = plain.base.replacen("http://", "https://", 1);
    let error = client(&https)
        .issue(HOST, &generate_p256().expect("key"), &Challenges::default())
        .await
        .expect_err("handshake");
    assert!(error.starts_with("acme: tls 127.0.0.1:"), "{error}");
}

#[tokio::test]
async fn an_oversized_answer_is_cut_off() {
    let huge = ca(except(|ask| {
        (ask.path == "/directory").then(|| raw(200, vec![b' '; (1 << 20) + 10]))
    }))
    .await;
    assert_eq!(failure(&huge).await, "acme: response too large");
}
