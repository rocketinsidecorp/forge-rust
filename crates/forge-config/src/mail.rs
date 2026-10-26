//! Outgoing mail settings: the `FORGE_MAIL_*` variables and
//! `FORGE_AUTH_PASSWORD_RESET_URL` (ADR 0035), and the TOTP seed key
//! `FORGE_AUTH_MFA_KEY` (ADR 0040).
//!
//! The keys, defaults and rules are Forge-Go's (`config/mail.go`,
//! `config/mailbox.go`, ADR 0024 there), so one set of variables configures
//! both frameworks. This module also holds the single definition of what
//! Forge accepts as a mailbox, shared by `FORGE_MAIL_FROM` validation and
//! every message `forge-mail` encodes, so the two cannot drift apart.

use std::{
    net::{IpAddr, Ipv6Addr},
    path::PathBuf,
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{ConfigError, Environment, REDACTED, SecretString, telemetry};

/// Every `FORGE_MAIL_*` variable, in the order the help lists them.
pub const MAIL_KEYS: &[&str] = &[
    "FORGE_MAIL_URL",
    "FORGE_MAIL_FROM",
    "FORGE_MAIL_ALLOW_PLAINTEXT",
    "FORGE_MAIL_TIMEOUT",
    "FORGE_MAIL_PREVIEW",
    "FORGE_MAIL_TLS_CA_FILE",
];

/// `FORGE_AUTH_PASSWORD_RESET_URL`: the absolute URL of the page password
/// reset emails link to.
pub const PASSWORD_RESET_URL_KEY: &str = "FORGE_AUTH_PASSWORD_RESET_URL";

/// `FORGE_AUTH_MFA_KEY`: the AES-256-GCM key that encrypts TOTP seeds at
/// rest, as canonical unpadded base64url of 32 random bytes (Forge-Go's
/// format). Unset or empty, multi-factor authentication is not configured.
pub const MFA_KEY_KEY: &str = "FORGE_AUTH_MFA_KEY";

/// Bytes of the `FORGE_AUTH_MFA_KEY` key.
pub const MFA_KEY_BYTES: usize = 32;

/// `FORGE_AUTH_SESSION_IDLE_TIMEOUT`: how long a session may go unused
/// before it ends, ahead of its absolute expiry (Forge-Go's key, default and
/// bounds).
pub const SESSION_IDLE_TIMEOUT_KEY: &str = "FORGE_AUTH_SESSION_IDLE_TIMEOUT";

/// Bounds and default of [`AuthConfig::session_idle_timeout`]: at least five
/// minutes, at most the longest session lifetime (30 days), where it no
/// longer shortens anything. `forge::auth::SessionIdleTimeout` holds the
/// same bounds.
pub const DEFAULT_SESSION_IDLE_TIMEOUT: Duration = Duration::from_secs(2 * 3600);
const MIN_SESSION_IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const MAX_SESSION_IDLE_TIMEOUT: Duration = Duration::from_secs(720 * 3600);

const DEFAULT_MAIL_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_MAIL_TIMEOUT: Duration = Duration::from_secs(600);
const MAIL_URL_CONTRACT: &str =
    "smtp://user:password@host:587, smtps://user:password@host:465 or file:///path";

const MAX_MAILBOX_BYTES: usize = 254;
const MAX_LOCAL_BYTES: usize = 64;
const MAX_DOMAIN_BYTES: usize = 253;
const MAX_LABEL_BYTES: usize = 63;
const MAX_NAME_CHARS: usize = 255;

/// How `FORGE_MAIL_URL` delivers mail.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MailTransport {
    /// `smtp://`: connect, then upgrade with STARTTLS (port 587).
    Starttls,
    /// `smtps://`: TLS from the first byte (port 465).
    ImplicitTls,
    /// `file://`: the development sender writing `.eml` files. Refused in
    /// production.
    File,
}

/// The SMTP server described by an `smtp://` or `smtps://` URL.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MailEndpoint {
    /// Host name or IP address (without brackets).
    pub host: String,
    /// Port; 587 for `smtp://` and 465 for `smtps://` unless the URL names one.
    pub port: u16,
    /// Percent-decoded user name, when the URL carries credentials.
    pub username: Option<String>,
    /// Percent-decoded password, when the URL carries credentials.
    pub password: Option<SecretString>,
    /// `true` for `smtps://`.
    pub implicit_tls: bool,
}

/// Outgoing mail settings (`FORGE_MAIL_*`, docs/mail.md).
///
/// An unset URL means the application sends no mail: features that need it,
/// such as password reset emails, stay fail-closed.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MailConfig {
    /// Transport and usually credentials (`FORGE_MAIL_URL`); redacted in
    /// diagnostics.
    pub url: Option<SecretString>,
    /// Default sender (`FORGE_MAIL_FROM`), `app@example.com` or
    /// `App <app@example.com>`; required with a URL.
    pub from: Option<String>,
    /// Lets the SMTP sender continue when the server offers no STARTTLS
    /// (`FORGE_MAIL_ALLOW_PLAINTEXT`). Credentials still go only over TLS,
    /// or to a loopback host.
    pub allow_plaintext: bool,
    /// Deadline of one delivery attempt (`FORGE_MAIL_TIMEOUT`, default 30 s,
    /// at most 10 min).
    pub timeout: Duration,
    /// Enables the mail preview handler (`FORGE_MAIL_PREVIEW`); only valid in
    /// development and test.
    pub preview: bool,
    /// PEM bundle of extra trusted roots for a private relay
    /// (`FORGE_MAIL_TLS_CA_FILE`). There is no setting to skip verification.
    pub ca_file: Option<PathBuf>,
}

impl Default for MailConfig {
    fn default() -> Self {
        Self {
            url: None,
            from: None,
            allow_plaintext: false,
            timeout: DEFAULT_MAIL_TIMEOUT,
            preview: false,
            ca_file: None,
        }
    }
}

impl MailConfig {
    /// The transport the URL selects, or `None` when no (valid) URL is set.
    #[must_use]
    pub fn transport(&self) -> Option<MailTransport> {
        let url = self.url.as_ref()?;
        parse_url(url.expose()).ok().map(|parsed| parsed.transport)
    }

