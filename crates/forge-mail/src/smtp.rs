//! The SMTP sender: a small, Forge-owned SMTP client over Tokio and rustls
//! (ADR 0035).
//!
//! It speaks only what submission needs (EHLO/HELO, STARTTLS, AUTH PLAIN and
//! LOGIN, MAIL, RCPT, DATA, QUIT), opens one connection per message, and is
//! secure by construction:
//!
//! - `smtp://` requires STARTTLS and `smtps://` uses implicit TLS; a server
//!   that does not advertise STARTTLS is refused before AUTH, MAIL or DATA,
//!   so a stripped advertisement cannot downgrade the connection;
//! - certificates and host names are always verified (TLS 1.2 or 1.3, the
//!   Mozilla roots plus `FORGE_MAIL_TLS_CA_FILE`); there is no switch to
//!   skip verification;
//! - credentials are produced only on a TLS connection, or in clear to a
//!   loopback host with `allow_plaintext` (a local Mailpit);
//! - bytes the server sends ahead of the TLS handshake after `STARTTLS` are
//!   refused (the STARTTLS command-injection class of bugs);
//! - every reply line and reply is bounded, and the whole delivery runs
//!   under one deadline; dropping the future closes the connection;
//! - errors carry the stage and numeric reply code only: server text echoes
//!   recipients and sometimes credentials.

use std::{
    collections::HashMap,
    io,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use forge_config::{SecretString, is_loopback_host, valid_mail_domain};
use rustls::pki_types::{CertificateDer, ServerName, pem::PemObject};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, ReadBuf},
    net::TcpStream,
};
use tokio_rustls::{TlsConnector, client::TlsStream};

use crate::{Encoded, MailError, Message, Sender, encode};

const DEFAULT_SUBMISSION_PORT: u16 = 587;
const DEFAULT_SMTPS_PORT: u16 = 465;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_TIMEOUT: Duration = Duration::from_secs(600);
/// Longest reply line read (RFC 5321 allows 512 octets; servers exceed it).
const MAX_REPLY_LINE: usize = 2048;
/// Most lines one multi-line reply may have (an EHLO lists extensions).
const MAX_REPLY_LINES: usize = 128;
/// Most rounds of an AUTH exchange.
const MAX_AUTH_ROUNDS: usize = 4;

/// Configures an [`SmtpSender`].
#[derive(Clone, Debug, Default)]
pub struct SmtpConfig {
    /// The server's DNS name or IP address; the certificate must be valid
    /// for it.
    pub host: String,
    /// Port; `0` means 587, or 465 with `implicit_tls`.
    pub port: u16,
    /// TLS from the first byte (`smtps://`, port 465) instead of STARTTLS.
    pub implicit_tls: bool,
    /// Enables AUTH (PLAIN, or LOGIN when PLAIN is not offered).
    pub username: Option<String>,
    /// The password; never logged or returned in errors.
    pub password: Option<SecretString>,
    /// Announced in EHLO; defaults to `localhost`.
    pub hello_name: Option<String>,
    /// PEM certificates trusted in addition to the Mozilla roots, for a
    /// private relay (`FORGE_MAIL_TLS_CA_FILE`).
    pub extra_roots_pem: Option<Vec<u8>>,
    /// Lets a send proceed when the server does not offer STARTTLS (a
    /// trusted network, a local development server). It never allows
    /// credentials in clear to a non-loopback host, and when STARTTLS is
    /// offered it is still used and verified.
    pub allow_plaintext: bool,
    /// Bounds one whole delivery; `ZERO` means 30 s, at most 10 min.
    pub timeout: Duration,
    /// Bounds the TCP connect; `ZERO` means 10 s.
    pub connect_timeout: Duration,
}

/// Delivers mail over SMTP, one connection per message (pooling is not
/// offered: a stale pooled connection is the usual source of lost or
/// duplicated mail; put a queue in front for volume). Safe for concurrent
/// use.
pub struct SmtpSender {
    host: String,
    port: u16,
    implicit_tls: bool,
    credentials: Option<(String, SecretString)>,
    hello_name: String,
    allow_plaintext: bool,
    timeout: Duration,
    connect_timeout: Duration,
    tls: TlsConnector,
    /// Connect here instead of resolving the host (tests: a local fake
    /// server presented under another name).
    #[cfg(test)]
    connect_to: Option<std::net::SocketAddr>,
}

