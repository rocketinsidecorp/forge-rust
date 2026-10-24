//! SMTP sender tests against an in-process fake server.
//!
//! `tests/fixtures/tls` holds a throwaway test CA and a `localhost` /
//! `127.0.0.1` server certificate (P-256, valid until 2126) generated with
//! `openssl` for these tests only. The key protects nothing.

use std::{net::SocketAddr, sync::Arc};

use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
    task::JoinHandle,
};
use tokio_rustls::TlsAcceptor;

use super::*;
use crate::Address;

const CA: &[u8] = include_bytes!("../tests/fixtures/tls/ca.pem");
const SERVER_CERT: &[u8] = include_bytes!("../tests/fixtures/tls/server.pem");
const SERVER_KEY: &[u8] = include_bytes!("../tests/fixtures/tls/server.key");
const PASSWORD: &str = "s3cr3t-Pa55word";

/// What the fake server advertises and answers.
#[derive(Clone)]
struct Script {
    implicit_tls: bool,
    starttls: bool,
    auth: &'static str,
    /// Advertise the mechanisms as the legacy `AUTH=...` line only.
    legacy_auth: bool,
    auth_reply: u16,
    ehlo_reply: u16,
    rcpt_refused: Option<(&'static str, u16)>,
    mail_reply: u16,
    data_reply: u16,
    /// Bytes sent right after the `220` to STARTTLS, before the handshake.
    inject_after_starttls: bool,
    /// Never answer the greeting.
    silent: bool,
    /// Close instead of answering QUIT.
    drop_on_quit: bool,
    /// Never answer QUIT.
    hang_on_quit: bool,
    /// Echo something sensitive in reply text.
    echo: bool,
}

impl Default for Script {
    fn default() -> Self {
        Self {
            implicit_tls: false,
            starttls: true,
            auth: "PLAIN LOGIN",
            legacy_auth: false,
            auth_reply: 235,
            ehlo_reply: 250,
            rcpt_refused: None,
            mail_reply: 250,
            data_reply: 250,
            inject_after_starttls: false,
            silent: false,
            drop_on_quit: false,
            hang_on_quit: false,
            echo: false,
        }
    }
}

#[derive(Debug, Default)]
struct Transcript {
    /// Commands as received, with whether TLS was active.
    commands: Vec<(String, bool)>,
    data: Vec<u8>,
}

impl Transcript {
    fn saw(&self, prefix: &str) -> bool {
        self.commands
            .iter()
            .any(|(line, _)| line.starts_with(prefix))
    }

    fn tls_for(&self, prefix: &str) -> Option<bool> {
        self.commands
            .iter()
            .find(|(line, _)| line.starts_with(prefix))
            .map(|(_, tls)| *tls)
    }
}

fn acceptor() -> TlsAcceptor {
    let certificates: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(SERVER_CERT)
        .collect::<Result<_, _>>()
        .expect("test certificate");
    let key = rustls::pki_types::PrivateKeyDer::from_pem_slice(SERVER_KEY).expect("test key");
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .expect("versions")
    .with_no_client_auth()
    .with_single_cert(certificates, key)
    .expect("server config");
    TlsAcceptor::from(Arc::new(config))
}

enum ServerStream {
    Plain(BufReader<tokio::net::TcpStream>),
    Tls(Box<BufReader<tokio_rustls::server::TlsStream<tokio::net::TcpStream>>>),
}

impl ServerStream {
    async fn line(&mut self) -> Option<String> {
        let mut line = String::new();
        let read = match self {
            Self::Plain(stream) => stream.read_line(&mut line).await,
            Self::Tls(stream) => stream.read_line(&mut line).await,
        };
        match read {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(line.trim_end_matches(['\r', '\n']).to_owned()),
        }
    }

    async fn send(&mut self, text: &str) {
        let bytes = format!("{text}\r\n");
        let _ = match self {
            Self::Plain(stream) => stream.get_mut().write_all(bytes.as_bytes()).await,
            Self::Tls(stream) => stream.get_mut().write_all(bytes.as_bytes()).await,
        };
    }