    /// The server of an `smtp://` or `smtps://` URL.
    #[must_use]
    pub fn smtp(&self) -> Option<MailEndpoint> {
        let parsed = parse_url(self.url.as_ref()?.expose()).ok()?;
        let implicit_tls = match parsed.transport {
            MailTransport::Starttls => false,
            MailTransport::ImplicitTls => true,
            MailTransport::File => return None,
        };
        let port = match parsed.port {
            Some(port) => port.parse().ok()?,
            None if implicit_tls => 465,
            None => 587,
        };
        Some(MailEndpoint {
            host: parsed.host,
            port,
            username: parsed.username,
            password: parsed.password.map(SecretString::new),
            implicit_tls,
        })
    }

    /// The directory of a `file:` URL (`file:tmp/mail` or
    /// `file:///var/tmp/mail`).
    #[must_use]
    pub fn file_path(&self) -> Option<PathBuf> {
        let parsed = parse_url(self.url.as_ref()?.expose()).ok()?;
        (parsed.transport == MailTransport::File && !parsed.path.is_empty())
            .then(|| PathBuf::from(parsed.path))
    }
}

/// Identity settings that are not secrets of the session layer.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AuthConfig {
    /// Absolute URL of the page password reset emails link to, such as
    /// `https://app.example.com/passwords/edit`
    /// (`FORGE_AUTH_PASSWORD_RESET_URL`): https, or http on a loopback host,
    /// without credentials, query or fragment. `None` disables reset emails.
    pub password_reset_url: Option<String>,
    /// The TOTP seed key (`FORGE_AUTH_MFA_KEY`), validated when loaded and
    /// redacted in diagnostics. `None` leaves MFA unconfigured: enrolling is
    /// refused and an account with a factor cannot sign in.
    pub mfa_key: Option<SecretString>,
    /// How long a session may go unused before it ends, ahead of its
    /// absolute expiry (`FORGE_AUTH_SESSION_IDLE_TIMEOUT`, default `2h`,
    /// from `5m` to `720h`; no value turns it off). Generated applications
    /// pass it to their session store through
    /// `forge::auth::SessionIdleTimeout`.
    #[serde(default = "default_session_idle_timeout")]
    pub session_idle_timeout: Duration,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            password_reset_url: None,
            mfa_key: None,
            session_idle_timeout: DEFAULT_SESSION_IDLE_TIMEOUT,
        }
    }
}

fn default_session_idle_timeout() -> Duration {
    DEFAULT_SESSION_IDLE_TIMEOUT
}

/// Parses `FORGE_AUTH_SESSION_IDLE_TIMEOUT`: a Go duration from `5m` to
/// `720h`. An empty value is refused, as in Forge-Go: it names no timeout,
/// and there is no way to turn the idle limit off.
fn parse_session_idle_timeout(value: &str) -> Result<Duration, ConfigError> {
    telemetry::parse_go_duration(value.trim())
        .filter(|timeout| (MIN_SESSION_IDLE_TIMEOUT..=MAX_SESSION_IDLE_TIMEOUT).contains(timeout))
        .ok_or_else(|| ConfigError::InvalidValue {
            key: SESSION_IDLE_TIMEOUT_KEY,
            value: value.to_owned(),
            expected: "a duration between 5m and 720h, such as 2h",
        })
}

impl AuthConfig {
    /// Decodes the configured MFA key, or `None` when none is set. The
    /// error never contains the key.
    pub fn mfa_key_bytes(&self) -> Result<Option<[u8; MFA_KEY_BYTES]>, ConfigError> {
        self.mfa_key
            .as_ref()
            .map(|key| decode_mfa_key(key.expose()))
            .transpose()
    }
}

/// Canonical unpadded base64url of exactly 32 bytes: no padding, the
/// URL-safe alphabet, no trailing bits, nothing around it.
fn decode_mfa_key(value: &str) -> Result<[u8; MFA_KEY_BYTES], ConfigError> {
    let refused = || {
        invalid(
            MFA_KEY_KEY,
            REDACTED,
            "canonical unpadded base64url of exactly 32 random bytes",
        )
    };
    let bytes = URL_SAFE_NO_PAD.decode(value).map_err(|_| refused())?;
    if URL_SAFE_NO_PAD.encode(&bytes) != value {
        return Err(refused());
    }
    <[u8; MFA_KEY_BYTES]>::try_from(bytes.as_slice()).map_err(|_| refused())
}

/// Reads the `FORGE_MAIL_*` keys and `FORGE_AUTH_PASSWORD_RESET_URL`.
pub(crate) fn apply(
    lookup: &impl Fn(&str) -> Option<String>,
    mail: &mut MailConfig,
    auth: &mut AuthConfig,
) -> Result<(), ConfigError> {
    if let Some(value) = lookup("FORGE_MAIL_URL")
        && !value.is_empty()
    {
        mail.url = Some(SecretString::new(value));
    }
    if let Some(value) = lookup("FORGE_MAIL_FROM")
        && !value.is_empty()
    {
        mail.from = Some(value);
    }
    if let Some(value) = lookup("FORGE_MAIL_ALLOW_PLAINTEXT") {
        mail.allow_plaintext = telemetry::parse_bool("FORGE_MAIL_ALLOW_PLAINTEXT", &value)?;
    }
    if let Some(value) = lookup("FORGE_MAIL_PREVIEW") {
        mail.preview = telemetry::parse_bool("FORGE_MAIL_PREVIEW", &value)?;
    }
    if let Some(value) = lookup("FORGE_MAIL_TIMEOUT") {
        let timeout = telemetry::parse_go_duration(value.trim()).ok_or_else(|| {
            ConfigError::InvalidValue {
                key: "FORGE_MAIL_TIMEOUT",
                value: value.clone(),
                expected: "a duration such as 30s",
            }
        })?;
        // Zero means the default, as in Forge-Go.
        mail.timeout = if timeout.is_zero() {
            DEFAULT_MAIL_TIMEOUT
        } else {
            timeout
        };
    }
    if let Some(value) = lookup("FORGE_MAIL_TLS_CA_FILE")
        && !value.is_empty()
    {
        if value.contains('\0') {
            return Err(invalid(
                "FORGE_MAIL_TLS_CA_FILE",
                REDACTED,
                "a path to a PEM file",
            ));
        }
        mail.ca_file = Some(PathBuf::from(value));
    }
    if let Some(value) = lookup(PASSWORD_RESET_URL_KEY)
        && !value.is_empty()
    {
        auth.password_reset_url = Some(value);
    }
    if let Some(value) = lookup(MFA_KEY_KEY)
        && !value.is_empty()
    {
        // A key that is set but invalid stops start-up in every
        // environment; the rejected value is a secret and never echoed.
        decode_mfa_key(&value)?;
        auth.mfa_key = Some(SecretString::new(value));
    }
    if let Some(value) = lookup(SESSION_IDLE_TIMEOUT_KEY) {
        auth.session_idle_timeout = parse_session_idle_timeout(&value)?;
    }
    Ok(())
}