impl std::fmt::Debug for SmtpSender {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SmtpSender")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("implicit_tls", &self.implicit_tls)
            .field("authenticates", &self.credentials.is_some())
            .field("allow_plaintext", &self.allow_plaintext)
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

fn configuration(message: &str) -> MailError {
    MailError::Configuration(message.to_owned())
}

impl SmtpSender {
    /// Validates `config` and returns a sender. It performs no network I/O.
    pub fn new(config: SmtpConfig) -> Result<Self, MailError> {
        if config.host.is_empty()
            || config.host.chars().any(|character| {
                character.is_whitespace()
                    || character.is_control()
                    || matches!(character, '/' | '@')
            })
        {
            return Err(configuration(
                "SMTP host is required and must be a bare host name or IP",
            ));
        }
        let port = match config.port {
            0 if config.implicit_tls => DEFAULT_SMTPS_PORT,
            0 => DEFAULT_SUBMISSION_PORT,
            port => port,
        };
        let credentials = match (config.username, config.password) {
            (Some(username), Some(password)) if !username.is_empty() => {
                if username.chars().any(char::is_control)
                    || password.expose().chars().any(char::is_control)
                {
                    return Err(configuration("SMTP credentials contain control characters"));
                }
                Some((username, password))
            }
            (None | Some(_), Some(_)) => {
                return Err(configuration("SMTP password requires a username"));
            }
            (Some(username), None) if !username.is_empty() => {
                return Err(configuration("SMTP username requires a password"));
            }
            _ => None,
        };
        let hello_name = config.hello_name.unwrap_or_else(|| "localhost".to_owned());
        let hello_ip = hello_name
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::IpAddr>()
            .is_ok();
        if !valid_mail_domain(&hello_name) && !hello_ip {
            return Err(configuration("SMTP hello name is not a valid host name"));
        }
        let timeout = if config.timeout.is_zero() {
            DEFAULT_TIMEOUT
        } else {
            config.timeout
        };
        let connect_timeout = if config.connect_timeout.is_zero() {
            DEFAULT_CONNECT_TIMEOUT
        } else {
            config.connect_timeout
        };
        if timeout > MAX_TIMEOUT || connect_timeout > MAX_TIMEOUT {
            return Err(configuration("SMTP timeouts must be at most 10 minutes"));
        }
        Ok(Self {
            host: config
                .host
                .trim_start_matches('[')
                .trim_end_matches(']')
                .to_owned(),
            port,
            implicit_tls: config.implicit_tls,
            credentials,
            hello_name,
            allow_plaintext: config.allow_plaintext,
            timeout,
            connect_timeout,
            tls: tls_connector(config.extra_roots_pem.as_deref())?,
            #[cfg(test)]
            connect_to: None,
        })
    }

    /// Delivers a message `encode` produced (never a hand-built
    /// [`Encoded`], whose fields are not validated). Delivery is
    /// all-or-nothing at the envelope: a refused recipient aborts the
    /// transaction before `DATA` ([`MailError::Rejected`]), so no subset
    /// receives it.
    async fn deliver(&self, encoded: &Encoded) -> Result<(), MailError> {
        let mut stage = "connect";
        match tokio::time::timeout(self.timeout, self.transaction(encoded, &mut stage)).await {
            Ok(result) => result,
            // The server accepted the message; only QUIT was pending.
            Err(_) if stage == "quit" => Ok(()),
            Err(_) => Err(MailError::Unavailable {
                stage,
                reply: None,
                timeout: true,
            }),
        }
    }

