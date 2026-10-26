//! The SMTP sender's configuration rules and its failure to reach a server,
//! and how the mail value types print.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use forge_config::SecretString;
use forge_mail::{
    Address, MailError, Message, PasswordResetEmails, Sender, SmtpConfig, SmtpSender, Templates,
};

fn config(mutate: impl FnOnce(&mut SmtpConfig)) -> Result<SmtpSender, MailError> {
    let mut config = SmtpConfig {
        host: "mail.example.com".to_owned(),
        ..SmtpConfig::default()
    };
    mutate(&mut config);
    SmtpSender::new(config)
}

fn reason(result: Result<SmtpSender, MailError>) -> String {
    match result {
        Err(MailError::Configuration(reason)) => reason,
        other => panic!("expected a configuration error, got {other:?}"),
    }
}

#[test]
fn smtp_settings_are_validated_without_touching_the_network() {
    let sender = config(|_| {}).expect("defaults");
    let printed = format!("{sender:?}");
    assert!(
        printed.contains("mail.example.com") && printed.contains("587"),
        "{printed}"
    );
    assert!(printed.contains("authenticates: false"), "{printed}");
    assert!(format!("{:?}", config(|c| c.implicit_tls = true).unwrap()).contains("465"));

    let with_credentials = config(|c| {
        c.username = Some("ada".to_owned());
        c.password = Some(SecretString::new("hunter2-secret"));
    })
    .unwrap();
    let printed = format!("{with_credentials:?}");
    assert!(printed.contains("authenticates: true"), "{printed}");
    assert!(!printed.contains("hunter2"), "{printed}");

    assert!(
        reason(config(|c| c.username = Some("ada".to_owned()))).contains("requires a password")
    );
    assert!(
        reason(config(|c| c.password = Some(SecretString::new("x"))))
            .contains("requires a username")
    );
    assert!(reason(config(|c| c.host = String::new())).contains("host is required"));
    assert!(reason(config(|c| c.host = "a/b".to_owned())).contains("host is required"));
    assert!(
        reason(config(|c| {
            c.username = Some("ada".to_owned());
            c.password = Some(SecretString::new("pass\nword"));
        }))
        .contains("control characters")
    );
    assert!(
        reason(config(|c| c.hello_name = Some("not valid!".to_owned()))).contains("hello name")
    );
    assert!(config(|c| c.hello_name = Some("[127.0.0.1]".to_owned())).is_ok());
    assert!(
        reason(config(|c| c.timeout = Duration::from_secs(3600))).contains("at most 10 minutes")
    );
    assert!(
        reason(config(|c| c.connect_timeout = Duration::from_secs(3600)))
            .contains("at most 10 minutes")
    );
    // Zero means the default, and an IPv6 literal loses its brackets.
    assert!(
        config(|c| {
            c.timeout = Duration::ZERO;
            c.connect_timeout = Duration::ZERO;
            c.host = "[::1]".to_owned();
        })
        .is_ok()
    );
    assert!(reason(config(|c| c.extra_roots_pem = Some(b"not a pem".to_vec()))).contains("CA"));
}

#[tokio::test]
async fn a_server_that_is_not_there_is_a_retryable_connect_failure() {
    let closed = {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        listener.local_addr().unwrap().port()
    };
    let sender = config(|c| {
        c.host = "127.0.0.1".to_owned();
        c.port = closed;
        c.allow_plaintext = true;
    })
    .unwrap();
    let message = Message::new(Address::parse("app@example.com").unwrap())
        .to(Address::parse("ada@example.com").unwrap())
        .subject("Hi")
        .text("Hello");
    let error = sender.send(&message).await.unwrap_err();
    assert!(
        matches!(
            &error,
            MailError::Unavailable {
                stage: "connect",
                reply: None,
                timeout: false
            }
        ),
        "{error:?}"
    );
    assert!(error.is_retryable());
}

#[test]
fn mail_value_types_print_their_shape_and_not_their_content() {
    let printed = format!("{:?}", Templates::new());
    assert!(
        printed.contains("subject: false") && printed.contains("html: false"),
        "{printed}"
    );
    let emails = PasswordResetEmails::new(
        Address::parse("app@example.com").unwrap(),
        "Shop",
        "https://shop.example.com/passwords/edit",
    )
    .unwrap();
    let printed = format!("{emails:?}");
    assert!(
        printed.contains("Shop") && printed.contains("passwords/edit"),
        "{printed}"
    );
}