/// Rules that involve several keys or the environment, Forge-Go's
/// `Mail.validate` and `Auth.validate`.
pub(crate) fn validate(
    mail: &MailConfig,
    auth: &AuthConfig,
    environment: Environment,
) -> Result<(), ConfigError> {
    if let Some(url) = &auth.password_reset_url {
        validate_password_reset_url(url)?;
    }
    if mail.preview && environment == Environment::Production {
        return Err(invalid(
            "FORGE_MAIL_PREVIEW",
            "true",
            "false: mail previews are only allowed when FORGE_ENV is development or test",
        ));
    }
    if mail.timeout > MAX_MAIL_TIMEOUT {
        return Err(ConfigError::InvalidValue {
            key: "FORGE_MAIL_TIMEOUT",
            value: format!("{:?}", mail.timeout),
            expected: "a positive duration of at most 10m",
        });
    }
    let Some(url) = &mail.url else {
        return Ok(());
    };
    // Errors never echo the URL: it usually carries a password.
    let parsed = parse_url(url.expose()).map_err(|problem| match problem {
        UrlProblem::Ambiguous => invalid(
            "FORGE_MAIL_URL",
            REDACTED,
            "an unambiguous URL: percent-encode \"@\" and \"#\" in passwords",
        ),
        UrlProblem::Query => invalid(
            "FORGE_MAIL_URL",
            REDACTED,
            "a URL without parameters: TLS is always required and verified",
        ),
        UrlProblem::Malformed => invalid("FORGE_MAIL_URL", REDACTED, MAIL_URL_CONTRACT),
    })?;
    let Some(from) = &mail.from else {
        return Err(invalid(
            "FORGE_MAIL_FROM",
            "(not set)",
            "a sender address whenever FORGE_MAIL_URL is set",
        ));
    };
    if parse_mailbox(from).is_err() {
        return Err(ConfigError::InvalidValue {
            key: "FORGE_MAIL_FROM",
            value: from.clone(),
            expected: "a plain ASCII email address such as \"App <app@example.com>\"",
        });
    }
    match parsed.transport {
        MailTransport::Starttls | MailTransport::ImplicitTls => {
            validate_smtp(&parsed, mail.allow_plaintext, environment)
        }
        MailTransport::File => {
            if environment == Environment::Production {
                return Err(invalid(
                    "FORGE_MAIL_URL",
                    REDACTED,
                    "an smtp:// or smtps:// server in production: the development file sender delivers nothing",
                ));
            }
            if !parsed.host.is_empty() || parsed.path.is_empty() || parsed.path == "/" {
                return Err(invalid(
                    "FORGE_MAIL_URL",
                    REDACTED,
                    "file:///absolute/dir or file:relative/dir for the file sender",
                ));
            }
            Ok(())
        }
    }
}

fn validate_smtp(
    parsed: &ParsedUrl,
    allow_plaintext: bool,
    environment: Environment,
) -> Result<(), ConfigError> {
    if parsed.host.is_empty() || parsed.host.contains(',') {
        return Err(invalid(
            "FORGE_MAIL_URL",
            REDACTED,
            "smtp://user:password@host:port with one host",
        ));
    }
    if !parsed.path.trim_matches('/').is_empty() {
        return Err(invalid("FORGE_MAIL_URL", REDACTED, "a URL without a path"));
    }
    if let Some(port) = &parsed.port {
        let valid = !port.is_empty()
            && port.bytes().all(|byte| byte.is_ascii_digit())
            && port.parse::<u16>().is_ok_and(|port| port >= 1);
        if !valid {
            return Err(invalid(
                "FORGE_MAIL_URL",
                REDACTED,
                "a port from 1 to 65535",
            ));
        }
    }
    if environment == Environment::Production
        && allow_plaintext
        && parsed.has_userinfo
        && !is_loopback_host(&parsed.host)
    {
        // SMTP AUTH PLAIN/LOGIN reveal the password to anyone who can strip
        // STARTTLS from the exchange. The sender refuses to authenticate in
        // clear to a non-loopback host anyway, so this could only ever fail.
        return Err(invalid(
            "FORGE_MAIL_ALLOW_PLAINTEXT",
            "true",
            "false in production when FORGE_MAIL_URL has credentials for a remote host: credentials are only sent over verified TLS",
        ));
    }
    if parsed.has_userinfo {
        let username = parsed.username.as_deref().unwrap_or_default();
        let password = parsed.password.as_deref().unwrap_or_default();
        if username.is_empty() || password.is_empty() {
            return Err(invalid(
                "FORGE_MAIL_URL",
                REDACTED,
                "credentials with both a user name and a password",
            ));
        }
        if username.chars().any(char::is_control) || password.chars().any(char::is_control) {
            return Err(invalid(
                "FORGE_MAIL_URL",
                REDACTED,
                "credentials without control characters",
            ));
        }
    }
    Ok(())
}

