//! What the certificate manager does when it cannot get or keep a
//! certificate: a CA that refuses, a corrupt cache entry, an account key that
//! is unusable or in the older SEC1 form, a chain that does not belong to its
//! key, and settings that cannot work at all. The proxy keeps serving and
//! logs each failure with its reason and its next attempt.

mod support;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use forge_proxy::log::Logger;
use forge_proxy::tls::{AcmeSettings, Tls};
use forge_proxy::{DeployRequest, Server};
use support::scripted_ca::{ca, except, happy, problem, raw, reply};
use support::{TEST_HOST, backend, fast_request, front_with};
use tokio_util::sync::CancellationToken;

const ACCOUNT_FILE: &str = "acme_account+key";

/// A P-256 account key in the SEC1 form autocert writes.
const SEC1_ACCOUNT_KEY: &str = "-----BEGIN EC PRIVATE KEY-----
MHcCAQEEIHqyKQ8RLM1BPvtPLK+jm17lZycee5e03sRaZleTCxOAoAoGCCqGSM49
AwEHoUQDQgAEeFJ8XdMMR7FEZ3DV0Q/zbvKdO/UCyLu30dOlSl5F8wlQE8mBpBIs
swSr4/bIkukU7rB3ZFxfGcucAZNhPly48g==
-----END EC PRIVATE KEY-----
";

/// A log sink the test can read.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

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

