//! The mail error codes and the development file sender on directories it
//! must tighten, leave alone or refuse.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::os::unix::fs::PermissionsExt as _;

use forge_mail::{Address, FileSender, MailError, Message, Sender};

#[test]
fn every_error_has_a_stable_code_and_only_what_is_safe_to_report() {
    use std::io::ErrorKind;
    let cases: Vec<(MailError, &str, bool, Option<&str>, Option<u16>)> = vec![
        (
            MailError::Invalid {
                field: "to".into(),
                reason: "is required",
            },
            "mail_invalid",
            false,
            None,
            None,
        ),
        (
            MailError::AddressUnsupported {
                field: "from".into(),
                reason: "is not ASCII",
            },
            "mail_address_unsupported",
            false,
            None,
            None,
        ),
        (
            MailError::Unavailable {
                stage: "connect",
                reply: Some(421),
                timeout: true,
            },
            "mail_unavailable",
            true,
            Some("connect"),
            Some(421),
        ),
        (
            MailError::Rejected {
                stage: "rcpt",
                reply: Some(550),
            },
            "mail_rejected",
            false,
            Some("rcpt"),
            Some(550),
        ),
        (
            MailError::AuthFailed {
                stage: "auth",
                reply: None,
            },
            "mail_auth_failed",
            false,
            Some("auth"),
            None,
        ),
        (
            MailError::TlsRequired,
            "mail_tls_required",
            false,
            Some("starttls"),
            None,
        ),
        (
            MailError::TlsFailed { stage: "starttls" },
            "mail_tls_failed",
            false,
            Some("starttls"),
            None,
        ),
        (
            MailError::Template { part: "html" },
            "mail_template",
            false,
            None,
            None,
        ),
        (
            MailError::File {
                kind: ErrorKind::NotFound,
            },
            "mail_file_failed",
            false,
            None,
            None,
        ),
        (
            MailError::NotConfigured,
            "mail_not_configured",
            false,
            None,
            None,
        ),
        (
            MailError::Configuration("bad".into()),
            "mail_configuration",
            false,
            None,
            None,
        ),
    ];
    for (error, code, retryable, stage, reply) in cases {
        assert_eq!(error.code(), code, "{error}");
        assert_eq!(error.is_retryable(), retryable, "{error}");
        assert_eq!(error.stage(), stage, "{error}");
        assert_eq!(error.reply_code(), reply, "{error}");
        assert!(!error.to_string().is_empty());
    }
    let shown = MailError::Rejected {
        stage: "rcpt",
        reply: Some(550),
    }
    .to_string();
    assert_eq!(
        shown,
        "mail server rejected the message (stage rcpt, reply 550)"
    );
    let shown = MailError::AuthFailed {
        stage: "auth",
        reply: None,
    }
    .to_string();
    assert_eq!(shown, "mail server authentication failed (stage auth)");
}

fn mode(path: &std::path::Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
}

#[test]
fn a_file_sender_needs_a_usable_directory() {
    assert!(matches!(
        FileSender::new(""),
        Err(MailError::Configuration(reason)) if reason.contains("directory is required")
    ));
    let root = tempfile::tempdir().unwrap();
    let blocker = root.path().join("blocker");
    std::fs::write(&blocker, b"x").unwrap();
    assert!(matches!(
        FileSender::new(blocker.join("mail")),
        Err(MailError::File { .. })
    ));
    assert!(matches!(
        FileSender::new(&blocker),
        Err(MailError::File { .. })
    ));
    assert_eq!(std::fs::read(&blocker).unwrap(), b"x");
}

#[test]
fn an_existing_directory_is_tightened_unless_it_is_shared_or_a_link() {
    let root = tempfile::tempdir().unwrap();

    let open = root.path().join("open");
    std::fs::create_dir(&open).unwrap();
    std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o755)).unwrap();
    let sender = FileSender::new(&open).unwrap();
    assert_eq!(mode(&open), 0o700);
    assert_eq!(sender.dir(), open.as_path());
    // Already private: left as it is.
    FileSender::new(&open).unwrap();
    assert_eq!(mode(&open), 0o700);

    let shared = root.path().join("shared");
    std::fs::create_dir(&shared).unwrap();
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o1777)).unwrap();
    FileSender::new(&shared).unwrap();
    assert_eq!(
        mode(&shared),
        0o1777,
        "a sticky directory is not ours to change"
    );

    let target = root.path().join("target");
    std::fs::create_dir(&target).unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
    let link = root.path().join("link");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    FileSender::new(&link).unwrap();
    assert_eq!(mode(&target), 0o755, "the link's target is left alone");
}

#[tokio::test]
async fn a_message_is_written_once_with_every_recipient_and_a_private_mode() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("outbox");
    let sender = FileSender::new(&dir).unwrap();
    let from = Address::parse("app@example.com").unwrap();
    let message = Message::new(from)
        .to(Address::parse("ada@example.com").unwrap())
        .subject("Hello")
        .text("Hi");
    sender.send(&message).await.unwrap();
    sender.send(&message).await.unwrap();
    let files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(files.len(), 2, "each send is its own file");
    for file in files {
        assert_eq!(mode(&file), 0o600);
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(
            text.starts_with("X-Forge-Envelope-To: ada@example.com"),
            "{text}"
        );
    }
    // An invalid message writes nothing.
    let empty = Message::new(Address::parse("app@example.com").unwrap());
    assert!(sender.send(&empty).await.is_err());
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2);
}
