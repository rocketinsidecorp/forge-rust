//! Password reset emails: the adapter that delivers `forge-auth` reset links
//! through a mail [`Sender`], Forge-Go's `auth/recoverymail`.

use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use askama::Template;
use async_trait::async_trait;
use forge_auth::{
    PasswordResetDelivery, PasswordResetDeliveryError, PasswordResetNotifier,
    UndeliverablePasswordResets, UnixTimestamp,
};
use forge_config::{AppConfig, is_local_web_host};
use forge_telemetry::Telemetry;

use crate::{
    Address, MailError, Message, Preview, Sender, Templates, default_from, from_config, instrument,
    template::Rendered,
};

const MAX_APP_NAME_BYTES: usize = 100;
/// A canonical token for probes and previews; it opens nothing.
const FIXTURE_TOKEN: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// What a reset email template receives.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PasswordResetEmail {
    /// The application's name, shown in the subject and body.
    pub app_name: String,
    /// The complete reset link, token included.
    pub link: String,
    /// When the link expires.
    pub expires_at: UnixTimestamp,
    /// Minutes left, rounded up, at least one.
    pub expires_in_minutes: u64,
}

type ComposeFn = dyn Fn(&PasswordResetEmail) -> Result<Rendered, MailError> + Send + Sync;

#[derive(Template)]
#[template(path = "password_reset.txt")]
struct ResetText<'a> {
    app_name: &'a str,
    link: &'a str,
    expires_in_minutes: u64,
}

#[derive(Template)]
#[template(path = "password_reset.html")]
struct ResetHtml<'a> {
    app_name: &'a str,
    link: &'a str,
    expires_in_minutes: u64,
}

/// The built-in English template.
fn built_in(email: &PasswordResetEmail) -> Result<Rendered, MailError> {
    let text = ResetText {
        app_name: &email.app_name,
        link: &email.link,
        expires_in_minutes: email.expires_in_minutes,
    };
    let html = ResetHtml {
        app_name: &email.app_name,
        link: &email.link,
        expires_in_minutes: email.expires_in_minutes,
    };
    let mut rendered = Templates::new().text(&text).html(&html).render()?;
    rendered.subject = format!("Reset your {} password", email.app_name);
    Ok(rendered)
}

/// Composes password reset emails: the sender, the application name, the
/// public reset page and the template. It sends nothing; see
/// [`PasswordResetMailer`].
#[derive(Clone)]
pub struct PasswordResetEmails {
    from: Address,
    app_name: String,
    reset_url: String,
    compose: Arc<ComposeFn>,
}

impl std::fmt::Debug for PasswordResetEmails {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PasswordResetEmails")
            .field("from", &self.from)
            .field("app_name", &self.app_name)
            .field("reset_url", &self.reset_url)
            .finish_non_exhaustive()
    }
}

impl PasswordResetEmails {
    /// Validates the settings and renders a probe email, so an `app_name`,
    /// `from` or template that can never produce a deliverable email stops
    /// start-up instead of failing every reset behind the uniform answer.
    ///
    /// `reset_url` is the page a link opens (`/passwords/edit`): an absolute
    /// `https` URL, or `http` on `localhost`, without credentials or
    /// fragment. The token is appended as its `token` query parameter.
    pub fn new(
        from: Address,
        app_name: impl Into<String>,
        reset_url: impl Into<String>,
    ) -> Result<Self, MailError> {
        let app_name = app_name.into();
        let reset_url = reset_url.into();
        from.validate()
            .map_err(|_| configuration("password reset From must be a valid sender address"))?;
        if app_name.trim().is_empty()
            || app_name.len() > MAX_APP_NAME_BYTES
            || app_name.chars().any(char::is_control)
        {
            return Err(configuration(
                "password reset app name is required (at most 100 bytes, no control characters)",
            ));
        }
        validate_reset_url(&reset_url)?;
        let emails = Self {
            from,
            app_name,
            reset_url,
            compose: Arc::new(built_in),
        };
        emails.probe()?;
        Ok(emails)
    }

    /// Reads `FORGE_MAIL_FROM` and `FORGE_AUTH_PASSWORD_RESET_URL`.
    pub fn from_config(config: &AppConfig, app_name: &str) -> Result<Self, MailError> {
        let reset_url = config.auth.password_reset_url.as_deref().ok_or_else(|| {
            configuration(
                "FORGE_MAIL_URL is set but FORGE_AUTH_PASSWORD_RESET_URL is not: reset emails need the public URL of /passwords/edit",
            )
        })?;
        Self::new(default_from(config)?, app_name, reset_url)
    }