    async fn transaction(
        &self,
        encoded: &Encoded,
        stage: &mut &'static str,
    ) -> Result<(), MailError> {
        *stage = "connect";
        let tcp = self.connect().await?;
        let connection = if self.implicit_tls {
            Connection::Tls(Box::new(self.handshake(tcp, "connect").await?))
        } else {
            Connection::Plain(tcp)
        };
        let mut session = Session {
            stream: BufReader::new(connection),
            secure: self.implicit_tls,
        };

        *stage = "greeting";
        session.expect("greeting", 220).await?;
        *stage = "ehlo";
        let mut extensions = session.hello(&self.hello_name).await?;

        if !self.implicit_tls {
            *stage = "starttls";
            if extensions.contains_key("STARTTLS") {
                session.command("starttls", "STARTTLS").await?;
                session.expect("starttls", 220).await?;
                if !session.stream.buffer().is_empty() {
                    // Anything sent before the handshake would be read as if
                    // it came over TLS: refuse it (CVE-2011-0411 class).
                    return Err(MailError::TlsFailed { stage: "starttls" });
                }
                let Connection::Plain(tcp) = session.stream.into_inner() else {
                    return Err(MailError::TlsFailed { stage: "starttls" });
                };
                let tls = self.handshake(tcp, "starttls").await?;
                session = Session {
                    stream: BufReader::new(Connection::Tls(Box::new(tls))),
                    secure: true,
                };
                extensions = session.hello(&self.hello_name).await?;
            } else if !self.allow_plaintext {
                return Err(MailError::TlsRequired);
            }
        }

        if let Some((username, password)) = &self.credentials {
            *stage = "auth";
            // Credentials leave the machine only over verified TLS.
            // `allow_plaintext` relaxes that for the mail, never for the
            // password, except to a loopback relay.
            let clear_ok = self.allow_plaintext && is_loopback_host(&self.host);
            if !session.secure && !clear_ok {
                return Err(MailError::AuthFailed {
                    stage: "auth",
                    reply: None,
                });
            }
            session
                .authenticate(&extensions, username, password)
                .await?;
        }

        *stage = "mail";
        session
            .command("mail", &format!("MAIL FROM:<{}>", encoded.sender))
            .await?;
        session.expect("mail", 250).await?;
        *stage = "rcpt";
        for recipient in &encoded.recipients {
            session
                .command("rcpt", &format!("RCPT TO:<{recipient}>"))
                .await?;
            let reply = session.reply("rcpt").await?;
            if reply.code != 250 && reply.code != 251 {
                return Err(classify_reply("rcpt", reply.code));
            }
        }
        *stage = "data";
        session.command("data", "DATA").await?;
        session.expect("data", 354).await?;
        session.write("data", &dot_stuff(&encoded.data)).await?;
        session.expect("data", 250).await?;
        // The message is accepted: a failing QUIT must not turn success
        // into an error that would cause a duplicate on retry.
        *stage = "quit";
        if session.command("quit", "QUIT").await.is_ok() {
            let _ = session.reply("quit").await;
        }
        Ok(())
    }

    async fn connect(&self) -> Result<TcpStream, MailError> {
        let unavailable = |timeout| MailError::Unavailable {
            stage: "connect",
            reply: None,
            timeout,
        };
        #[cfg(test)]
        if let Some(address) = self.connect_to {
            return tokio::time::timeout(self.connect_timeout, TcpStream::connect(address))
                .await
                .map_err(|_| unavailable(true))?
                .map_err(|_| unavailable(false));
        }
        tokio::time::timeout(
            self.connect_timeout,
            TcpStream::connect((self.host.as_str(), self.port)),
        )
        .await
        .map_err(|_| unavailable(true))?
        .map_err(|_| unavailable(false))
    }

    async fn handshake(
        &self,
        tcp: TcpStream,
        stage: &'static str,
    ) -> Result<TlsStream<TcpStream>, MailError> {
        let name =
            ServerName::try_from(self.host.clone()).map_err(|_| MailError::TlsFailed { stage })?;
        self.tls
            .connect(name, tcp)
            .await
            .map_err(|_| MailError::TlsFailed { stage })
    }
}

fn tls_connector(extra_roots_pem: Option<&[u8]>) -> Result<TlsConnector, MailError> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    if let Some(pem) = extra_roots_pem {
        let mut added = 0;
        for certificate in CertificateDer::pem_slice_iter(pem) {
            let certificate = certificate
                .map_err(|_| configuration("FORGE_MAIL_TLS_CA_FILE is not a valid PEM bundle"))?;
            roots.add(certificate).map_err(|_| {
                configuration("FORGE_MAIL_TLS_CA_FILE has a certificate that cannot be a root")
            })?;
            added += 1;
        }
        if added == 0 {
            return Err(configuration(
                "FORGE_MAIL_TLS_CA_FILE contains no PEM certificates",
            ));
        }
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
        .map_err(|_| configuration("TLS configuration failed"))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(TlsConnector::from(Arc::new(config)))
}

#[async_trait]
impl Sender for SmtpSender {
    /// Validates and encodes `message` before connecting, so an invalid
    /// message never causes network I/O, then delivers it.
    async fn send(&self, message: &Message) -> Result<(), MailError> {
        let encoded = encode(message)?;
        self.deliver(&encoded).await
    }
}

/// Maps a reply code to an error, Forge-Go's rules: 530 and 535 at any
/// stage, and 534 or 538 during AUTH, are authentication failures; other
/// 4xx are transient and everything else a rejection.
fn classify_reply(stage: &'static str, code: u16) -> MailError {
    let reply = Some(code);
    if code == 530 || code == 535 || (stage == "auth" && (code == 534 || code == 538)) {
        MailError::AuthFailed { stage, reply }
    } else if (400..500).contains(&code) {
        MailError::Unavailable {
            stage,
            reply,
            timeout: false,
        }
    } else {
        MailError::Rejected { stage, reply }
    }
}

/// `data` with every line that starts with `.` doubled and the terminating
/// `CRLF . CRLF` (RFC 5321 section 4.5.2).
fn dot_stuff(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + data.len() / 64 + 5);
    let mut line_start = true;
    for &byte in data {
        if line_start && byte == b'.' {
            out.push(b'.');
        }
        out.push(byte);
        line_start = byte == b'\n';
    }
    if !out.ends_with(b"\r\n") {
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b".\r\n");
    out
}

