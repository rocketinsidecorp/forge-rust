//! Building senders from `FORGE_MAIL_*`.

use std::sync::Arc;

use forge_config::{AppConfig, Environment, MailTransport};

use crate::{Address, FileSender, MailError, Sender, SmtpConfig, SmtpSender};

/// Builds the [`Sender`] `FORGE_MAIL_URL` selects: an [`SmtpSender`] for
/// `smtp://` (STARTTLS) and `smtps://` (implicit TLS), the development
/// [`FileSender`] for `file:`.
///
/// It returns [`MailError::NotConfigured`] when no URL is set, so
/// applications can leave features that need mail disabled instead of
/// pretending to send. It repeats the production rules of configuration
/// loading so a hand-built [`AppConfig`] cannot bypass them: the file sender
/// is refused in production, and the SMTP sender requires verified TLS.
///
/// Production with `FORGE_MAIL_ALLOW_PLAINTEXT=true` and no credentials is
/// accepted (an internal relay), but logs a warning: nothing then stops a
/// network attacker from stripping `STARTTLS`, so the mail travels in clear.
pub fn from_config(config: &AppConfig) -> Result<Arc<dyn Sender>, MailError> {
    let mail = &config.mail;
    if mail.url.is_none() {
        return Err(MailError::NotConfigured);
    }
    let invalid = || MailError::Configuration("FORGE_MAIL_URL is invalid".to_owned());
    match mail.transport().ok_or_else(invalid)? {
        MailTransport::File => {
            if config.environment == Environment::Production {
                return Err(MailError::Configuration(
                    "the development file sender cannot be used in production".to_owned(),
                ));
            }
            Ok(Arc::new(FileSender::new(
                mail.file_path().ok_or_else(invalid)?,
            )?))
        }
        MailTransport::Starttls | MailTransport::ImplicitTls => {
            let endpoint = mail.smtp().ok_or_else(invalid)?;
            let extra_roots_pem = mail
                .ca_file
                .as_ref()
                .map(|path| {
                    std::fs::read(path).map_err(|_| {
                        MailError::Configuration("FORGE_MAIL_TLS_CA_FILE cannot be read".to_owned())
                    })
                })
                .transpose()?;
            if config.environment == Environment::Production
                && mail.allow_plaintext
                && endpoint.username.is_none()
            {
                tracing::warn!(
                    "FORGE_MAIL_ALLOW_PLAINTEXT=true in production without credentials: mail may be sent unencrypted, and a network attacker can strip STARTTLS"
                );
            }
            Ok(Arc::new(SmtpSender::new(SmtpConfig {
                host: endpoint.host,
                port: endpoint.port,
                implicit_tls: endpoint.implicit_tls,
                username: endpoint.username,
                password: endpoint.password,
                hello_name: None,
                extra_roots_pem,
                allow_plaintext: mail.allow_plaintext,
                timeout: mail.timeout,
                connect_timeout: std::time::Duration::ZERO,
            })?))
        }
    }
}

/// Parses `FORGE_MAIL_FROM` into an [`Address`].
pub fn default_from(config: &AppConfig) -> Result<Address, MailError> {
    let from =
        config.mail.from.as_deref().ok_or_else(|| {
            MailError::Configuration("FORGE_MAIL_FROM is not configured".to_owned())
        })?;
    Address::parse(from).map_err(|_| {
        MailError::Configuration("FORGE_MAIL_FROM is not a valid sender address".to_owned())
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn load(pairs: &[(&str, &str)]) -> AppConfig {
        let values: HashMap<String, String> = pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect();
        AppConfig::from_lookup(|key| values.get(key).cloned()).expect("valid configuration")
    }

    #[test]
    fn no_url_means_not_configured() {
        assert_eq!(
            from_config(&AppConfig::default()).err(),
            Some(MailError::NotConfigured)
        );
        assert!(default_from(&AppConfig::default()).is_err());
    }

    #[test]
    fn smtp_and_file_urls_build_senders() {
        let config = load(&[
            ("FORGE_MAIL_URL", "smtps://user:pass@smtp.example.com"),
            ("FORGE_MAIL_FROM", "Shop <shop@example.com>"),
        ]);
        assert!(from_config(&config).is_ok());
        assert_eq!(
            default_from(&config).expect("valid"),
            Address::new("Shop", "shop@example.com").expect("valid")
        );
        let root = tempfile::tempdir().expect("temporary directory");
        let url = format!("file://{}/mail", root.path().display());
        let config = load(&[
            ("FORGE_MAIL_URL", &url),
            ("FORGE_MAIL_FROM", "shop@example.com"),
        ]);
        assert!(from_config(&config).is_ok());
        assert!(root.path().join("mail").is_dir());
    }

    #[derive(Clone, Default)]
    struct Logs(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Logs {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("logs").extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// The log text `from_config` writes for `pairs`.
    fn logged(pairs: &[(&str, &str)]) -> String {
        let logs = Logs::default();
        let writer = logs.clone();
        let _guard = tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_writer(move || writer.clone())
                .with_ansi(false)
                .finish(),
        );
        assert!(from_config(&load(pairs)).is_ok());
        String::from_utf8_lossy(&logs.0.lock().expect("logs")).into_owned()
    }

    #[test]
    fn plaintext_in_production_without_credentials_is_warned_about() {
        let from = ("FORGE_MAIL_FROM", "shop@example.com");
        let plaintext = ("FORGE_MAIL_ALLOW_PLAINTEXT", "true");
        let production = ("FORGE_ENV", "production");
        let relay = ("FORGE_MAIL_URL", "smtp://relay.example.com");
        let warning = logged(&[relay, from, plaintext, production]);
        assert!(warning.contains("WARN"), "{warning}");
        assert!(warning.contains("strip STARTTLS"), "{warning}");
        // Not in production, without the flag, or with credentials.
        assert!(!logged(&[relay, from, plaintext]).contains("STARTTLS"));
        assert!(!logged(&[relay, from, production]).contains("STARTTLS"));
        let credentials = ("FORGE_MAIL_URL", "smtp://user:pass@localhost");
        let quiet = logged(&[credentials, from, plaintext, production]);
        assert!(!quiet.contains("STARTTLS"), "{quiet}");
        assert!(!quiet.contains("pass"), "{quiet}");
    }

    #[test]
    fn hand_built_production_configs_cannot_use_the_file_sender() {
        let mut config = load(&[
            ("FORGE_MAIL_URL", "file:///tmp/forge-never-created"),
            ("FORGE_MAIL_FROM", "shop@example.com"),
        ]);
        config.environment = Environment::Production;
        assert!(matches!(
            from_config(&config),
            Err(MailError::Configuration(message)) if message.contains("production")
        ));
    }

    #[test]
    fn an_unreadable_ca_file_is_an_error_without_the_path_contents() {
        let config = load(&[
            ("FORGE_MAIL_URL", "smtp://smtp.example.com"),
            ("FORGE_MAIL_FROM", "shop@example.com"),
            ("FORGE_MAIL_TLS_CA_FILE", "/nonexistent/forge/ca.pem"),
        ]);
        assert!(from_config(&config).is_err());
    }
}