/// Checks a password-reset URL (`FORGE_AUTH_PASSWORD_RESET_URL`): an
/// absolute `https` URL, or `http` on a local host, with a valid host and
/// port and without credentials, query or fragment. Applications that take
/// the URL from somewhere else than the environment (for example
/// `forge_mail::PasswordResetEmails::new`) call this so both paths accept
/// the same URLs.
pub fn validate_password_reset_url(url: &str) -> Result<(), ConfigError> {
    let shape = || {
        invalid(
            PASSWORD_RESET_URL_KEY,
            "(redacted URL)",
            "an absolute URL without credentials, query or fragment, such as https://app.example.com/passwords/edit",
        )
    };
    let parsed = parse_web_url(url).ok_or_else(shape)?;
    if parsed.has_userinfo || parsed.query || parsed.fragment || parsed.host.is_empty() {
        return Err(shape());
    }
    let secure =
        parsed.scheme == "https" || (parsed.scheme == "http" && is_local_web_host(&parsed.host));
    if !secure {
        return Err(ConfigError::InvalidValue {
            key: PASSWORD_RESET_URL_KEY,
            value: url.to_owned(),
            expected: "an https:// URL (http is allowed only for localhost)",
        });
    }
    Ok(())
}

/// Whether a reset URL host is local: `localhost`, `*.localhost`,
/// `127.0.0.1` or `::1` (Forge-Go's rule for reset links).
#[must_use]
pub fn is_local_web_host(host: &str) -> bool {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host == "localhost" || host == "127.0.0.1" || host == "::1" || host.ends_with(".localhost")
}

fn invalid(key: &'static str, value: &str, expected: &'static str) -> ConfigError {
    ConfigError::InvalidValue {
        key,
        value: value.to_owned(),
        expected,
    }
}

/// Whether `host` is `localhost`, a 127.0.0.0/8 address or `::1`: a
/// destination traffic never leaves the machine for. Other host names are
/// not resolved, so they are never loopback here.
#[must_use]
pub fn is_loopback_host(host: &str) -> bool {
    let host = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if host == "localhost" {
        return true;
    }
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(address)) => address.is_loopback(),
        Ok(IpAddr::V6(address)) => {
            address == Ipv6Addr::LOCALHOST
                || address.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        }
        Err(_) => false,
    }
}

#[derive(Debug)]
enum UrlProblem {
    Malformed,
    Ambiguous,
    Query,
}

#[derive(Debug)]
struct ParsedUrl {
    transport: MailTransport,
    has_userinfo: bool,
    username: Option<String>,
    password: Option<String>,
    host: String,
    port: Option<String>,
    path: String,
}

/// Parses `FORGE_MAIL_URL` like Go's `url.Parse` does for these schemes:
/// the scheme is case-insensitive, the authority ends at the first `/`,
/// `?` or `#` and its user information at the last `@`, and the user
/// information and an opaque `file:` path are percent-decoded.
fn parse_url(raw: &str) -> Result<ParsedUrl, UrlProblem> {
    if raw
        .chars()
        .any(|character| character.is_control() || character == ' ')
    {
        return Err(UrlProblem::Malformed);
    }
    let (rest, fragment) = raw.split_once('#').unwrap_or((raw, ""));
    let (rest, query) = rest.split_once('?').unwrap_or((rest, ""));
    let (scheme, after) = rest.split_once(':').ok_or(UrlProblem::Malformed)?;
    let transport = match scheme.to_ascii_lowercase().as_str() {
        "smtp" => MailTransport::Starttls,
        "smtps" => MailTransport::ImplicitTls,
        "file" => MailTransport::File,
        _ => return Err(UrlProblem::Malformed),
    };
    let mut parsed = ParsedUrl {
        transport,
        has_userinfo: false,
        username: None,
        password: None,
        host: String::new(),
        port: None,
        path: String::new(),
    };
    match after.strip_prefix("//") {
        Some(hierarchical) => {
            let (authority, path) = match hierarchical.find('/') {
                Some(index) => hierarchical.split_at(index),
                None => (hierarchical, ""),
            };
            if let Some((userinfo, host)) = authority.rsplit_once('@') {
                parsed.has_userinfo = true;
                let (user, password) = match userinfo.split_once(':') {
                    Some((user, password)) => (user, Some(password)),
                    None => (userinfo, None),
                };
                parsed.username = Some(percent_decode(user).ok_or(UrlProblem::Malformed)?);
                parsed.password = password
                    .map(|password| percent_decode(password).ok_or(UrlProblem::Malformed))
                    .transpose()?;
                split_host_port(host, &mut parsed)?;
            } else {
                split_host_port(authority, &mut parsed)?;
            }
            parsed.path = percent_decode(path).ok_or(UrlProblem::Malformed)?;
            if path.contains('@') {
                return Err(UrlProblem::Ambiguous);
            }
        }
        None => {
            // An opaque URL such as `file:tmp/mail`.
            if transport != MailTransport::File || after.is_empty() {
                return Err(UrlProblem::Malformed);
            }
            parsed.path = percent_decode(after).ok_or(UrlProblem::Malformed)?;
            if after.contains('@') {
                return Err(UrlProblem::Ambiguous);
            }
        }
    }
    if !fragment.is_empty() || raw.ends_with('#') || query.contains('@') {
        return Err(UrlProblem::Ambiguous);
    }
    if !query.is_empty() || raw.contains('?') {
        return Err(UrlProblem::Query);
    }
    Ok(parsed)
}

fn split_host_port(authority: &str, parsed: &mut ParsedUrl) -> Result<(), UrlProblem> {
    if let Some(bracketed) = authority.strip_prefix('[') {
        let (literal, rest) = bracketed.split_once(']').ok_or(UrlProblem::Malformed)?;
        literal
            .parse::<Ipv6Addr>()
            .map_err(|_| UrlProblem::Malformed)?;
        parsed.host = literal.to_owned();
        if !rest.is_empty() {
            parsed.port = Some(
                rest.strip_prefix(':')
                    .ok_or(UrlProblem::Malformed)?
                    .to_owned(),
            );
        }
        return Ok(());
    }
    let (host, port) = match authority.split_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (authority, None),
    };
    if port.is_some_and(|port| port.contains(':')) {
        return Err(UrlProblem::Malformed);
    }
    if host
        .bytes()
        .any(|byte| !(byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b',')))
    {
        return Err(UrlProblem::Malformed);
    }
    parsed.host = host.to_owned();
    parsed.port = port.map(str::to_owned);
    Ok(())
}

