//! Password reset delivery end to end: a `forge-auth` reset delivery goes
//! through the mail adapter into the test recorder (or the development file
//! sender), and failures are logged without the recipient, link or token.

use std::{
    collections::HashMap,
    io::Write,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use forge_auth::{
    PasswordResetDelivery, PasswordResetDeliveryError, PasswordResetNotifier, PasswordResetToken,
    UnixTimestamp,
};
use forge_config::AppConfig;
use forge_mail::{
    Address, Instrumented, MailError, PasswordResetEmails, PasswordResetMailer, Sender,
    password_reset_notifier, testing::Recorder,
};

const RESET_URL: &str = "https://shop.example.com/passwords/edit";

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("after 1970")
        .as_secs()
}

fn emails() -> PasswordResetEmails {
    PasswordResetEmails::new(
        Address::new("Shop", "no-reply@shop.example.com").expect("valid"),
        "Shop",
        RESET_URL,
    )
    .expect("valid settings")
}

fn reset_for(login: &str) -> (PasswordResetDelivery, String) {
    let token = PasswordResetToken::generate().expect("randomness");
    let raw = token.expose().to_owned();
    (
        PasswordResetDelivery::new(login, token, UnixTimestamp::from_secs(now() + 900)),
        raw,
    )
}

#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<u8>>>);

impl Write for Logs {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("logs").extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Logs {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().expect("logs")).into_owned()
    }

    fn capture(&self) -> tracing::subscriber::DefaultGuard {
        let writer = self.clone();
        tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_writer(move || writer.clone())
                .with_ansi(false)
                .finish(),
        )
    }
}

#[tokio::test(flavor = "current_thread")]
async fn a_reset_link_is_delivered_through_the_mailer() {
    let recorder = Arc::new(Recorder::new());
    let mailer = PasswordResetMailer::new(emails(), recorder.clone());
    let (delivery, token) = reset_for("ada@example.org");
    mailer.deliver(&delivery).await.expect("delivered");

    let sent = recorder.last().expect("one email");
    assert_eq!(sent.recipients, ["ada@example.org"]);
    assert_eq!(sent.message.subject, "Reset your Shop password");
    assert_eq!(
        sent.message
            .headers
            .get("Auto-Submitted")
            .map(String::as_str),
        Some("auto-generated")
    );
    let link = format!("{RESET_URL}?token={token}");
    assert!(sent.message.text.contains(&link), "{}", sent.message.text);
    assert!(
        sent.message.html.contains(&format!("href=\"{link}\"")),
        "{}",
        sent.message.html
    );
    assert!(
        sent.message.text.contains("expires in 15 minutes"),
        "{}",
        sent.message.text
    );
    assert_eq!(
        sent.header("From").as_deref(),
        Some("\"Shop\" <no-reply@shop.example.com>")
    );
}

#[tokio::test(flavor = "current_thread")]
async fn failures_are_logged_without_recipient_link_or_token() {
    let logs = Logs::default();
    let _guard = logs.capture();
    let recorder = Arc::new(Recorder::new());
    recorder.fail_with(Some(MailError::Unavailable {
        stage: "rcpt",
        reply: Some(451),
        timeout: false,
    }));
    let mailer = PasswordResetMailer::new(emails(), recorder.clone());
    let (delivery, token) = reset_for("ada@example.org");
    assert_eq!(
        mailer.deliver(&delivery).await,
        Err(PasswordResetDeliveryError::Unavailable)
    );
    // An address Forge cannot deliver to fails before sending.
    let (unsupported, other_token) = reset_for("joão@example.org");
    assert_eq!(
        mailer.deliver(&unsupported).await,
        Err(PasswordResetDeliveryError::Unavailable)
    );
    let text = logs.text();
    assert!(text.contains("password reset email was not sent"), "{text}");
    assert!(
        text.contains("mail_unavailable") && text.contains("rcpt"),
        "{text}"
    );
    assert!(text.contains("mail_address_unsupported"), "{text}");
    for secret in [
        token.as_str(),
        other_token.as_str(),
        "ada@",
        "jo",
        "shop.example.com",
    ] {
        assert!(!text.contains(secret), "{secret} leaked: {text}");
    }
    assert!(recorder.is_empty());
}

fn config(pairs: &[(&str, &str)]) -> AppConfig {
    let values: HashMap<String, String> = pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect();
    AppConfig::from_lookup(|key| values.get(key).cloned()).expect("valid configuration")
}

