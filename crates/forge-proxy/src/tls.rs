//! HTTPS: certificates for exactly the hosts of services deployed with
//! `tls`, obtained from an ACME CA (Let's Encrypt by default) and served
//! with rustls (TLS 1.2 and 1.3, `ring` provider).
//!
//! Certificates are requested ahead of time — when a TLS service is
//! deployed and when one has less than 30 days left — by one owned renewal
//! loop, never during a handshake. A handshake for a host that is not a TLS
//! service, or has no certificate yet, fails: nothing is served with a
//! wrong or self-signed certificate.
//!
//! The cache directory (on the proxy's volume) holds `acme_account+key` and
//! one file per host with its private key and certificate chain in PEM, the
//! layout Forge-Go's autocert cache uses, so either proxy image can reuse
//! the other's account and certificates.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use rustls::pki_types::pem::PemObject as _;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::acme::{self, Challenges};
use crate::clock::Timestamp;
use crate::listen::Accept;
use crate::log::text;
use crate::server::Server;

/// Renew certificates with less than this left (autocert's default).
const RENEW_BEFORE: Duration = Duration::from_secs(30 * 24 * 3600);
/// How often certificates are checked when nothing is pending.
const CHECK_INTERVAL: Duration = Duration::from_secs(12 * 3600);
/// The longest one issuance may take.
const ISSUE_BUDGET: Duration = Duration::from_secs(300);
const FIRST_BACKOFF: Duration = Duration::from_secs(60);
const MAX_BACKOFF: Duration = Duration::from_secs(3600);
const ACCOUNT_FILE: &str = "acme_account+key";

/// Where and from whom certificates come.
#[derive(Clone, Debug)]
pub struct AcmeSettings {
    /// Cache directory (created 0700).
    pub cache: PathBuf,
    /// Contact address registered with the CA.
    pub email: String,
    /// Directory URL; empty is Let's Encrypt production.
    pub directory: String,
}

struct Loaded {
    key: Arc<CertifiedKey>,
    not_after: i64,
}

struct Manager {
    server: Server,
    settings: AcmeSettings,
    provider: Arc<rustls::crypto::CryptoProvider>,
    certificates: RwLock<HashMap<String, Loaded>>,
    failures: Mutex<HashMap<String, (Instant, Duration)>>,
    challenges: Challenges,
}

/// HTTPS for one proxy.
pub struct Tls {
    manager: Arc<Manager>,
    config: Arc<rustls::ServerConfig>,
}

impl std::fmt::Debug for Tls {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Tls")
    }
}