/// `%XX` decoding; `None` for a malformed escape or a result that is not
/// UTF-8.
fn percent_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let high = char::from(*bytes.get(index + 1)?).to_digit(16)?;
            let low = char::from(*bytes.get(index + 2)?).to_digit(16)?;
            out.push(u8::try_from(high * 16 + low).ok()?);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).ok()
}

struct WebUrl {
    scheme: String,
    has_userinfo: bool,
    host: String,
    query: bool,
    fragment: bool,
}

/// A minimal absolute http(s) URL reader for `FORGE_AUTH_PASSWORD_RESET_URL`.
fn parse_web_url(raw: &str) -> Option<WebUrl> {
    if raw
        .chars()
        .any(|character| character.is_control() || character == ' ')
    {
        return None;
    }
    let (scheme, rest) = raw.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let fragment = rest.contains('#');
    let query = rest.contains('?');
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let has_userinfo = authority.contains('@');
    let host_port = authority.rsplit('@').next().unwrap_or_default();
    let host = if let Some(bracketed) = host_port.strip_prefix('[') {
        let (literal, rest) = bracketed.split_once(']')?;
        literal.parse::<Ipv6Addr>().ok()?;
        if !rest.is_empty() && !valid_port(rest.strip_prefix(':')?) {
            return None;
        }
        literal.to_owned()
    } else {
        let (host, port) = match host_port.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (host_port, None),
        };
        if port.is_some_and(|port| !valid_port(port))
            || !host
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
        {
            return None;
        }
        host.to_ascii_lowercase()
    };
    Some(WebUrl {
        scheme,
        has_userinfo,
        host,
        query,
        fragment,
    })
}

fn valid_port(port: &str) -> bool {
    !port.is_empty()
        && port.bytes().all(|byte| byte.is_ascii_digit())
        && port.parse::<u16>().is_ok_and(|port| port >= 1)
}

/// Why a mailbox cannot be used. The reason is a static sentence; it never
/// contains the offending value.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[error("{reason}")]
pub struct MailboxError {
    /// What is wrong, e.g. `email must contain exactly one @`.
    pub reason: &'static str,
    /// Input that is valid in the wild but outside what Forge implements:
    /// addresses with non-ASCII characters (SMTPUTF8 and IDNA are not
    /// supported). Callers report it distinctly.
    pub unsupported: bool,
}

const fn mailbox_error(reason: &'static str) -> MailboxError {
    MailboxError {
        reason,
        unsupported: false,
    }
}

/// Checks a display name and an addr-spec, Forge-Go's
/// `config.ValidateMailbox`.
///
/// The address must be ASCII dot-atom `local@domain` with an LDH host name;
/// quoted local parts, comments, route addresses, domain literals and
/// display forms are rejected, which removes the ambiguity used to smuggle
/// recipients or SMTP commands. The display name may be any text without
/// control, line-break or bidirectional-override characters.
///
/// ```
/// use forge_config::validate_mailbox;
///
/// assert!(validate_mailbox("Ada", "ada@example.com").is_ok());
/// assert!(validate_mailbox("", "ada@example.com\r\nBcc: eve@example.com").is_err());
/// assert!(validate_mailbox("", "joão@example.com").is_err_and(|error| error.unsupported));
/// ```
pub fn validate_mailbox(name: &str, email: &str) -> Result<(), MailboxError> {
    if email.is_empty() {
        return Err(mailbox_error("email is required"));
    }
    if has_text_problem(name) {
        return Err(mailbox_error(
            "display name contains a control, line-break or direction-override character",
        ));
    }
    if name.chars().count() > MAX_NAME_CHARS {
        return Err(mailbox_error("display name is too long"));
    }
    if has_text_problem(email) {
        return Err(mailbox_error(
            "email contains a control, line-break or direction-override character",
        ));
    }
    if email.len() > MAX_MAILBOX_BYTES {
        return Err(mailbox_error("email is too long"));
    }
    for byte in email.bytes() {
        if !byte.is_ascii() {
            return Err(MailboxError {
                reason: "email has non-ASCII characters (internationalised addresses are not supported)",
                unsupported: true,
            });
        }
        if byte == b' ' {
            return Err(mailbox_error("email must not contain spaces"));
        }
    }
    let Some((local, domain)) = email.split_once('@') else {
        return Err(mailbox_error("email must contain exactly one @"));
    };
    if domain.contains('@') {
        return Err(mailbox_error("email must contain exactly one @"));
    }
    if !valid_local_part(local) {
        return Err(mailbox_error("email local part is not a plain dot-atom"));
    }
    if !valid_mail_domain(domain) {
        return Err(mailbox_error("email domain is not a valid host name"));
    }
    Ok(())
}

/// Parses `user@example.com` or `Display Name <user@example.com>` (the
/// name optionally quoted) and validates the result with
/// [`validate_mailbox`]. Returns the display name and the address.
///
/// Only the forms Forge itself writes are read: comments, groups, route
/// addresses and encoded words are not interpreted (a `=?...?=` name is
/// kept literally and re-encoded on output).
pub fn parse_mailbox(value: &str) -> Result<(String, String), MailboxError> {
    if has_text_problem(value) {
        return Err(mailbox_error(
            "mailbox contains a control, line-break or direction-override character",
        ));
    }
    let invalid = || mailbox_error("mailbox is not valid");
    let value = value.trim();
    let (name, email) = match value.strip_suffix('>') {
        Some(rest) => {
            let (name, email) = rest.rsplit_once('<').ok_or_else(invalid)?;
            (parse_display_name(name.trim()).ok_or_else(invalid)?, email)
        }
        None => (String::new(), value),
    };
    if email.contains(['<', '>', '"', '(', ')', ',', ';', ':', '[', ']', '\\']) {
        return Err(invalid());
    }
    validate_mailbox(&name, email)?;
    Ok((name, email.to_owned()))
}