struct Reply {
    code: u16,
    lines: Vec<String>,
}

struct Session {
    stream: BufReader<Connection>,
    secure: bool,
}

impl Session {
    fn unavailable(stage: &'static str) -> MailError {
        MailError::Unavailable {
            stage,
            reply: None,
            timeout: false,
        }
    }

    async fn write(&mut self, stage: &'static str, bytes: &[u8]) -> Result<(), MailError> {
        let stream = self.stream.get_mut();
        stream
            .write_all(bytes)
            .await
            .map_err(|_| Self::unavailable(stage))?;
        stream.flush().await.map_err(|_| Self::unavailable(stage))
    }

    async fn command(&mut self, stage: &'static str, line: &str) -> Result<(), MailError> {
        let mut bytes = Vec::with_capacity(line.len() + 2);
        bytes.extend_from_slice(line.as_bytes());
        bytes.extend_from_slice(b"\r\n");
        self.write(stage, &bytes).await
    }

    /// Reads one bounded line, without its line end.
    async fn line(&mut self, stage: &'static str) -> Result<String, MailError> {
        let mut line = Vec::new();
        loop {
            let available = self
                .stream
                .fill_buf()
                .await
                .map_err(|_| Self::unavailable(stage))?;
            if available.is_empty() {
                return Err(Self::unavailable(stage));
            }
            let (taken, done) = match available.iter().position(|&byte| byte == b'\n') {
                Some(index) => (index + 1, true),
                None => (available.len(), false),
            };
            line.extend_from_slice(&available[..taken]);
            self.stream.consume(taken);
            if line.len() > MAX_REPLY_LINE {
                return Err(Self::unavailable(stage));
            }
            if done {
                break;
            }
        }
        while line
            .last()
            .is_some_and(|byte| *byte == b'\n' || *byte == b'\r')
        {
            line.pop();
        }
        String::from_utf8(line).map_err(|_| Self::unavailable(stage))
    }

    /// Reads one (possibly multi-line) reply. Malformed or oversized
    /// replies are transient protocol failures.
    async fn reply(&mut self, stage: &'static str) -> Result<Reply, MailError> {
        let mut lines = Vec::new();
        let mut code = None;
        loop {
            let line = self.line(stage).await?;
            let bytes = line.as_bytes();
            let valid = bytes.len() >= 3
                && bytes[..3].iter().all(u8::is_ascii_digit)
                && (bytes.len() == 3 || bytes[3] == b' ' || bytes[3] == b'-');
            if !valid {
                return Err(Self::unavailable(stage));
            }
            let this: u16 = line[..3].parse().map_err(|_| Self::unavailable(stage))?;
            if !(200..600).contains(&this) || code.is_some_and(|code| code != this) {
                return Err(Self::unavailable(stage));
            }
            code = Some(this);
            let last = bytes.len() == 3 || bytes[3] == b' ';
            lines.push(line.get(4..).unwrap_or_default().to_owned());
            if last {
                break;
            }
            if lines.len() >= MAX_REPLY_LINES {
                return Err(Self::unavailable(stage));
            }
        }
        Ok(Reply {
            code: code.unwrap_or_default(),
            lines,
        })
    }

    async fn expect(&mut self, stage: &'static str, expected: u16) -> Result<Reply, MailError> {
        let reply = self.reply(stage).await?;
        if reply.code == expected {
            Ok(reply)
        } else {
            Err(classify_reply(stage, reply.code))
        }
    }

