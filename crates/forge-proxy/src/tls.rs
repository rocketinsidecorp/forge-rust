//! HTTPS: certificates for exactly the hosts of services deployed with
//! `tls`, obtained from an ACME CA (Let's Encrypt by default) and served
//! with rustls (TLS 1.2 and 1.3, `ring` provider).
//!
//! Certificates are requested ahead of time — when a TLS service is
//! deployed and when one has less than 30 days left — by one owned renewal
//! loop, never during a handshake. A handshake for a host that is not a TLS
//! service, or has no certificate yet, fails: nothing is served with a
//! wrong or self-signed certificate. A cached or issued certificate is used
//! only when its key matches and its subject alternative names cover the
//! host. Cache files are read and written on the blocking pool.
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
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
use rustls::server::{ClientHello, ParsedCertificate, ResolvesServerCert};
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
#[derive(Clone, Debug, Default)]
pub struct AcmeSettings {
    /// Cache directory (created 0700; an existing one is restricted to
    /// 0700).
    pub cache: PathBuf,
    /// Contact address registered with the CA.
    pub email: String,
    /// Directory URL; empty is Let's Encrypt production.
    pub directory: String,
    /// Extra trust anchors for the directory's HTTPS certificate, on top of
    /// the webpki roots (a private or test CA such as Pebble's). `forge-proxy
    /// run` has no flag for it and passes none.
    pub directory_roots: Vec<CertificateDer<'static>>,
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
    /// The cache directory cannot be created or restricted to 0700 (or is
    /// a link), or the directory URL is not usable (plain HTTP is refused
    /// except on loopback).
    pub fn new(server: Server, mut settings: AcmeSettings) -> Result<Self, String> {
        if settings.directory.is_empty() {
            acme::LETS_ENCRYPT.clone_into(&mut settings.directory);
        }
        crate::files::private_directory(&settings.cache)
            .map_err(|error| format!("ACME cache: {error}"))?;
        // Validates the URL now: fail at start, not at the first deploy.
        acme::Client::with_roots(
            &settings.directory,
            &crate::der::generate_p256()?,
            &settings.email,
            &settings.directory_roots,
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
    /// Cache reads and writes run on the blocking pool and are awaited to
    /// completion, so no file operation outlives this call.
    pub async fn run(&self, stop: CancellationToken) {
        let manager = &self.manager;
        let mut client: Option<acme::Client> = None;
        loop {
            let mut wake = Instant::now() + CHECK_INTERVAL;
            for host in manager.server.tls_hosts() {
                if stop.is_cancelled() {
                    return;
                }
                let fresh = {
                    let host = host.clone();
                    blocking(manager, move |manager| manager.fresh(&host))
                        .await
                        .unwrap_or(false)
                };
                if fresh {
                    continue;
                }
                if let Some((next, _)) = lock(&manager.failures).get(&host)
                    && *next > Instant::now()
                {
                    wake = wake.min(*next);
                    continue;
                }
                let Some(outcome) = manager.obtain(&mut client, &host, &stop).await else {
                    return;
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

/// Runs blocking file work for `manager` on the blocking pool and waits for
/// it.
async fn blocking<T, F>(manager: &Arc<Manager>, work: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce(&Manager) -> T + Send + 'static,
{
    let manager = Arc::clone(manager);
    tokio::task::spawn_blocking(move || work(&manager))
        .await
        .map_err(|error| format!("certificate cache: {error}"))
}

impl Manager {
    /// Obtains and caches a certificate for `host`; `None` when `stop` was
    /// cancelled first. Only the exchange with the CA is bounded by
    /// [`ISSUE_BUDGET`] and cut short by `stop`; the cache file operations
    /// around it always run to completion.
    async fn obtain(
        self: &Arc<Self>,
        client: &mut Option<acme::Client>,
        host: &str,
        stop: &CancellationToken,
    ) -> Option<Result<i64, String>> {
        if client.is_none() {
            match blocking(self, Self::client).await.and_then(|result| result) {
                Ok(created) => *client = Some(created),
                Err(error) => return Some(Err(error)),
            }
        }
        let Some(client) = client.as_mut() else {
            return Some(Err("acme client unavailable".to_owned()));
        };
        let pkcs8 = match crate::der::generate_p256() {
            Ok(pkcs8) => pkcs8,
            Err(error) => return Some(Err(error)),
        };
        let exchange = client.issue(host, &pkcs8, &self.challenges);
        let chain = tokio::select! {
            chain = tokio::time::timeout(ISSUE_BUDGET, exchange) => {
                chain.unwrap_or_else(|_| Err("certificate issuance timed out".to_owned()))
            }
            () = stop.cancelled() => return None,
        };
        let chain = match chain {
            Ok(chain) => chain,
            Err(error) => return Some(Err(error)),
        };
        let host = host.to_owned();
        Some(
            blocking(self, move |manager| manager.store(&host, &pkcs8, &chain))
                .await
                .and_then(|result| result),
        )
    }

    fn client(&self) -> Result<acme::Client, String> {
        let path = self.settings.cache.join(ACCOUNT_FILE);
        let pkcs8 = match std::fs::read(&path) {
            Ok(pem) => account_key(&pem)
                .ok_or_else(|| format!("{} is not a P-256 private key", path.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let pkcs8 = crate::der::generate_p256()?;
                let pem = pem_block("PRIVATE KEY", &pkcs8);
                crate::files::write_atomically(&path, pem.as_bytes())
                    .map_err(|error| format!("write {}: {error}", path.display()))?;
                pkcs8
            }
            Err(error) => return Err(format!("read {}: {error}", path.display())),
        };
        acme::Client::with_roots(
            &self.settings.directory,
            &pkcs8,
            &self.settings.email,
            &self.settings.directory_roots,
        )
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
        match self.certified(host, &pem) {
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

    /// A key and chain from PEM for `host`, checked to belong together and
    /// to name `host` among the leaf's subject alternative names (the
    /// common name is not consulted, as by browsers).
    fn certified(&self, host: &str, pem: &[u8]) -> Result<Loaded, String> {
        let key =
            PrivateKeyDer::from_pem_slice(pem).map_err(|error| format!("private key: {error}"))?;
        let chain: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(pem)
            .collect::<Result<_, _>>()
            .map_err(|error| format!("certificate: {error}"))?;
        let leaf = chain.first().ok_or_else(|| "no certificate".to_owned())?;
        let name =
            ServerName::try_from(host).map_err(|_| format!("{host:?} is not a valid host name"))?;
        let parsed =
            ParsedCertificate::try_from(leaf).map_err(|error| format!("certificate: {error}"))?;
        rustls::client::verify_server_name(&parsed, &name)
            .map_err(|_| format!("certificate is not valid for {host}"))?;
        let not_after = crate::der::not_after(leaf)
            .ok_or_else(|| "certificate validity unreadable".to_owned())?;
        let key = CertifiedKey::from_der(chain, key, &self.provider)
            .map_err(|error| format!("certificate and key: {error}"))?;
        Ok(Loaded {
            key: Arc::new(key),
            not_after,
        })
    }

    /// Checks the issued chain against its key and `host`, caches it and
    /// serves it. Blocks.
    fn store(&self, host: &str, pkcs8: &[u8], chain: &str) -> Result<i64, String> {
        let mut pem = pem_block("PRIVATE KEY", pkcs8);
        pem.push_str(chain);
        let loaded = self.certified(host, pem.as_bytes())?;
        let not_after = loaded.not_after;
        crate::files::write_atomically(&self.settings.cache.join(host), pem.as_bytes())
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
    /// The PEM does not hold a matching key and chain for `host`.
    #[doc(hidden)]
    pub fn install(&self, host: &str, pem: &[u8]) -> Result<(), String> {
        let loaded = self.manager.certified(host, pem)?;
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