/// A quoted string (with backslash escapes) or a phrase of words without
/// RFC 5322 specials.
fn parse_display_name(name: &str) -> Option<String> {
    if let Some(quoted) = name.strip_prefix('"') {
        let inner = quoted.strip_suffix('"')?;
        let mut out = String::with_capacity(inner.len());
        let mut characters = inner.chars();
        while let Some(character) = characters.next() {
            match character {
                '\\' => out.push(characters.next()?),
                '"' => return None,
                other => out.push(other),
            }
        }
        return Some(out);
    }
    if name
        .chars()
        .any(|character| "()<>[]:;@\\,\"".contains(character))
    {
        return None;
    }
    // Runs of spaces between words collapse, as a parser unfolds them.
    Some(name.split_whitespace().collect::<Vec<_>>().join(" "))
}

/// Whether `domain` is an LDH host name of up to 253 bytes. A single label
/// such as `localhost` is allowed for development.
#[must_use]
pub fn valid_mail_domain(domain: &str) -> bool {
    if domain.is_empty() || domain.len() > MAX_DOMAIN_BYTES {
        return false;
    }
    domain.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= MAX_LABEL_BYTES
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    })
}

/// RFC 5322 dot-atom: atext separated by single dots.
fn valid_local_part(local: &str) -> bool {
    !local.is_empty()
        && local.len() <= MAX_LOCAL_BYTES
        && !local.starts_with('.')
        && !local.ends_with('.')
        && !local.contains("..")
        && local.bytes().all(|byte| is_atext(byte) || byte == b'.')
}

fn is_atext(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"!#$%&'*+-/=?^_`{|}~".contains(&byte)
}

/// Whether text has a character that could end or split a header line
/// (every control character, U+2028 and U+2029), reorder displayed text
/// (bidirectional overrides and isolates) or stands for invalid input
/// (U+FFFD).
#[must_use]
pub fn has_text_problem(text: &str) -> bool {
    text.chars().any(|character| {
        character.is_control()
            || is_bidi_control(character)
            || matches!(character, '\u{2028}' | '\u{2029}' | '\u{FFFD}')
    })
}