    fn is_tls(&self) -> bool {
        matches!(self, Self::Tls(_))
    }
}

/// Serves one connection with `script`; the handle returns what it saw.
async fn serve(script: Script) -> (SocketAddr, JoinHandle<Transcript>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    let handle = tokio::spawn(async move {
        let mut transcript = Transcript::default();
        let Ok((tcp, _)) = listener.accept().await else {
            return transcript;
        };
        let mut stream = if script.implicit_tls {
            match acceptor().accept(tcp).await {
                Ok(tls) => ServerStream::Tls(Box::new(BufReader::new(tls))),
                Err(_) => return transcript,
            }
        } else {
            ServerStream::Plain(BufReader::new(tcp))
        };
        if script.silent {
            tokio::time::sleep(Duration::from_secs(60)).await;
            return transcript;
        }
        stream.send("220 fake.example ESMTP").await;
        while let Some(line) = stream.line().await {
            transcript.commands.push((line.clone(), stream.is_tls()));
            let upper = line.to_ascii_uppercase();
            if upper.starts_with("EHLO") {
                if script.ehlo_reply != 250 {
                    stream.send(&format!("{} no", script.ehlo_reply)).await;
                    continue;
                }
                stream.send("250-fake.example").await;
                if script.starttls && !stream.is_tls() {
                    stream.send("250-STARTTLS").await;
                }
                if !script.auth.is_empty() {
                    let separator = if script.legacy_auth { '=' } else { ' ' };
                    stream
                        .send(&format!("250-AUTH{separator}{}", script.auth))
                        .await;
                }
                stream.send("250 8BITMIME").await;
            } else if upper.starts_with("HELO") {
                stream.send("250 fake.example").await;
            } else if upper == "STARTTLS" {
                if script.inject_after_starttls {
                    stream
                        .send("220 go ahead\r\n250 injected-before-handshake")
                        .await;
                } else {
                    stream.send("220 go ahead").await;
                }
                let ServerStream::Plain(plain) = stream else {
                    return transcript;
                };
                match acceptor().accept(plain.into_inner()).await {
                    Ok(tls) => stream = ServerStream::Tls(Box::new(BufReader::new(tls))),
                    Err(_) => return transcript,
                }
            } else if upper.starts_with("AUTH PLAIN") {
                stream.send(&format!("{} auth", script.auth_reply)).await;
            } else if upper.starts_with("AUTH LOGIN") {
                stream.send("334 VXNlcm5hbWU6").await;
                if stream.line().await.is_none() {
                    return transcript;
                }
                stream.send("334 UGFzc3dvcmQ6").await;
                let Some(password) = stream.line().await else {
                    return transcript;
                };
                transcript
                    .commands
                    .push((format!("LOGIN-PASSWORD {password}"), stream.is_tls()));
                stream.send(&format!("{} auth", script.auth_reply)).await;
            } else if upper.starts_with("MAIL FROM") {
                stream.send(&format!("{} ok", script.mail_reply)).await;
            } else if upper.starts_with("RCPT TO") {
                match script.rcpt_refused {
                    Some((who, code)) if line.contains(who) => {
                        let text = if script.echo {
                            format!("{code} <{who}> unknown, password {PASSWORD}")
                        } else {
                            format!("{code} no")
                        };
                        stream.send(&text).await;
                    }
                    _ => stream.send("250 ok").await,
                }
            } else if upper == "DATA" {
                stream.send("354 go").await;
                loop {
                    let Some(data_line) = stream.line().await else {
                        return transcript;
                    };
                    if data_line == "." {
                        break;
                    }
                    transcript.data.extend_from_slice(data_line.as_bytes());
                    transcript.data.extend_from_slice(b"\r\n");
                }
                stream.send(&format!("{} queued", script.data_reply)).await;
            } else if upper == "QUIT" {
                if script.hang_on_quit {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                }
                if !script.drop_on_quit {
                    stream.send("221 bye").await;
                }
                return transcript;
            } else if upper == "*" {
                stream.send("501 cancelled").await;
            } else {
                stream.send("500 what").await;
            }
        }
        transcript
    });
    (address, handle)
}

fn message() -> Message {
    Message::new(Address::new("App", "app@example.com").expect("valid"))
        .to(Address::new("Ada", "ada@example.org").expect("valid"))
        .bcc(Address::new("", "audit@example.org").expect("valid"))
        .subject("Hello")
        .text(".starts with a dot\nsecond line")
}

fn config(host: &str, port: u16) -> SmtpConfig {
    SmtpConfig {
        host: host.to_owned(),
        port,
        username: Some("mailer".to_owned()),
        password: Some(SecretString::new(PASSWORD)),
        extra_roots_pem: Some(CA.to_vec()),
        timeout: Duration::from_secs(5),
        connect_timeout: Duration::from_secs(2),
        ..SmtpConfig::default()
    }
}

/// A sender for `config` that connects to `address` whatever its host.
fn sender(config: SmtpConfig, address: SocketAddr) -> SmtpSender {
    let mut sender = SmtpSender::new(config).expect("valid config");
    sender.connect_to = Some(address);
    sender
}

fn secret_free(error: &MailError) {
    let text = format!("{error} {error:?}");
    assert!(!text.contains(PASSWORD), "{text}");
    assert!(!text.contains("ada@"), "{text}");
    assert!(!text.contains("unknown"), "{text}");
}

#[tokio::test]
async fn starttls_then_auth_then_delivery_with_bcc_in_the_envelope_only() {
    let (address, server) = serve(Script::default()).await;
    sender(config("localhost", address.port()), address)
        .send(&message())
        .await
        .expect("delivered");
    let transcript = server.await.expect("server");
    assert_eq!(transcript.tls_for("STARTTLS"), Some(false));
    assert_eq!(transcript.tls_for("AUTH PLAIN"), Some(true));
    assert_eq!(
        transcript.tls_for("MAIL FROM:<app@example.com>"),
        Some(true)
    );
    assert!(transcript.saw("RCPT TO:<ada@example.org>"));
    assert!(transcript.saw("RCPT TO:<audit@example.org>"));
    let data = String::from_utf8(transcript.data.clone()).expect("ascii");
    assert!(data.contains("\r\n..starts with a dot"), "{data}");
    assert!(!data.to_ascii_lowercase().contains("audit@"), "{data}");
    assert!(transcript.saw("QUIT"));
}

#[tokio::test]
async fn implicit_tls_with_login_when_plain_is_not_offered() {
    let (address, server) = serve(Script {
        implicit_tls: true,
        starttls: false,
        auth: "LOGIN",
        ..Script::default()
    })
    .await;
    let mut smtps = config("127.0.0.1", address.port());
    smtps.implicit_tls = true;
    sender(smtps, address)
        .send(&message())
        .await
        .expect("delivered");
    let transcript = server.await.expect("server");
    assert_eq!(transcript.tls_for("EHLO"), Some(true));
    let expected = format!("LOGIN-PASSWORD {}", STANDARD.encode(PASSWORD));
    assert_eq!(transcript.tls_for(&expected), Some(true));
}

#[tokio::test]
async fn the_legacy_auth_equals_advertisement_is_recognised() {
    for auth in ["PLAIN LOGIN", "LOGIN"] {
        let (address, server) = serve(Script {
            auth,
            legacy_auth: true,
            ..Script::default()
        })
        .await;
        sender(config("localhost", address.port()), address)
            .send(&message())
            .await
            .expect("delivered");
        let transcript = server.await.expect("server");
        let method = if auth == "LOGIN" {
            "AUTH LOGIN"
        } else {
            "AUTH PLAIN"
        };
        assert_eq!(transcript.tls_for(method), Some(true), "{auth}");
    }
}

#[tokio::test]
async fn an_unsupported_legacy_advertisement_still_fails_closed() {
    let (address, server) = serve(Script {
        auth: "CRAM-MD5",
        legacy_auth: true,
        ..Script::default()
    })
    .await;
    let error = sender(config("localhost", address.port()), address)
        .send(&message())
        .await
        .expect_err("no mechanism");
    assert_eq!(
        error,
        MailError::AuthFailed {
            stage: "auth",
            reply: None
        }
    );
    assert!(!server.await.expect("server").saw("MAIL"));
}

#[tokio::test]
async fn a_server_without_starttls_is_refused_before_auth_or_mail() {
    let (address, server) = serve(Script {
        starttls: false,
        ..Script::default()
    })
    .await;
    let error = sender(config("localhost", address.port()), address)
        .send(&message())
        .await
        .expect_err("refused");
    assert_eq!(error, MailError::TlsRequired);
    let transcript = server.await.expect("server");
    assert!(!transcript.saw("AUTH"));
    assert!(!transcript.saw("MAIL"));
}

#[tokio::test]
async fn plaintext_never_carries_credentials_to_a_remote_host() {
    // A stripped STARTTLS advertisement towards a non-loopback name.
    let (address, server) = serve(Script {
        starttls: false,
        ..Script::default()
    })
    .await;
    let mut remote = config("smtp.example.com", address.port());
    remote.allow_plaintext = true;
    let error = sender(remote, address)
        .send(&message())
        .await
        .expect_err("refused");
    assert_eq!(
        error,
        MailError::AuthFailed {
            stage: "auth",
            reply: None
        }
    );
    let transcript = server.await.expect("server");
    assert!(!transcript.saw("AUTH"), "{:?}", transcript.commands);
    assert!(!transcript.saw("MAIL"));
}

#[tokio::test]
async fn plaintext_to_loopback_is_allowed_only_with_the_flag() {
    let (address, server) = serve(Script {
        starttls: false,
        ..Script::default()
    })
    .await;
    let mut local = config("127.0.0.1", address.port());
    local.allow_plaintext = true;
    sender(local, address)
        .send(&message())
        .await
        .expect("delivered");
    let transcript = server.await.expect("server");
    assert_eq!(transcript.tls_for("AUTH PLAIN"), Some(false));

    // Without credentials, the flag permits a trusted relay in clear.
    let (address, server) = serve(Script {
        starttls: false,
        auth: "",
        ..Script::default()
    })
    .await;
    let mut anonymous = config("relay.internal", address.port());
    anonymous.username = None;
    anonymous.password = None;
    anonymous.allow_plaintext = true;
    sender(anonymous, address)
        .send(&message())
        .await
        .expect("delivered");
    assert!(server.await.expect("server").saw("MAIL FROM"));
}

#[tokio::test]
async fn offered_starttls_is_used_even_with_the_plaintext_flag() {
    let (address, server) = serve(Script::default()).await;
    let mut local = config("localhost", address.port());
    local.allow_plaintext = true;
    sender(local, address)
        .send(&message())
        .await
        .expect("delivered");
    assert_eq!(server.await.expect("server").tls_for("AUTH"), Some(true));
}

#[tokio::test]
async fn certificates_are_verified_and_failures_never_fall_back() {
    // Untrusted CA: the Mozilla roots alone.
    let (address, server) = serve(Script::default()).await;
    let mut untrusted = config("localhost", address.port());
    untrusted.extra_roots_pem = None;
    untrusted.allow_plaintext = true;
    let error = sender(untrusted, address)
        .send(&message())
        .await
        .expect_err("untrusted");
    assert_eq!(error, MailError::TlsFailed { stage: "starttls" });
    let transcript = server.await.expect("server");
    assert!(!transcript.saw("AUTH") && !transcript.saw("MAIL"));

    // Wrong name: the certificate is for localhost and 127.0.0.1.
    let (address, server) = serve(Script {
        implicit_tls: true,
        ..Script::default()
    })
    .await;
    let mut wrong = config("mail.example.com", address.port());
    wrong.implicit_tls = true;
    let error = sender(wrong, address)
        .send(&message())
        .await
        .expect_err("name");
    assert_eq!(error, MailError::TlsFailed { stage: "connect" });
    drop(server.await);
}

#[tokio::test]
async fn bytes_injected_before_the_handshake_are_refused() {
    let (address, server) = serve(Script {
        inject_after_starttls: true,
        ..Script::default()
    })
    .await;
    let error = sender(config("localhost", address.port()), address)
        .send(&message())
        .await
        .expect_err("injection");
    assert_eq!(error, MailError::TlsFailed { stage: "starttls" });
    server.abort();
}

#[tokio::test]
async fn a_refused_recipient_aborts_before_data_and_reply_text_is_dropped() {
    let (address, server) = serve(Script {
        rcpt_refused: Some(("audit@example.org", 550)),
        echo: true,
        ..Script::default()
    })
    .await;
    let error = sender(config("localhost", address.port()), address)
        .send(&message())
        .await
        .expect_err("rejected");
    assert_eq!(
        error,
        MailError::Rejected {
            stage: "rcpt",
            reply: Some(550)
        }
    );
    assert!(!error.is_retryable());
    secret_free(&error);
    let transcript = server.await.expect("server");
    assert!(!transcript.saw("DATA"));
}

#[tokio::test]
async fn reply_codes_are_classified() {
    for (script, expected) in [
        (
            Script {
                rcpt_refused: Some(("ada@", 451)),
                ..Script::default()
            },
            MailError::Unavailable {
                stage: "rcpt",
                reply: Some(451),
                timeout: false,
            },
        ),
        (
            Script {
                auth_reply: 535,
                ..Script::default()
            },
            MailError::AuthFailed {
                stage: "auth",
                reply: Some(535),
            },
        ),
        (
            Script {
                auth_reply: 534,
                ..Script::default()
            },
            MailError::AuthFailed {
                stage: "auth",
                reply: Some(534),
            },
        ),
        (
            Script {
                mail_reply: 530,
                ..Script::default()
            },
            MailError::AuthFailed {
                stage: "mail",
                reply: Some(530),
            },
        ),
        (
            Script {
                data_reply: 554,
                ..Script::default()
            },
            MailError::Rejected {
                stage: "data",
                reply: Some(554),
            },
        ),
        (
            Script {
                auth: "CRAM-MD5",
                ..Script::default()
            },
            MailError::AuthFailed {
                stage: "auth",
                reply: None,
            },
        ),
    ] {
        let (address, server) = serve(script).await;
        let error = sender(config("localhost", address.port()), address)
            .send(&message())
            .await
            .expect_err("failure");
        assert_eq!(error, expected);
        secret_free(&error);
        server.abort();
    }
}

#[tokio::test]
async fn a_refused_ehlo_falls_back_to_helo_which_offers_no_starttls() {
    let (address, server) = serve(Script {
        ehlo_reply: 502,
        ..Script::default()
    })
    .await;
    let error = sender(config("localhost", address.port()), address)
        .send(&message())
        .await
        .expect_err("no tls");
    assert_eq!(error, MailError::TlsRequired);
    assert!(server.await.expect("server").saw("HELO"));
}

#[tokio::test]
async fn a_lost_quit_after_acceptance_is_still_success() {
    let (address, server) = serve(Script {
        drop_on_quit: true,
        ..Script::default()
    })
    .await;
    sender(config("localhost", address.port()), address)
        .send(&message())
        .await
        .expect("accepted counts");
    drop(server.await);
}

#[tokio::test]
async fn a_hanging_quit_after_acceptance_is_still_success() {
    let (address, server) = serve(Script {
        hang_on_quit: true,
        ..Script::default()
    })
    .await;
    let mut quick = config("localhost", address.port());
    quick.timeout = Duration::from_millis(500);
    sender(quick, address)
        .send(&message())
        .await
        .expect("accepted counts");
    server.abort();
}

#[tokio::test]
async fn a_silent_server_hits_the_deadline() {
    let (address, server) = serve(Script {
        silent: true,
        ..Script::default()
    })
    .await;
    let mut slow = config("localhost", address.port());
    slow.timeout = Duration::from_millis(200);
    let started = std::time::Instant::now();
    let error = sender(slow, address)
        .send(&message())
        .await
        .expect_err("timeout");
    assert_eq!(
        error,
        MailError::Unavailable {
            stage: "greeting",
            reply: None,
            timeout: true
        }
    );
    assert!(error.is_retryable());
    assert!(started.elapsed() < Duration::from_secs(2));
    server.abort();
}

#[tokio::test]
async fn a_closed_port_is_transient() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    drop(listener);
    let error = sender(config("localhost", address.port()), address)
        .send(&message())
        .await
        .expect_err("refused");
    assert!(matches!(
        error,
        MailError::Unavailable {
            stage: "connect",
            ..
        }
    ));
}