    /// For [`PasswordResetEmails::preview`] in development: the configured
    /// sender and reset page when set, otherwise `<app_name>
    /// <no-reply@example.com>` and `http://localhost:<port>/passwords/edit`.
    pub fn for_preview(config: &AppConfig, app_name: &str) -> Result<Self, MailError> {
        let from = match default_from(config) {
            Ok(from) => from,
            Err(_) => Address::new(app_name, "no-reply@example.com")?,
        };
        let reset_url = config.auth.password_reset_url.clone().unwrap_or_else(|| {
            format!(
                "http://localhost:{}{}",
                config.server.bind.port(),
                forge_auth::PASSWORD_RESET_PATH
            )
        });
        Self::new(from, app_name, reset_url)
    }

    /// Replaces the built-in English template, then probes it.
    pub fn with_template(
        mut self,
        compose: impl Fn(&PasswordResetEmail) -> Result<Rendered, MailError> + Send + Sync + 'static,
    ) -> Result<Self, MailError> {
        self.compose = Arc::new(compose);
        self.probe()?;
        Ok(self)
    }

    fn probe(&self) -> Result<(), MailError> {
        let expires =
            UnixTimestamp::from_secs(unix_now() + forge_auth::PASSWORD_RESET_LIFETIME_SECS);
        self.compose("probe@example.com", FIXTURE_TOKEN, expires)
            .and_then(|message| message.validate())
            .map_err(|_| {
                configuration(
                    "password reset settings do not produce a valid email (check the app name, From and template)",
                )
            })
    }

    /// The reset link for `token`.
    #[must_use]
    pub fn link(&self, token: &str) -> String {
        let separator = if self.reset_url.contains('?') {
            '&'
        } else {
            '?'
        };
        format!("{}{separator}token={token}", self.reset_url)
    }

    /// Renders the email to `login` for `token`.
    pub fn compose(
        &self,
        login: &str,
        token: &str,
        expires_at: UnixTimestamp,
    ) -> Result<Message, MailError> {
        let to = Address::new("", login)?;
        let remaining = expires_at.as_secs().saturating_sub(unix_now());
        let email = PasswordResetEmail {
            app_name: self.app_name.clone(),
            link: self.link(token),
            expires_at,
            expires_in_minutes: remaining.div_ceil(60).max(1),
        };
        let base = Message::new(self.from.clone())
            .to(to)
            .header("Auto-Submitted", "auto-generated");
        Ok((self.compose)(&email)?.apply(base))
    }

    /// A preview of the email (`password_reset`) with a fixture recipient
    /// and token, for [`crate::PreviewHandler`].
    #[must_use]
    pub fn preview(&self) -> Preview {
        let emails = self.clone();
        Preview::new(
            "password_reset",
            "The email a password reset request sends",
            move || {
                let expires =
                    UnixTimestamp::from_secs(unix_now() + forge_auth::PASSWORD_RESET_LIFETIME_SECS);
                Ok(emails.compose("someone@example.com", FIXTURE_TOKEN, expires)?)
            },
        )
    }
}

/// Delivers password reset links by email: a
/// [`PasswordResetNotifier`] over a mail [`Sender`].
///
/// The token appears only in the link inside the message body: it is never
/// logged and never part of an error. `forge-auth` answers the reset request
/// identically whether delivery worked, so every failure is logged here
/// once, with the error code and SMTP stage only (never the recipient, the
/// link or the token), before
/// [`PasswordResetDeliveryError::Unavailable`] is returned.
#[derive(Clone, Debug)]
pub struct PasswordResetMailer {
    emails: PasswordResetEmails,
    sender: Arc<dyn Sender>,
}

impl PasswordResetMailer {
    /// Delivers `emails` through `sender`.
    #[must_use]
    pub fn new(emails: PasswordResetEmails, sender: Arc<dyn Sender>) -> Self {
        Self { emails, sender }
    }

    /// The composer.
    #[must_use]
    pub fn emails(&self) -> &PasswordResetEmails {
        &self.emails
    }

    fn fail(step: &'static str, error: &MailError) -> PasswordResetDeliveryError {
        tracing::error!(
            step,
            code = error.code(),
            stage = error.stage().unwrap_or_default(),
            "password reset email was not sent"
        );
        PasswordResetDeliveryError::Unavailable
    }
}

#[async_trait]
impl PasswordResetNotifier for PasswordResetMailer {
    async fn deliver(
        &self,
        delivery: &PasswordResetDelivery,
    ) -> Result<(), PasswordResetDeliveryError> {
        let message = self
            .emails
            .compose(
                delivery.login(),
                delivery.token().expose(),
                delivery.expires_at(),
            )
            .map_err(|error| Self::fail("render", &error))?;
        self.sender
            .send(&message)
            .await
            .map_err(|error| Self::fail("send", &error))
    }
}