fn read<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lock.read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn write<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    lock.write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Tls {
    /// Prepares the cache and starts answering HTTP-01 challenges on the
    /// plain listener.
    ///
    /// # Errors
    ///
    /// The cache directory cannot be created, or the directory URL is not
    /// usable (plain HTTP is refused except on loopback).
    pub fn new(server: Server, mut settings: AcmeSettings) -> Result<Self, String> {
        use std::os::unix::fs::DirBuilderExt as _;

        if settings.directory.is_empty() {
            acme::LETS_ENCRYPT.clone_into(&mut settings.directory);
        }
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&settings.cache)
            .map_err(|error| format!("create ACME cache {}: {error}", settings.cache.display()))?;
        // Validates the URL now: fail at start, not at the first deploy.
        acme::Client::new(
            &settings.directory,
            &crate::der::generate_p256()?,
            &settings.email,
        )?;
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let manager = Arc::new(Manager {
            server: server.clone(),
            settings,
            provider: Arc::clone(&provider),
            certificates: RwLock::new(HashMap::new()),
            failures: Mutex::new(HashMap::new()),
            challenges: server.enable_challenges(),
        });
        let mut config = rustls::ServerConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
            .map_err(|error| format!("tls configuration: {error}"))?
            .with_no_client_auth()
            .with_cert_resolver(Arc::new(Resolver {
                manager: Arc::clone(&manager),
            }));
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(Self {
            manager,
            config: Arc::new(config),
        })
    }

    /// The handshake side.
    #[must_use]
    pub fn acceptor(&self) -> TlsAccept {
        TlsAccept {
            acceptor: tokio_rustls::TlsAcceptor::from(Arc::clone(&self.config)),
        }
    }

    /// Keeps certificates for every TLS host until `stop` is cancelled.
    pub async fn run(&self, stop: CancellationToken) {
        let manager = &self.manager;
        let mut client: Option<acme::Client> = None;
        loop {
            let mut wake = Instant::now() + CHECK_INTERVAL;
            for host in manager.server.tls_hosts() {
                if stop.is_cancelled() {
                    return;
                }
                if manager.fresh(&host) {
                    continue;
                }
                if let Some((next, _)) = lock(&manager.failures).get(&host)
                    && *next > Instant::now()
                {
                    wake = wake.min(*next);
                    continue;
                }
                let attempt = async {
                    if client.is_none() {
                        client = Some(manager.client()?);
                    }
                    let Some(client) = client.as_mut() else {
                        return Err("acme client unavailable".to_owned());
                    };
                    manager.issue(client, &host).await
                };
                let outcome = tokio::select! {
                    outcome = tokio::time::timeout(ISSUE_BUDGET, attempt) => {
                        outcome.unwrap_or_else(|_| Err("certificate issuance timed out".to_owned()))
                    }
                    () = stop.cancelled() => return,
                };
                match outcome {
                    Ok(not_after) => {
                        lock(&manager.failures).remove(&host);
                        manager.server.logger().info(
                            "certificate obtained",
                            &[
                                ("host", text(&host)),
                                (
                                    "not_after",
                                    text(
                                        Timestamp {
                                            unix_seconds: not_after,
                                            nanos: 0,
                                            offset_seconds: 0,
                                        }
                                        .rfc3339(),
                                    ),
                                ),
                            ],
                        );
                    }
                    Err(error) => {
                        let mut failures = lock(&manager.failures);
                        let backoff = failures.get(&host).map_or(FIRST_BACKOFF, |(_, previous)| {
                            (*previous * 2).min(MAX_BACKOFF)
                        });
                        let next = Instant::now() + backoff;
                        failures.insert(host.clone(), (next, backoff));
                        wake = wake.min(next);
                        manager.server.logger().warn(
                            "certificate issuance failed",
                            &[
                                ("host", text(&host)),
                                ("error", text(&error)),
                                (
                                    "retry_in",
                                    text(
                                        crate::duration::GoDuration::from_std(backoff).to_string(),
                                    ),
                                ),
                            ],
                        );
                    }
                }
            }
            tokio::select! {
                () = tokio::time::sleep_until(wake) => {}
                () = manager.server.tls_changed() => {}
                () = stop.cancelled() => return,
            }
        }
    }
}