#[tokio::test]
async fn invalid_messages_never_connect() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    let mut bad = message();
    bad.subject = "x\r\nBcc: eve@example.com".to_owned();
    let error = sender(config("localhost", address.port()), address)
        .send(&bad)
        .await
        .expect_err("invalid");
    assert_eq!(error.code(), "mail_invalid");
    let accepted = tokio::time::timeout(Duration::from_millis(100), listener.accept()).await;
    assert!(accepted.is_err(), "no connection may be opened");
}

#[test]
fn configuration_is_validated_and_redacted() {
    let ok = SmtpSender::new(config("smtp.example.com", 0)).expect("valid");
    assert_eq!(ok.port, 587);
    let debug = format!("{ok:?}");
    assert!(!debug.contains(PASSWORD), "{debug}");
    let mut implicit = config("smtp.example.com", 0);
    implicit.implicit_tls = true;
    assert_eq!(SmtpSender::new(implicit).expect("valid").port, 465);
    for host in ["", "smtp example.com", "a/b", "user@host", "host\r\n"] {
        assert!(SmtpSender::new(config(host, 0)).is_err(), "{host:?}");
    }
    let mut orphan = config("smtp.example.com", 0);
    orphan.username = None;
    assert!(SmtpSender::new(orphan).is_err());
    let mut control = config("smtp.example.com", 0);
    control.password = Some(SecretString::new("pass\r\nRCPT TO:<eve@example.com>"));
    assert!(SmtpSender::new(control).is_err());
    let mut hello = config("smtp.example.com", 0);
    hello.hello_name = Some("bad name".to_owned());
    assert!(SmtpSender::new(hello).is_err());
    let mut slow = config("smtp.example.com", 0);
    slow.timeout = Duration::from_secs(601);
    assert!(SmtpSender::new(slow).is_err());
    let mut roots = config("smtp.example.com", 0);
    roots.extra_roots_pem = Some(b"not pem".to_vec());
    assert!(SmtpSender::new(roots).is_err());
}

#[test]
fn dot_stuffing_and_termination() {
    assert_eq!(dot_stuff(b".a\r\nb\r\n.\r\n"), b"..a\r\nb\r\n..\r\n.\r\n");
    assert_eq!(dot_stuff(b"no end"), b"no end\r\n.\r\n");
}