#[tokio::test(flavor = "current_thread")]
async fn without_mail_the_notifier_fails_closed() {
    let notifier = password_reset_notifier(&AppConfig::default(), "shop", None).expect("default");
    let (delivery, _) = reset_for("ada@example.org");
    assert_eq!(
        notifier.deliver(&delivery).await,
        Err(PasswordResetDeliveryError::NotConfigured)
    );
}

#[test]
fn mail_without_a_reset_url_stops_start_up() {
    let root = tempfile::tempdir().expect("temporary directory");
    let url = format!("file://{}/mail", root.path().display());
    let error = password_reset_notifier(
        &config(&[
            ("FORGE_MAIL_URL", &url),
            ("FORGE_MAIL_FROM", "shop@example.com"),
        ]),
        "shop",
        None,
    )
    .err()
    .expect("refused");
    assert!(
        error.to_string().contains("FORGE_AUTH_PASSWORD_RESET_URL"),
        "{error}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn the_development_file_sender_receives_reset_emails() {
    let root = tempfile::tempdir().expect("temporary directory");
    let dir = root.path().join("mail");
    let url = format!("file://{}", dir.display());
    let configuration = config(&[
        ("FORGE_MAIL_URL", &url),
        ("FORGE_MAIL_FROM", "Shop <shop@example.com>"),
        (
            "FORGE_AUTH_PASSWORD_RESET_URL",
            "http://localhost:3000/passwords/edit",
        ),
    ]);
    let telemetry =
        forge_telemetry::Telemetry::new(forge_telemetry::Options::new("shop")).expect("telemetry");
    let notifier =
        password_reset_notifier(&configuration, "shop", Some(&telemetry)).expect("configured");
    let (delivery, token) = reset_for("ada@example.org");
    notifier.deliver(&delivery).await.expect("written");
    let files: Vec<_> = std::fs::read_dir(&dir).expect("listing").collect();
    assert_eq!(files.len(), 1);
    let eml = std::fs::read_to_string(files[0].as_ref().expect("entry").path()).expect("read");
    assert!(
        eml.starts_with("X-Forge-Envelope-To: ada@example.org\r\n"),
        "{eml}"
    );
    // Quoted-printable may break the link across lines; undo soft breaks.
    let decoded = eml.replace("=\r\n", "").replace("=3D", "=");
    assert!(
        decoded.contains(&format!(
            "http://localhost:3000/passwords/edit?token={token}"
        )),
        "{decoded}"
    );
    let metrics = forge_telemetry::prometheus::render(telemetry.metrics());
    assert!(
        metrics.contains("forge_mail_send_duration_seconds_count{forge_mail_outcome=\"sent\"} 1"),
        "{metrics}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn instrumented_sends_record_the_outcome_code() {
    let telemetry =
        forge_telemetry::Telemetry::new(forge_telemetry::Options::new("shop")).expect("telemetry");
    let recorder = Arc::new(Recorder::new());
    let sender = Instrumented::new(recorder.clone(), &telemetry).expect("instruments");
    let message = emails()
        .compose(
            "ada@example.org",
            &"0".repeat(64),
            UnixTimestamp::from_secs(now() + 60),
        )
        .expect("composed");
    sender.send(&message).await.expect("sent");
    recorder.fail_with(Some(MailError::Rejected {
        stage: "data",
        reply: Some(554),
    }));
    assert!(sender.send(&message).await.is_err());
    let metrics = forge_telemetry::prometheus::render(telemetry.metrics());
    for outcome in ["sent", "mail_rejected"] {
        assert!(
            metrics.contains(&format!(
                "forge_mail_send_duration_seconds_count{{forge_mail_outcome=\"{outcome}\"}} 1"
            )),
            "{metrics}"
        );
    }
    assert!(!metrics.contains("ada@"), "{metrics}");
}

#[test]
fn previews_render_the_reset_email_without_a_mail_server() {
    let mut configuration = AppConfig::default();
    configuration.mail.preview = true;
    let preview = PasswordResetEmails::for_preview(&configuration, "shop")
        .expect("fixture settings")
        .preview();
    assert_eq!(preview.name(), "password_reset");
    let handler = forge_mail::PreviewHandler::new(&configuration, "/_mail", vec![preview])
        .expect("development");
    let request = forge_http::Request::new_for_test(
        forge_http::Method::GET,
        "/_mail/password_reset/text".parse().expect("uri"),
        forge_http::HeaderMap::new(),
        Vec::new().into(),
        "127.0.0.1:9".parse().expect("address"),
        uuid::Uuid::nil(),
    );
    let response = handler.serve(&request).expect("served");
    let body = String::from_utf8_lossy(response.body()).into_owned();
    assert!(
        body.contains("http://localhost:3000/passwords/edit?token=0000"),
        "{body}"
    );
}