    /// EHLO, falling back to HELO (no extensions) when EHLO is refused.
    async fn hello(&mut self, name: &str) -> Result<HashMap<String, String>, MailError> {
        let stage = "ehlo";
        self.command(stage, &format!("EHLO {name}")).await?;
        let reply = self.reply(stage).await?;
        if reply.code == 250 {
            let mut extensions: HashMap<String, String> = HashMap::new();
            for line in reply.lines.iter().skip(1) {
                let (key, value) = line.split_once(' ').unwrap_or((line, ""));
                let key = key.to_ascii_uppercase();
                match key.strip_prefix("AUTH=") {
                    // The legacy `250-AUTH=PLAIN LOGIN` advertisement (old
                    // Outlook Express era servers, still seen in relays)
                    // lists the same mechanisms as `AUTH`; merge both.
                    Some(first) => {
                        let merged = extensions.entry("AUTH".to_owned()).or_default();
                        for mechanism in std::iter::once(first).chain(value.split_whitespace()) {
                            merged.push(' ');
                            merged.push_str(mechanism);
                        }
                    }
                    None if key == "AUTH" => {
                        let merged = extensions.entry(key).or_default();
                        merged.push(' ');
                        merged.push_str(value);
                    }
                    None => {
                        extensions.insert(key, value.to_owned());
                    }
                }
            }
            return Ok(extensions);
        }
        if !(500..600).contains(&reply.code) {
            return Err(classify_reply(stage, reply.code));
        }
        self.command(stage, &format!("HELO {name}")).await?;
        self.expect(stage, 250).await?;
        Ok(HashMap::new())
    }

    async fn authenticate(
        &mut self,
        extensions: &HashMap<String, String>,
        username: &str,
        password: &SecretString,
    ) -> Result<(), MailError> {
        let stage = "auth";
        let refused = MailError::AuthFailed { stage, reply: None };
        let mechanisms: Vec<String> = extensions
            .get("AUTH")
            .map(|value| {
                value
                    .split_whitespace()
                    .map(str::to_ascii_uppercase)
                    .collect()
            })
            .unwrap_or_default();
        if mechanisms.iter().any(|mechanism| mechanism == "PLAIN") {
            let token = STANDARD.encode(format!("\0{username}\0{}", password.expose()));
            self.command(stage, &format!("AUTH PLAIN {token}")).await?;
            let reply = self.reply(stage).await?;
            return match reply.code {
                235 => Ok(()),
                334 => {
                    // PLAIN expects no challenge: cancel the exchange.
                    let _ = self.command(stage, "*").await;
                    Err(refused)
                }
                code => Err(classify_reply(stage, code)),
            };
        }
        if !mechanisms.iter().any(|mechanism| mechanism == "LOGIN") {
            return Err(refused);
        }
        self.command(stage, "AUTH LOGIN").await?;
        for _ in 0..MAX_AUTH_ROUNDS {
            let reply = self.reply(stage).await?;
            match reply.code {
                235 => return Ok(()),
                334 => {
                    let raw = reply.lines.first().map(String::as_str).unwrap_or_default();
                    let prompt = STANDARD
                        .decode(raw.trim())
                        .ok()
                        .and_then(|bytes| String::from_utf8(bytes).ok())
                        .unwrap_or_else(|| raw.to_owned())
                        .trim()
                        .to_ascii_lowercase();
                    let answer = if prompt.starts_with("username") {
                        username
                    } else if prompt.starts_with("password") {
                        password.expose()
                    } else {
                        let _ = self.command(stage, "*").await;
                        return Err(refused);
                    };
                    self.command(stage, &STANDARD.encode(answer)).await?;
                }
                code => return Err(classify_reply(stage, code)),
            }
        }
        Err(refused)
    }
}

/// A plain or TLS connection.
enum Connection {
    Plain(TcpStream),
    Tls(Box<TlsStream<TcpStream>>),
}

impl AsyncRead for Connection {
    fn poll_read(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_read(context, buffer),
            Self::Tls(stream) => Pin::new(stream.as_mut()).poll_read(context, buffer),
        }
    }
}

impl AsyncWrite for Connection {
    fn poll_write(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_write(context, buffer),
            Self::Tls(stream) => Pin::new(stream.as_mut()).poll_write(context, buffer),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_flush(context),
            Self::Tls(stream) => Pin::new(stream.as_mut()).poll_flush(context),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_shutdown(context),
            Self::Tls(stream) => Pin::new(stream.as_mut()).poll_shutdown(context),
        }
    }
}

#[cfg(test)]
#[path = "smtp_tests.rs"]
mod tests;