/// Runs the certificate manager for `TEST_HOST` against the CA at
/// `directory` after `prepare` set up the cache, until it logged its first
/// issuance failure; returns the log.
async fn first_failure(
    directory: String,
    prepare: impl FnOnce(&Path),
) -> (String, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let cache = dir.path().join("acme");
    std::fs::create_dir(&cache).expect("mkdir");
    prepare(&cache);
    let logs = Captured::default();
    let server = Server::new(Logger::new(Box::new(logs.clone())), None).expect("server");
    let settings = AcmeSettings {
        cache,
        email: "ops@example.com".to_owned(),
        directory,
        ..AcmeSettings::default()
    };
    let tls = Arc::new(Tls::new(server.clone(), settings).expect("tls"));
    let front = front_with(server).await;
    let app = backend("app", None).await;
    let stop = CancellationToken::new();
    let manager = tokio::spawn({
        let (tls, stop) = (Arc::clone(&tls), stop.clone());
        async move { tls.run(stop).await }
    });
    let mut request: DeployRequest = fast_request(&app.addr);
    request.tls = true;
    front.server.deploy(request).await.expect("deploy");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while !logs.text().contains("certificate issuance failed") {
        assert!(
            tokio::time::Instant::now() < deadline,
            "no failure logged: {}",
            logs.text()
        );
        // real-clock: polls the logs of a task or process that runs on its own threads, until a deadline
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    stop.cancel();
    manager.await.expect("the manager stops");
    (logs.text(), dir)
}

#[tokio::test]
async fn a_ca_that_refuses_is_logged_with_its_reason_and_the_next_attempt() {
    let refusing = ca(except(|ask| {
        (ask.path == "/directory")
            .then(|| reply(503, &problem("urn:test:unavailable", "come back later")))
    }))
    .await;
    let (logs, _dir) = first_failure(format!("{}/directory", refusing.base), |cache| {
        // A cached certificate that no longer parses is reported, then replaced.
        std::fs::write(cache.join(TEST_HOST), b"not a pem").expect("write");
    })
    .await;
    assert!(logs.contains("cached certificate unusable"), "{logs}");
    assert!(
        logs.contains("urn:test:unavailable (503): come back later"),
        "{logs}"
    );
    // The first retry waits one minute.
    assert!(logs.contains("\"retry_in\":\"1m0s\""), "{logs}");
    assert!(logs.contains(TEST_HOST), "{logs}");
}

#[tokio::test]
async fn an_unusable_account_key_stops_issuance_without_touching_the_ca() {
    let asked = Arc::new(AtomicUsize::new(0));
    let counted = ca({
        let asked = Arc::clone(&asked);
        move |ask| {
            asked.fetch_add(1, Ordering::SeqCst);
            happy(ask)
        }
    })
    .await;
    let (logs, _dir) = first_failure(format!("{}/directory", counted.base), |cache| {
        std::fs::write(cache.join(ACCOUNT_FILE), b"not a key").expect("write");
    })
    .await;
    assert!(logs.contains("is not a P-256 private key"), "{logs}");
    assert!(logs.contains(ACCOUNT_FILE), "{logs}");
    assert_eq!(asked.load(Ordering::SeqCst), 0, "the CA was asked: {logs}");

    // An account "file" that cannot be read at all is its own error.
    let (logs, _dir) = first_failure(format!("{}/directory", counted.base), |cache| {
        std::fs::create_dir(cache.join(ACCOUNT_FILE)).expect("mkdir");
    })
    .await;
    assert!(logs.contains("read "), "{logs}");
    assert_eq!(asked.load(Ordering::SeqCst), 0, "the CA was asked: {logs}");
}

#[tokio::test]
async fn a_sec1_account_key_is_accepted_and_a_chain_without_certificates_is_refused() {
    let accounts = Arc::new(AtomicUsize::new(0));
    let scripted = ca({
        let accounts = Arc::clone(&accounts);
        move |ask| {
            if ask.path == "/account" {
                accounts.fetch_add(1, Ordering::SeqCst);
            }
            if ask.path == "/cert" {
                return raw(200, "");
            }
            happy(ask)
        }
    })
    .await;
    let (logs, _dir) = first_failure(format!("{}/directory", scripted.base), |cache| {
        std::fs::write(cache.join(ACCOUNT_FILE), SEC1_ACCOUNT_KEY).expect("write");
    })
    .await;
    assert_eq!(accounts.load(Ordering::SeqCst), 1, "{logs}");
    assert!(logs.contains("no certificate"), "{logs}");
    assert!(logs.contains("certificate issuance failed"), "{logs}");
}

#[tokio::test]
async fn a_chain_that_does_not_parse_is_not_cached_or_served() {
    let scripted = ca(happy).await;
    let (logs, dir) = first_failure(format!("{}/directory", scripted.base), |_| {}).await;
    // `happy` answers with a certificate body that is not a certificate.
    assert!(logs.contains("certificate issuance failed"), "{logs}");
    assert!(logs.contains("certificate"), "{logs}");
    assert!(!dir.path().join("acme").join(TEST_HOST).exists());
}

#[tokio::test]
async fn certificates_are_validated_when_installed_and_the_settings_when_created() {
    let server = Server::new(Logger::discard(), None).expect("server");
    let dir = tempfile::tempdir().expect("tempdir");
    let settings = |cache: PathBuf, directory: &str| AcmeSettings {
        cache,
        directory: directory.to_owned(),
        ..AcmeSettings::default()
    };
    let tls = Tls::new(
        server.clone(),
        settings(dir.path().join("acme"), "http://127.0.0.1:1/directory"),
    )
    .expect("tls");
    assert_eq!(format!("{tls:?}"), "Tls");
    assert_eq!(format!("{:?}", tls.acceptor()), "TlsAccept");
    assert_eq!(
        tls.cache_path(TEST_HOST),
        dir.path().join("acme").join(TEST_HOST)
    );
    for pem in [
        &b""[..],
        b"not a pem",
        b"-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----\n",
    ] {
        assert!(tls.install(TEST_HOST, pem).is_err());
    }
    assert!(tls.install("not a host name", b"x").is_err());

    // Settings that cannot work are refused when the manager is created.
    let error = Tls::new(
        server.clone(),
        settings(
            dir.path().join("plain"),
            "http://acme.example.com/directory",
        ),
    )
    .expect_err("plain HTTP to a remote CA");
    assert!(
        error.contains("plain HTTP is accepted only on loopback"),
        "{error}"
    );
    let file = dir.path().join("file");
    std::fs::write(&file, b"x").expect("write");
    let error = Tls::new(
        server,
        settings(file.join("acme"), "http://127.0.0.1:1/directory"),
    )
    .expect_err("a cache below a file");
    assert!(error.starts_with("ACME cache: "), "{error}");
}