/// The bidirectional overrides, isolates and marks: U+202A-202E,
/// U+2066-2069, U+200E, U+200F and U+061C.
#[must_use]
pub fn is_bidi_control(character: char) -> bool {
    matches!(character, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{200E}' | '\u{200F}' | '\u{061C}')
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::AppConfig;

    fn load(pairs: &[(&str, &str)]) -> Result<AppConfig, ConfigError> {
        let values: HashMap<String, String> = pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect();
        AppConfig::from_lookup(|key| values.get(key).cloned())
    }

    fn rejected(pairs: &[(&str, &str)]) -> ConfigError {
        load(pairs).expect_err("must be rejected")
    }

    /// Two hours unless `FORGE_AUTH_SESSION_IDLE_TIMEOUT` says otherwise,
    /// within five minutes and 720 hours; no value turns the idle limit off
    /// (Forge-Go's `TestSessionIdleTimeout`).
    #[test]
    fn the_session_idle_timeout_defaults_to_two_hours_and_is_bounded() {
        assert_eq!(
            load(&[]).expect("defaults").auth.session_idle_timeout,
            Duration::from_secs(7200)
        );
        assert_eq!(
            AuthConfig::default().session_idle_timeout,
            Duration::from_secs(7200)
        );
        for (value, minutes) in [
            ("5m", 5),
            ("30m", 30),
            ("2h", 120),
            ("720h", 720 * 60),
            (" 45m ", 45),
        ] {
            let config = load(&[(SESSION_IDLE_TIMEOUT_KEY, value)]).expect("valid");
            assert_eq!(
                config.auth.session_idle_timeout,
                Duration::from_secs(minutes * 60),
                "{value}"
            );
        }
        for value in ["4m59s", "721h", "0", "-1h", "two hours", "", "2"] {
            let error = rejected(&[(SESSION_IDLE_TIMEOUT_KEY, value)]);
            let message = error.to_string();
            assert!(
                message.contains(SESSION_IDLE_TIMEOUT_KEY)
                    && message.contains("between 5m and 720h"),
                "{value:?}: {message}"
            );
        }
        // A configuration serialized before the key existed still loads,
        // with the default.
        let legacy: AuthConfig =
            serde_json::from_str(r#"{"password_reset_url":null,"mfa_key":null}"#)
                .expect("deserializes");
        assert_eq!(legacy.session_idle_timeout, DEFAULT_SESSION_IDLE_TIMEOUT);
    }

    #[test]
    fn the_mfa_key_is_optional_canonical_and_never_echoed() {
        let key = URL_SAFE_NO_PAD.encode([7_u8; 32]);
        for environment in ["development", "test", "production"] {
            let unset = load(&[("FORGE_ENV", environment)]).expect("optional");
            assert_eq!(unset.auth.mfa_key, None);
            assert_eq!(unset.auth.mfa_key_bytes(), Ok(None));
            let empty = load(&[("FORGE_ENV", environment), (MFA_KEY_KEY, "")]).expect("empty");
            assert_eq!(empty.auth.mfa_key, None);
            let set = load(&[("FORGE_ENV", environment), (MFA_KEY_KEY, &key)]).expect("valid");
            assert_eq!(set.auth.mfa_key_bytes(), Ok(Some([7_u8; 32])));
            assert!(!format!("{set:?}").contains(&key));
            assert!(
                !serde_json::to_string(&set)
                    .expect("serializes")
                    .contains(&key)
            );
        }
        let padded = format!("{key}=");
        let standard = STANDARD_KEY;
        let short = URL_SAFE_NO_PAD.encode([1_u8; 31]);
        let long = URL_SAFE_NO_PAD.encode([1_u8; 33]);
        let spaced = format!(" {key}");
        let mut trailing_bits = key.clone();
        trailing_bits.pop();
        trailing_bits.push('B');
        for bad in [
            padded.as_str(),
            standard,
            short.as_str(),
            long.as_str(),
            spaced.as_str(),
            trailing_bits.as_str(),
            "not a key",
        ] {
            for environment in ["development", "production"] {
                let error = rejected(&[("FORGE_ENV", environment), (MFA_KEY_KEY, bad)]);
                let text = error.to_string();
                assert!(text.contains(MFA_KEY_KEY), "{text}");
                assert!(text.contains("[REDACTED]"), "{text}");
                assert!(!text.contains(bad.trim()), "{text}");
            }
        }
    }

    /// 32 bytes in the standard (not URL-safe) alphabet.
    const STANDARD_KEY: &str = "+/+/+/+/+/+/+/+/+/+/+/+/+/+/+/+/+/+/+/+/+/8";

    #[test]
    fn mail_is_disabled_by_default() {
        let config = load(&[]).expect("defaults");
        assert_eq!(config.mail, MailConfig::default());
        assert_eq!(config.mail.transport(), None);
        assert_eq!(config.mail.timeout, Duration::from_secs(30));
        assert_eq!(config.auth.password_reset_url, None);
    }

    #[test]
    fn smtp_urls_select_starttls_or_implicit_tls_with_default_ports() {
        let config = load(&[
            (
                "FORGE_MAIL_URL",
                "smtp://user:p%40ss%3Aword@smtp.example.com",
            ),
            ("FORGE_MAIL_FROM", "App <app@example.com>"),
        ])
        .expect("valid");
        assert_eq!(config.mail.transport(), Some(MailTransport::Starttls));
        let endpoint = config.mail.smtp().expect("smtp");
        assert_eq!(endpoint.host, "smtp.example.com");
        assert_eq!(endpoint.port, 587);
        assert!(!endpoint.implicit_tls);
        assert_eq!(endpoint.username.as_deref(), Some("user"));
        assert_eq!(
            endpoint.password.as_ref().map(SecretString::expose),
            Some("p@ss:word")
        );

        let config = load(&[
            ("FORGE_MAIL_URL", "SMTPS://smtp.example.com:2465"),
            ("FORGE_MAIL_FROM", "app@example.com"),
        ])
        .expect("valid");
        let endpoint = config.mail.smtp().expect("smtps");
        assert_eq!((endpoint.port, endpoint.implicit_tls), (2465, true));
        assert_eq!(endpoint.username, None);

        let config = load(&[
            ("FORGE_MAIL_URL", "smtp://[::1]:1025"),
            ("FORGE_MAIL_FROM", "app@example.com"),
        ])
        .expect("valid");
        assert_eq!(config.mail.smtp().expect("smtp").host, "::1");
    }

    #[test]
    fn file_urls_name_a_directory_outside_production() {
        for (url, dir) in [
            ("file:tmp/mail", "tmp/mail"),
            ("file:///var/tmp/forge-mail", "/var/tmp/forge-mail"),
        ] {
            let config = load(&[
                ("FORGE_MAIL_URL", url),
                ("FORGE_MAIL_FROM", "app@example.com"),
            ])
            .expect(url);
            assert_eq!(config.mail.transport(), Some(MailTransport::File));
            assert_eq!(config.mail.file_path(), Some(PathBuf::from(dir)));
            assert_eq!(config.mail.smtp(), None);
        }
        let error = rejected(&[
            ("FORGE_ENV", "production"),
            ("FORGE_MAIL_URL", "file:///var/tmp/mail"),
            ("FORGE_MAIL_FROM", "app@example.com"),
        ]);
        assert!(
            error.to_string().contains("development file sender"),
            "{error}"
        );
        for url in ["file://host/dir", "file:///", "file:"] {
            rejected(&[
                ("FORGE_MAIL_URL", url),
                ("FORGE_MAIL_FROM", "app@example.com"),
            ]);
        }
    }

    #[test]
    fn malformed_ambiguous_or_parameterised_urls_are_refused_without_echoing_them() {
        for url in [
            "http://user:secret-password@smtp.example.com",
            "smtp://user:secret-password@smtp.example.com/path",
            "smtp://user:secret-password@smtp.example.com:0",
            "smtp://user:secret-password@smtp.example.com:70000",
            "smtp://user:secret-password@smtp.example.com:x",
            "smtp://user:secret-password@a.example.com,b.example.com",
            "smtp://user:secret-password@smtp.example.com?tls=skip-verify",
            "smtp://user:secret-password#x@smtp.example.com",
            "smtp://user:secret-password@smtp.example.com/a@b",
            "smtp://user:secret-password@",
            "smtp://user:secret-password@smtp.example.com:587\r\nRCPT",
            "smtp://user:secret%ZZpassword@smtp.example.com",
            "smtp://user:secret%0d%0apassword@smtp.example.com",
            "smtp://user@smtp.example.com",
            "smtp://:secret-password@smtp.example.com",
            "smtp://user:@smtp.example.com",
        ] {
            let error = rejected(&[
                ("FORGE_MAIL_URL", url),
                ("FORGE_MAIL_FROM", "app@example.com"),
            ]);
            let text = error.to_string();
            assert!(!text.contains("secret"), "{url}: {text}");
            assert!(text.contains("FORGE_MAIL"), "{url}: {text}");
        }
    }

    #[test]
    fn a_url_needs_a_valid_sender() {
        let error = rejected(&[("FORGE_MAIL_URL", "smtp://smtp.example.com")]);
        assert!(error.to_string().contains("FORGE_MAIL_FROM"), "{error}");
        for from in [
            "not an address",
            "app@example.com\r\nBcc: eve@example.com",
            "joão@example.com",
            "\"App\" <app@[127.0.0.1]>",
            "a@b@example.com",
        ] {
            let error = rejected(&[
                ("FORGE_MAIL_URL", "smtp://smtp.example.com"),
                ("FORGE_MAIL_FROM", from),
            ]);
            assert!(error.to_string().contains("FORGE_MAIL_FROM"), "{error}");
        }
    }

    #[test]
    fn production_refuses_plaintext_with_remote_credentials_and_previews() {
        let error = rejected(&[
            ("FORGE_ENV", "production"),
            ("FORGE_MAIL_URL", "smtp://user:pass@smtp.example.com"),
            ("FORGE_MAIL_FROM", "app@example.com"),
            ("FORGE_MAIL_ALLOW_PLAINTEXT", "true"),
        ]);
        assert!(
            error.to_string().contains("FORGE_MAIL_ALLOW_PLAINTEXT"),
            "{error}"
        );
        // Loopback relays and credential-free URLs remain possible.
        for url in ["smtp://user:pass@127.0.0.1:25", "smtp://smtp.example.com"] {
            load(&[
                ("FORGE_ENV", "production"),
                ("FORGE_MAIL_URL", url),
                ("FORGE_MAIL_FROM", "app@example.com"),
                ("FORGE_MAIL_ALLOW_PLAINTEXT", "true"),
            ])
            .expect(url);
        }
        let error = rejected(&[("FORGE_ENV", "production"), ("FORGE_MAIL_PREVIEW", "true")]);
        assert!(error.to_string().contains("FORGE_MAIL_PREVIEW"), "{error}");
        for environment in ["development", "test"] {
            let config =
                load(&[("FORGE_ENV", environment), ("FORGE_MAIL_PREVIEW", "true")]).expect("dev");
            assert!(config.mail.preview);
        }
        rejected(&[("FORGE_MAIL_PREVIEW", "yes")]);
    }

    #[test]
    fn timeouts_are_bounded_and_zero_means_the_default() {
        let config = load(&[("FORGE_MAIL_TIMEOUT", "1m30s")]).expect("valid");
        assert_eq!(config.mail.timeout, Duration::from_secs(90));
        let config = load(&[("FORGE_MAIL_TIMEOUT", "0")]).expect("valid");
        assert_eq!(config.mail.timeout, Duration::from_secs(30));
        for value in ["11m", "-1s", "soon"] {
            rejected(&[("FORGE_MAIL_TIMEOUT", value)]);
        }
    }

    #[test]
    fn secrets_are_redacted_in_debug_and_serialization() {
        let config = load(&[
            (
                "FORGE_MAIL_URL",
                "smtps://user:hunter2-secret@smtp.example.com",
            ),
            ("FORGE_MAIL_FROM", "app@example.com"),
        ])
        .expect("valid");
        assert!(!format!("{config:?}").contains("hunter2"));
        let json = serde_json::to_string(&config).expect("serializable");
        assert!(!json.contains("hunter2"), "{json}");
    }

    #[test]
    fn reset_urls_are_absolute_https_or_local_http() {
        for url in [
            "https://app.example.com/passwords/edit",
            "http://localhost:3000/passwords/edit",
            "http://127.0.0.1:3000/passwords/edit",
            "http://[::1]:3000/passwords/edit",
            "http://shop.localhost/passwords/edit",
        ] {
            let config = load(&[(PASSWORD_RESET_URL_KEY, url)]).expect(url);
            assert_eq!(config.auth.password_reset_url.as_deref(), Some(url));
        }
        for url in [
            "http://app.example.com/passwords/edit",
            "https://user:pw@app.example.com/passwords/edit",
            "https://app.example.com/passwords/edit?x=1",
            "https://app.example.com/passwords/edit#x",
            "/passwords/edit",
            "javascript:alert(1)",
            "https:///passwords/edit",
        ] {
            let error = rejected(&[(PASSWORD_RESET_URL_KEY, url)]);
            assert!(error.to_string().contains(PASSWORD_RESET_URL_KEY), "{url}");
        }
    }

    #[test]
    fn loopback_hosts() {
        for host in [
            "localhost",
            "LOCALHOST.",
            "127.0.0.1",
            "127.8.9.10",
            "::1",
            "[::1]",
        ] {
            assert!(is_loopback_host(host), "{host}");
        }
        for host in [
            "localhost.example.com",
            "10.0.0.1",
            "example.com",
            "::2",
            "0.0.0.0",
        ] {
            assert!(!is_loopback_host(host), "{host}");
        }
    }

    #[test]
    fn mailboxes_follow_forge_go_rules() {
        assert_eq!(
            parse_mailbox("App <app@example.com>"),
            Ok(("App".to_owned(), "app@example.com".to_owned()))
        );
        assert_eq!(
            parse_mailbox("\"Doe, \\\"Jane\\\"\" <jane@example.com>"),
            Ok(("Doe, \"Jane\"".to_owned(), "jane@example.com".to_owned()))
        );
        assert_eq!(
            parse_mailbox("  plain.user+tag@example.co  "),
            Ok((String::new(), "plain.user+tag@example.co".to_owned()))
        );
        assert_eq!(
            parse_mailbox("João Silva <joao@example.com.br>"),
            Ok(("João Silva".to_owned(), "joao@example.com.br".to_owned()))
        );
        for value in [
            "",
            "app",
            "app@",
            "@example.com",
            "a..b@example.com",
            ".a@example.com",
            "a.@example.com",
            "\"quoted\"@example.com",
            "app@example.com (comment)",
            "app@[127.0.0.1]",
            "app@-example.com",
            "app@example-.com",
            "app@exa_mple.com",
            "App <app@example.com",
            "Team: a@example.com;",
            "a@example.com, b@example.com",
            "<a@example.com> <b@example.com>",
            "App\u{202E} <app@example.com>",
            "App\r\n <app@example.com>",
        ] {
            assert!(parse_mailbox(value).is_err(), "{value:?}");
        }
        let error = validate_mailbox("", "user@exämple.com").expect_err("unsupported");
        assert!(error.unsupported);
        assert!(!error.to_string().contains("ex"), "{error}");
        assert!(validate_mailbox(&"n".repeat(256), "a@example.com").is_err());
        assert!(validate_mailbox("", &format!("{}@example.com", "a".repeat(65))).is_err());
        assert!(validate_mailbox("", &format!("a@{}.com", "b".repeat(64))).is_err());
        assert!(validate_mailbox("", "localuser@localhost").is_ok());
    }
}