/// The reset notifier a generated application wires: emails through the
/// sender `FORGE_MAIL_URL` selects (instrumented when telemetry is on) when
/// mail is configured, and the fail-closed
/// [`UndeliverablePasswordResets`] when it is not.
///
/// `FORGE_MAIL_URL` without `FORGE_AUTH_PASSWORD_RESET_URL` (or a missing
/// `FORGE_MAIL_FROM`) is an error, so a half-configured deployment never
/// advertises a reset it cannot send.
pub fn password_reset_notifier(
    config: &AppConfig,
    app_name: &str,
    telemetry: Option<&Telemetry>,
) -> Result<Arc<dyn PasswordResetNotifier>, MailError> {
    let sender = match from_config(config) {
        Err(MailError::NotConfigured) => return Ok(Arc::new(UndeliverablePasswordResets)),
        other => other?,
    };
    let emails = PasswordResetEmails::from_config(config, app_name)?;
    let sender = instrument(sender, telemetry)?;
    Ok(Arc::new(PasswordResetMailer::new(emails, sender)))
}

fn configuration(message: &str) -> MailError {
    MailError::Configuration(message.to_owned())
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
}

/// An absolute `https` URL, or `http` on a local host, without credentials
/// or fragment (Forge-Go's `recoverymail` rule).
fn validate_reset_url(url: &str) -> Result<(), MailError> {
    let refuse = || {
        configuration(
            "password reset URL must be an absolute https URL (http only for localhost) without credentials or fragment",
        )
    };
    if url
        .chars()
        .any(|character| character.is_control() || character == ' ')
        || url.contains('#')
    {
        return Err(refuse());
    }
    let (scheme, rest) = url.split_once("://").ok_or_else(refuse)?;
    let authority = rest.split(['/', '?']).next().unwrap_or_default();
    if authority.is_empty() || authority.contains('@') {
        return Err(refuse());
    }
    let host = if let Some(bracketed) = authority.strip_prefix('[') {
        bracketed.split(']').next().unwrap_or_default()
    } else {
        authority.split(':').next().unwrap_or_default()
    };
    if host.is_empty() {
        return Err(refuse());
    }
    match scheme.to_ascii_lowercase().as_str() {
        "https" => Ok(()),
        "http" if is_local_web_host(&host.to_ascii_lowercase()) => Ok(()),
        _ => Err(refuse()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reset_urls_must_be_https_or_local() {
        for url in [
            "https://app.example.com/passwords/edit",
            "http://localhost:3000/passwords/edit",
            "http://[::1]:3000/passwords/edit",
            "http://shop.localhost/passwords/edit",
        ] {
            assert!(validate_reset_url(url).is_ok(), "{url}");
        }
        for url in [
            "http://app.example.com/passwords/edit",
            "https://user:pass@app.example.com/x",
            "https://app.example.com/x#frag",
            "javascript:alert(1)",
            "/passwords/edit",
            "https:///x",
            "https://app.example.com/x\r\n",
        ] {
            assert!(validate_reset_url(url).is_err(), "{url}");
        }
    }

    #[test]
    fn settings_are_validated_with_a_probe() {
        let from = Address::new("Shop", "shop@example.com").expect("valid");
        let url = "https://shop.example.com/passwords/edit";
        assert!(PasswordResetEmails::new(from.clone(), "Shop", url).is_ok());
        for name in ["", "   ", "Shop\r\nBcc: eve@example.com", &"x".repeat(101)] {
            assert!(
                PasswordResetEmails::new(from.clone(), name, url).is_err(),
                "{name:?}"
            );
        }
        let bad_from = Address {
            name: String::new(),
            email: "not an address".to_owned(),
        };
        assert!(PasswordResetEmails::new(bad_from, "Shop", url).is_err());
        // A template that can never produce a valid email stops start-up.
        let broken = PasswordResetEmails::new(from, "Shop", url)
            .expect("valid")
            .with_template(|_| Ok(Rendered::default()));
        assert!(broken.is_err());
    }

    #[test]
    fn links_append_the_token() {
        let from = Address::new("Shop", "shop@example.com").expect("valid");
        let emails = PasswordResetEmails::new(from, "Shop", "http://localhost:3000/passwords/edit")
            .expect("valid");
        assert_eq!(
            emails.link("abc"),
            "http://localhost:3000/passwords/edit?token=abc"
        );
    }
}