impl Manager {
    fn client(&self) -> Result<acme::Client, String> {
        let path = self.settings.cache.join(ACCOUNT_FILE);
        let pkcs8 = match std::fs::read(&path) {
            Ok(pem) => account_key(&pem)
                .ok_or_else(|| format!("{} is not a P-256 private key", path.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let pkcs8 = crate::der::generate_p256()?;
                let pem = pem_block("PRIVATE KEY", &pkcs8);
                crate::server::write_atomically(&path, pem.as_bytes())
                    .map_err(|error| format!("write {}: {error}", path.display()))?;
                pkcs8
            }
            Err(error) => return Err(format!("read {}: {error}", path.display())),
        };
        acme::Client::new(&self.settings.directory, &pkcs8, &self.settings.email)
    }

    /// Whether `host` has a certificate with more than [`RENEW_BEFORE`]
    /// left, loading it from the cache when it is not in memory.
    fn fresh(&self, host: &str) -> bool {
        let renew_at = Timestamp::now().unix_seconds
            + i64::try_from(RENEW_BEFORE.as_secs()).unwrap_or(i64::MAX);
        if let Some(loaded) = read(&self.certificates).get(host) {
            return loaded.not_after > renew_at;
        }
        let Ok(pem) = std::fs::read(self.settings.cache.join(host)) else {
            return false;
        };
        match self.certified(&pem) {
            Ok(loaded) => {
                let fresh = loaded.not_after > renew_at;
                write(&self.certificates).insert(host.to_owned(), loaded);
                fresh
            }
            Err(error) => {
                self.server.logger().warn(
                    "cached certificate unusable",
                    &[("host", text(host)), ("error", text(error))],
                );
                false
            }
        }
    }

    /// A key and chain from PEM, checked to belong together.
    fn certified(&self, pem: &[u8]) -> Result<Loaded, String> {
        let key =
            PrivateKeyDer::from_pem_slice(pem).map_err(|error| format!("private key: {error}"))?;
        let chain: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(pem)
            .collect::<Result<_, _>>()
            .map_err(|error| format!("certificate: {error}"))?;
        let leaf = chain.first().ok_or_else(|| "no certificate".to_owned())?;
        let not_after = crate::der::not_after(leaf)
            .ok_or_else(|| "certificate validity unreadable".to_owned())?;
        let key = CertifiedKey::from_der(chain, key, &self.provider)
            .map_err(|error| format!("certificate and key: {error}"))?;
        Ok(Loaded {
            key: Arc::new(key),
            not_after,
        })
    }

    async fn issue(&self, client: &mut acme::Client, host: &str) -> Result<i64, String> {
        let pkcs8 = crate::der::generate_p256()?;
        let chain = client.issue(host, &pkcs8, &self.challenges).await?;
        let mut pem = pem_block("PRIVATE KEY", &pkcs8);
        pem.push_str(&chain);
        let loaded = self.certified(pem.as_bytes())?;
        let not_after = loaded.not_after;
        crate::server::write_atomically(&self.settings.cache.join(host), pem.as_bytes())
            .map_err(|error| format!("write certificate cache: {error}"))?;
        write(&self.certificates).insert(host.to_owned(), loaded);
        Ok(not_after)
    }
}

/// The account key from a PEM file: PKCS#8 (`PRIVATE KEY`) or SEC1
/// (`EC PRIVATE KEY`, what autocert writes).
fn account_key(pem: &[u8]) -> Option<Vec<u8>> {
    match PrivateKeyDer::from_pem_slice(pem).ok()? {
        PrivateKeyDer::Pkcs8(key) => Some(key.secret_pkcs8_der().to_vec()),
        PrivateKeyDer::Sec1(key) => Some(
            PrivatePkcs8KeyDer::from(crate::der::sec1_to_pkcs8(key.secret_sec1_der()))
                .secret_pkcs8_der()
                .to_vec(),
        ),
        _ => None,
    }
}

fn pem_block(label: &str, der: &[u8]) -> String {
    use base64::Engine as _;
    let encoded = base64::engine::general_purpose::STANDARD.encode(der);
    let mut pem = format!("-----BEGIN {label}-----\n");
    for chunk in encoded.as_bytes().chunks(64) {
        pem.push_str(std::str::from_utf8(chunk).unwrap_or_default());
        pem.push('\n');
    }
    pem.push_str(&format!("-----END {label}-----\n"));
    pem
}

#[derive(Debug)]
struct Resolver {
    manager: Arc<Manager>,
}

impl std::fmt::Debug for Manager {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Manager")
    }
}

impl ResolvesServerCert for Resolver {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        // No SNI, a host that is not a TLS service, or no certificate yet:
        // the handshake fails.
        let host = hello.server_name()?.to_ascii_lowercase();
        if !self.manager.server.tls_host(&host) {
            return None;
        }
        read(&self.manager.certificates)
            .get(&host)
            .map(|loaded| Arc::clone(&loaded.key))
    }
}

/// Completes TLS handshakes on accepted connections.
pub struct TlsAccept {
    acceptor: tokio_rustls::TlsAcceptor,
}

impl std::fmt::Debug for TlsAccept {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("TlsAccept")
    }
}

impl Accept for TlsAccept {
    type Stream = tokio_rustls::server::TlsStream<tokio::net::TcpStream>;

    async fn accept(&self, stream: tokio::net::TcpStream) -> Option<Self::Stream> {
        self.acceptor.accept(stream).await.ok()
    }

    fn tls(&self) -> bool {
        true
    }
}

impl Tls {
    /// Installs a certificate for `host` directly (tests).
    ///
    /// # Errors
    ///
    /// The PEM does not hold a matching key and chain.
    #[doc(hidden)]
    pub fn install(&self, host: &str, pem: &[u8]) -> Result<(), String> {
        let loaded = self.manager.certified(pem)?;
        write(&self.manager.certificates).insert(host.to_owned(), loaded);
        Ok(())
    }

    /// Where the certificate of `host` is cached (tests).
    #[doc(hidden)]
    #[must_use]
    pub fn cache_path(&self, host: &str) -> PathBuf {
        self.manager.settings.cache.join(host)
    }
}

/// Whether `path` is a challenge path, returning the token.
pub(crate) fn challenge_token(path: &str) -> Option<&str> {
    path.strip_prefix("/.well-known/acme-challenge/")
        .filter(|token| !token.is_empty() && !token.contains('/'))
}
