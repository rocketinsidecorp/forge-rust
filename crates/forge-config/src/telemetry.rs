//! Telemetry settings: the `FORGE_TELEMETRY_*` variables (ADR 0033).
//!
//! The keys, defaults and rules are Forge-Go's (`config/telemetry.go`, ADR
//! 0034 and 0040 there), so one set of variables configures both frameworks.
//! Durations use Go's duration syntax (`10s`, `1m30s`, `500ms`).

use std::{
    fmt,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    time::Duration,
};

use serde::{Deserialize, Serialize};

use crate::{ConfigError, Environment, REDACTED, SecretString};

/// `FORGE_TELEMETRY_ENABLED`.
pub const TELEMETRY_ENABLED_KEY: &str = "FORGE_TELEMETRY_ENABLED";

/// Every `FORGE_TELEMETRY_*` variable, in the order the help lists them.
pub const TELEMETRY_KEYS: &[&str] = &[
    TELEMETRY_ENABLED_KEY,
    "FORGE_TELEMETRY_SERVICE_NAME",
    "FORGE_TELEMETRY_SERVICE_VERSION",
    "FORGE_TELEMETRY_EXPORTER",
    "FORGE_TELEMETRY_LOGS_EXPORTER",
    "FORGE_TELEMETRY_LOGS_LEVEL",
    "FORGE_TELEMETRY_OTLP_ENDPOINT",
    "FORGE_TELEMETRY_OTLP_HEADERS",
    "FORGE_TELEMETRY_OTLP_TIMEOUT",
    "FORGE_TELEMETRY_OTLP_COMPRESSION",
    "FORGE_TELEMETRY_OTLP_ALLOW_PLAINTEXT",
    "FORGE_TELEMETRY_SAMPLE_RATIO",
    "FORGE_TELEMETRY_TRUST_TRACEPARENT",
    "FORGE_TELEMETRY_METRICS_ADDR",
    "FORGE_TELEMETRY_METRICS_PUBLIC",
    "FORGE_TELEMETRY_METRICS_INTERVAL",
];

const DEFAULT_OTLP_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_OTLP_TIMEOUT: Duration = Duration::from_secs(60);
const DEFAULT_METRICS_INTERVAL: Duration = Duration::from_secs(60);
const MIN_METRICS_INTERVAL: Duration = Duration::from_secs(1);
const MAX_SERVICE_NAME_LENGTH: usize = 255;
/// Headers the exporter sets itself; `FORGE_TELEMETRY_OTLP_HEADERS` cannot
/// replace them.
const RESERVED_OTLP_HEADERS: &[&str] = &[
    "content-type",
    "content-encoding",
    "content-length",
    "host",
    "transfer-encoding",
    "connection",
];
const ENDPOINT_CONTRACT: &str = "an http:// or https:// URL without credentials, query or fragment, such as http://127.0.0.1:4318";

/// Where sampled spans and pushed metrics go (`FORGE_TELEMETRY_EXPORTER`).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TelemetryExporter {
    /// Nothing is exported: traces still correlate logs and propagate, and
    /// metrics are served by the metrics listener when one is configured.
    #[default]
    None,
    /// OTLP/HTTP with the JSON encoding to `FORGE_TELEMETRY_OTLP_ENDPOINT`.
    Otlp,
    /// Spans are written to the application log. Development only.
    Log,
}

/// Where application log records go in addition to standard output
/// (`FORGE_TELEMETRY_LOGS_EXPORTER`).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TelemetryLogsExporter {
    /// Standard output only.
    #[default]
    None,
    /// Also OTLP/HTTP `/v1/logs`, with the `FORGE_TELEMETRY_OTLP_*` settings.
    Otlp,
}

/// Lowest level sent to the log exporter (`FORGE_TELEMETRY_LOGS_LEVEL`).
/// Standard output keeps `FORGE_LOG`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TelemetryLogLevel {
    /// `debug` and above.
    Debug,
    /// `info` and above (default).
    #[default]
    Info,
    /// `warn` and above.
    Warn,
    /// `error` only.
    Error,
}

/// OTLP request body compression (`FORGE_TELEMETRY_OTLP_COMPRESSION`).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OtlpCompression {
    /// Uncompressed JSON.
    #[default]
    None,
    /// `Content-Encoding: gzip`.
    Gzip,
}

/// Fraction of new traces that are sampled, `0` to `1`
/// (`FORGE_TELEMETRY_SAMPLE_RATIO`). Never NaN, so it is `Eq`.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(try_from = "f64", into = "f64")]
pub struct SampleRatio(f64);

impl SampleRatio {
    /// Every trace is sampled.
    pub const ALWAYS: Self = Self(1.0);

    /// A ratio between `0` and `1` inclusive; `None` otherwise (and for NaN).
    #[must_use]
    pub fn new(ratio: f64) -> Option<Self> {
        (0.0..=1.0).contains(&ratio).then_some(Self(ratio))
    }

    /// The ratio.
    #[must_use]
    pub const fn get(self) -> f64 {
        self.0
    }
}

impl Default for SampleRatio {
    fn default() -> Self {
        Self::ALWAYS
    }
}

impl PartialEq for SampleRatio {
    fn eq(&self, other: &Self) -> bool {
        self.0.to_bits() == other.0.to_bits()
    }
}

impl Eq for SampleRatio {}

impl TryFrom<f64> for SampleRatio {
    type Error = &'static str;

    fn try_from(value: f64) -> Result<Self, Self::Error> {
        Self::new(value).ok_or("a sample ratio between 0 and 1")
    }
}

impl From<SampleRatio> for f64 {
    fn from(value: SampleRatio) -> Self {
        value.0
    }
}

/// One `FORGE_TELEMETRY_OTLP_HEADERS` entry. The name is a lowercase HTTP
/// token; the value is secret and redacted in every diagnostic.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OtlpHeader {
    /// Lowercase header name.
    pub name: String,
    /// Percent-decoded value.
    pub value: SecretString,
}

/// Validated telemetry settings (`FORGE_TELEMETRY_*`, docs/observability.md).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TelemetryConfig {
    /// Instrumentation on (`FORGE_TELEMETRY_ENABLED`, default `true`). When
    /// false no instrument or span exists and nothing else here is checked.
    pub enabled: bool,
    /// `service.name` (`FORGE_TELEMETRY_SERVICE_NAME`); `None` means the
    /// application name.
    pub service_name: Option<String>,
    /// `service.version` (`FORGE_TELEMETRY_SERVICE_VERSION`); `None` means the
    /// application version.
    pub service_version: Option<String>,
    /// Span and metric exporter (default `none`).
    pub exporter: TelemetryExporter,
    /// Log exporter (default `none`).
    pub logs_exporter: TelemetryLogsExporter,
    /// Lowest exported log level (default `info`).
    pub logs_level: TelemetryLogLevel,
    /// Collector base URL; Forge posts to `/v1/traces`, `/v1/metrics` and
    /// `/v1/logs`. Required by either `otlp` exporter.
    pub otlp_endpoint: Option<String>,
    /// Headers sent with every export, typically an API key. Values secret.
    pub otlp_headers: Vec<OtlpHeader>,
    /// Deadline of one export, retries included (default 10 s, at most 1 min).
    pub otlp_timeout: Duration,
    /// Body compression (default `none`).
    pub otlp_compression: OtlpCompression,
    /// Permits an `http://` endpoint in production.
    pub otlp_allow_plaintext: bool,
    /// Fraction of new traces sampled (default 1).
    pub sample_ratio: SampleRatio,
    /// Continue traces from incoming `traceparent`/`tracestate` (default
    /// false: every request starts a new trace).
    pub trust_traceparent: bool,
    /// Address of the separate Prometheus listener (`GET /metrics`); `None`
    /// (default) means no listener.
    pub metrics_addr: Option<SocketAddr>,
    /// Confirms a non-loopback [`TelemetryConfig::metrics_addr`].
    pub metrics_public: bool,
    /// OTLP metric push interval (default 1 min, at least 1 s).
    pub metrics_interval: Duration,
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            service_name: None,
            service_version: None,
            exporter: TelemetryExporter::None,
            logs_exporter: TelemetryLogsExporter::None,
            logs_level: TelemetryLogLevel::Info,
            otlp_endpoint: None,
            otlp_headers: Vec::new(),
            otlp_timeout: DEFAULT_OTLP_TIMEOUT,
            otlp_compression: OtlpCompression::None,
            otlp_allow_plaintext: false,
            sample_ratio: SampleRatio::ALWAYS,
            trust_traceparent: false,
            metrics_addr: None,
            metrics_public: false,
            metrics_interval: DEFAULT_METRICS_INTERVAL,
        }
    }
}

impl TelemetryConfig {
    /// Whether an exporter needs the OTLP settings.
    #[must_use]
    pub fn uses_otlp(&self) -> bool {
        self.exporter == TelemetryExporter::Otlp
            || self.logs_exporter == TelemetryLogsExporter::Otlp
    }

    /// Whether the metrics listener binds only to loopback.
    #[must_use]
    pub fn metrics_loopback(&self) -> bool {
        self.metrics_addr
            .is_some_and(|address| address.ip().is_loopback())
    }
}

impl fmt::Display for TelemetryExporter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::None => "none",
            Self::Otlp => "otlp",
            Self::Log => "log",
        })
    }
}

/// Reads the `FORGE_TELEMETRY_*` keys into `config`.
pub(crate) fn apply(
    lookup: &impl Fn(&str) -> Option<String>,
    config: &mut TelemetryConfig,
) -> Result<(), ConfigError> {
    if let Some(value) = lookup(TELEMETRY_ENABLED_KEY) {
        config.enabled = parse_bool(TELEMETRY_ENABLED_KEY, &value)?;
    }
    for (key, target) in [
        ("FORGE_TELEMETRY_SERVICE_NAME", &mut config.service_name),
        (
            "FORGE_TELEMETRY_SERVICE_VERSION",
            &mut config.service_version,
        ),
    ] {
        if let Some(value) = lookup(key) {
            let value = value.trim();
            if value.len() > MAX_SERVICE_NAME_LENGTH || value.chars().any(char::is_control) {
                return Err(ConfigError::InvalidValue {
                    key,
                    value: value.to_owned(),
                    expected: "at most 255 bytes without control characters",
                });
            }
            *target = (!value.is_empty()).then(|| value.to_owned());
        }
    }
    if let Some(value) = lookup("FORGE_TELEMETRY_EXPORTER") {
        config.exporter = match value.trim().to_ascii_lowercase().as_str() {
            "none" => TelemetryExporter::None,
            "otlp" => TelemetryExporter::Otlp,
            "log" => TelemetryExporter::Log,
            _ => {
                return Err(invalid(
                    "FORGE_TELEMETRY_EXPORTER",
                    value,
                    "none, otlp or log",
                ));
            }
        };
    }
    if let Some(value) = lookup("FORGE_TELEMETRY_LOGS_EXPORTER") {
        config.logs_exporter = match value.trim().to_ascii_lowercase().as_str() {
            "none" => TelemetryLogsExporter::None,
            "otlp" => TelemetryLogsExporter::Otlp,
            _ => {
                return Err(invalid(
                    "FORGE_TELEMETRY_LOGS_EXPORTER",
                    value,
                    "none or otlp",
                ));
            }
        };
    }
    if let Some(value) = lookup("FORGE_TELEMETRY_LOGS_LEVEL") {
        config.logs_level = match value.trim().to_ascii_lowercase().as_str() {
            "debug" => TelemetryLogLevel::Debug,
            "info" | "" => TelemetryLogLevel::Info,
            "warn" | "warning" => TelemetryLogLevel::Warn,
            "error" => TelemetryLogLevel::Error,
            _ => {
                return Err(invalid(
                    "FORGE_TELEMETRY_LOGS_LEVEL",
                    value,
                    "debug, info, warn or error",
                ));
            }
        };
    }
    if let Some(value) = lookup("FORGE_TELEMETRY_OTLP_ENDPOINT") {
        let value = value.trim();
        config.otlp_endpoint = (!value.is_empty()).then(|| value.to_owned());
    }
    if let Some(value) = lookup("FORGE_TELEMETRY_OTLP_HEADERS") {
        config.otlp_headers = parse_otlp_headers(&value)?;
    }
    if let Some(value) = lookup("FORGE_TELEMETRY_OTLP_TIMEOUT") {
        config.otlp_timeout = parse_duration("FORGE_TELEMETRY_OTLP_TIMEOUT", &value)?;
    }
    if let Some(value) = lookup("FORGE_TELEMETRY_OTLP_COMPRESSION") {
        config.otlp_compression = match value.trim().to_ascii_lowercase().as_str() {
            "none" | "" => OtlpCompression::None,
            "gzip" => OtlpCompression::Gzip,
            _ => {
                return Err(invalid(
                    "FORGE_TELEMETRY_OTLP_COMPRESSION",
                    value,
                    "none or gzip",
                ));
            }
        };
    }
    for (key, target) in [
        (
            "FORGE_TELEMETRY_OTLP_ALLOW_PLAINTEXT",
            &mut config.otlp_allow_plaintext,
        ),
        (
            "FORGE_TELEMETRY_TRUST_TRACEPARENT",
            &mut config.trust_traceparent,
        ),
        ("FORGE_TELEMETRY_METRICS_PUBLIC", &mut config.metrics_public),
    ] {
        if let Some(value) = lookup(key) {
            *target = parse_bool(key, &value)?;
        }
    }
    if let Some(value) = lookup("FORGE_TELEMETRY_SAMPLE_RATIO") {
        config.sample_ratio = value
            .trim()
            .parse::<f64>()
            .ok()
            .and_then(SampleRatio::new)
            .ok_or_else(|| {
                invalid(
                    "FORGE_TELEMETRY_SAMPLE_RATIO",
                    value,
                    "a number between 0 and 1",
                )
            })?;
    }
    if let Some(value) = lookup("FORGE_TELEMETRY_METRICS_ADDR") {
        let trimmed = value.trim();
        config.metrics_addr = if trimmed.is_empty() {
            None
        } else {
            Some(parse_listen_address(trimmed).ok_or_else(|| {
                invalid(
                    "FORGE_TELEMETRY_METRICS_ADDR",
                    value,
                    "host:port with an IP address or localhost, such as 127.0.0.1:9464",
                )
            })?)
        };
    }
    if let Some(value) = lookup("FORGE_TELEMETRY_METRICS_INTERVAL") {
        config.metrics_interval = parse_duration("FORGE_TELEMETRY_METRICS_INTERVAL", &value)?;
    }
    Ok(())
}

/// Rules that involve several keys, the environment or `FORGE_BIND`. A
/// disabled telemetry is not checked further, as in Forge-Go.
pub(crate) fn validate(
    config: &TelemetryConfig,
    environment: Environment,
    bind: SocketAddr,
) -> Result<(), ConfigError> {
    if !config.enabled {
        return Ok(());
    }
    if config.metrics_interval < MIN_METRICS_INTERVAL {
        return Err(ConfigError::InvalidValue {
            key: "FORGE_TELEMETRY_METRICS_INTERVAL",
            value: format_duration(config.metrics_interval),
            expected: "a duration of at least 1s",
        });
    }
    if let Some(address) = config.metrics_addr {
        let overlaps = address.port() == bind.port()
            && (address.ip() == bind.ip()
                || address.ip().is_unspecified()
                || bind.ip().is_unspecified());
        if overlaps {
            return Err(ConfigError::InvalidValue {
                key: "FORGE_TELEMETRY_METRICS_ADDR",
                value: address.to_string(),
                expected: "an address other than FORGE_BIND: metrics are never served on the application listener",
            });
        }
        if environment == Environment::Production
            && !address.ip().is_loopback()
            && !config.metrics_public
        {
            return Err(ConfigError::InvalidValue {
                key: "FORGE_TELEMETRY_METRICS_ADDR",
                value: address.to_string(),
                expected: "a loopback address in production: the metrics listener has no authentication; set FORGE_TELEMETRY_METRICS_PUBLIC=true when only a private network can reach it",
            });
        }
    }
    if config.exporter == TelemetryExporter::Log && environment == Environment::Production {
        return Err(ConfigError::InvalidValue {
            key: "FORGE_TELEMETRY_EXPORTER",
            value: config.exporter.to_string(),
            expected: "none or otlp in production (log is for development)",
        });
    }
    if config.uses_otlp() {
        validate_otlp(config, environment)?;
    }
    Ok(())
}

fn validate_otlp(config: &TelemetryConfig, environment: Environment) -> Result<(), ConfigError> {
    let Some(endpoint) = &config.otlp_endpoint else {
        return Err(ConfigError::InvalidValue {
            key: "FORGE_TELEMETRY_OTLP_ENDPOINT",
            value: "(not set)".to_owned(),
            expected: "a collector URL whenever FORGE_TELEMETRY_EXPORTER or FORGE_TELEMETRY_LOGS_EXPORTER is otlp",
        });
    };
    // The endpoint is never echoed: it may name internal hosts.
    let scheme = endpoint_scheme(endpoint).ok_or(ConfigError::InvalidValue {
        key: "FORGE_TELEMETRY_OTLP_ENDPOINT",
        value: REDACTED.to_owned(),
        expected: ENDPOINT_CONTRACT,
    })?;
    if environment == Environment::Production
        && scheme != EndpointScheme::Https
        && !config.otlp_allow_plaintext
    {
        return Err(ConfigError::InvalidValue {
            key: "FORGE_TELEMETRY_OTLP_ENDPOINT",
            value: REDACTED.to_owned(),
            expected: "an https:// URL in production unless FORGE_TELEMETRY_OTLP_ALLOW_PLAINTEXT=true",
        });
    }
    if config.otlp_timeout.is_zero() || config.otlp_timeout > MAX_OTLP_TIMEOUT {
        return Err(ConfigError::InvalidValue {
            key: "FORGE_TELEMETRY_OTLP_TIMEOUT",
            value: format_duration(config.otlp_timeout),
            expected: "a positive duration of at most 1m",
        });
    }
    Ok(())
}

/// Scheme of a valid OTLP endpoint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EndpointScheme {
    /// `http://`.
    Http,
    /// `https://`.
    Https,
}

/// Checks a collector base URL the way Forge-Go does: `http` or `https`, a
/// host, an optional port from 1 to 65535, an optional path, and no
/// credentials, query or fragment. Returns the scheme.
#[must_use]
pub fn endpoint_scheme(endpoint: &str) -> Option<EndpointScheme> {
    let (scheme, rest) = endpoint.split_once("://")?;
    let scheme = match scheme.to_ascii_lowercase().as_str() {
        "http" => EndpointScheme::Http,
        "https" => EndpointScheme::Https,
        _ => return None,
    };
    if rest.contains(['?', '#', '@', '\\'])
        || rest
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
    {
        return None;
    }
    let authority = rest.split('/').next().unwrap_or_default();
    let (host, port) = match authority.rsplit_once(':') {
        // An IPv6 literal without a port: "[::1]".
        Some((host, port)) if !port.contains(']') => (host, Some(port)),
        _ => (authority, None),
    };
    let host_ok = match host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
        Some(literal) => literal.parse::<std::net::Ipv6Addr>().is_ok(),
        None => {
            !host.is_empty()
                && host
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
        }
    };
    let port_ok = port.is_none_or(|port| port.parse::<u16>().is_ok_and(|number| number >= 1));
    (host_ok && port_ok).then_some(scheme)
}

/// Parses `name=value,name2=value2` with percent-encoded values (the
/// `OTEL_EXPORTER_OTLP_HEADERS` format). Errors name the entry's position,
/// never a value.
fn parse_otlp_headers(raw: &str) -> Result<Vec<OtlpHeader>, ConfigError> {
    let mut headers: Vec<OtlpHeader> = Vec::new();
    if raw.trim().is_empty() {
        return Ok(headers);
    }
    for (index, item) in raw.split(',').enumerate() {
        let entry = || ConfigError::InvalidValue {
            key: "FORGE_TELEMETRY_OTLP_HEADERS",
            value: format!("entry {}", index + 1),
            expected: "name=value pairs separated by commas, valid header names, percent-encoded values without control characters, no reserved or repeated header",
        };
        let (name, value) = item.split_once('=').ok_or_else(entry)?;
        let name = name.trim().to_ascii_lowercase();
        if name.is_empty() || !name.bytes().all(is_token_byte) {
            return Err(entry());
        }
        let value = percent_decode(value.trim()).ok_or_else(entry)?;
        if value
            .chars()
            .any(|character| character.is_control() && character != '\t')
        {
            return Err(entry());
        }
        if RESERVED_OTLP_HEADERS.contains(&name.as_str())
            || headers.iter().any(|header| header.name == name)
        {
            return Err(entry());
        }
        headers.push(OtlpHeader {
            name,
            value: SecretString::new(value),
        });
    }
    Ok(headers)
}

fn is_token_byte(byte: u8) -> bool {
    byte > 0x20 && byte < 0x7f && !b"\"(),/:;<=>?@[\\]{}".contains(&byte)
}

/// `%XX` decoding (Go's `url.PathUnescape`: `+` stays a plus).
fn percent_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let high = (*bytes.get(index + 1)? as char).to_digit(16)?;
            let low = (*bytes.get(index + 2)? as char).to_digit(16)?;
            out.push(u8::try_from(high * 16 + low).ok()?);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Go's `strconv.ParseBool`.
pub(crate) fn parse_bool(key: &'static str, value: &str) -> Result<bool, ConfigError> {
    match value.trim() {
        "1" | "t" | "T" | "TRUE" | "true" | "True" => Ok(true),
        "0" | "f" | "F" | "FALSE" | "false" | "False" => Ok(false),
        _ => Err(invalid(key, value.to_owned(), "true or false")),
    }
}

/// `localhost:PORT` or an IP socket address.
fn parse_listen_address(value: &str) -> Option<SocketAddr> {
    if let Some(port) = value.strip_prefix("localhost:") {
        return Some(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            port.parse::<u16>().ok().filter(|port| *port != 0)?,
        ));
    }
    value
        .parse::<SocketAddr>()
        .ok()
        .filter(|address| address.port() != 0)
}

fn parse_duration(key: &'static str, value: &str) -> Result<Duration, ConfigError> {
    parse_go_duration(value.trim()).ok_or_else(|| {
        invalid(
            key,
            value.to_owned(),
            "a duration such as 10s, 500ms or 1m30s",
        )
    })
}

/// Parses Go's duration syntax (`time.ParseDuration`): a sequence of decimal
/// numbers with a unit each (`ns`, `us`, `µs`, `ms`, `s`, `m`, `h`), or `0`.
/// Negative durations are refused.
#[must_use]
pub fn parse_go_duration(value: &str) -> Option<Duration> {
    if value == "0" {
        return Some(Duration::ZERO);
    }
    let mut rest = value.strip_prefix('+').unwrap_or(value);
    if rest.is_empty() {
        return None;
    }
    let mut total_nanos: u128 = 0;
    while !rest.is_empty() {
        let number_end = rest
            .find(|character: char| !character.is_ascii_digit() && character != '.')
            .unwrap_or(rest.len());
        let (number, after) = rest.split_at(number_end);
        if number.is_empty() || number == "." || number.matches('.').count() > 1 {
            return None;
        }
        let unit_end = after
            .find(|character: char| character.is_ascii_digit() || character == '.')
            .unwrap_or(after.len());
        let (unit, next) = after.split_at(unit_end);
        let scale: u128 = match unit {
            "ns" => 1,
            "us" | "µs" | "μs" => 1_000,
            "ms" => 1_000_000,
            "s" => 1_000_000_000,
            "m" => 60_000_000_000,
            "h" => 3_600_000_000_000,
            _ => return None,
        };
        let (whole, fraction) = number.split_once('.').unwrap_or((number, ""));
        let whole: u128 = if whole.is_empty() {
            0
        } else {
            whole.parse().ok()?
        };
        let mut nanos = whole.checked_mul(scale)?;
        let mut divisor: u128 = 1;
        let mut fraction_value: u128 = 0;
        for digit in fraction.bytes().take(18) {
            fraction_value = fraction_value * 10 + u128::from(digit - b'0');
            divisor *= 10;
        }
        nanos = nanos.checked_add(fraction_value * scale / divisor)?;
        total_nanos = total_nanos.checked_add(nanos)?;
        rest = next;
    }
    let seconds = u64::try_from(total_nanos / 1_000_000_000).ok()?;
    let nanos = u32::try_from(total_nanos % 1_000_000_000).ok()?;
    Some(Duration::new(seconds, nanos))
}

fn format_duration(duration: Duration) -> String {
    format!("{duration:?}")
}

fn invalid(key: &'static str, value: String, expected: &'static str) -> ConfigError {
    ConfigError::InvalidValue {
        key,
        value,
        expected,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::AppConfig;

    fn load(pairs: &[(&str, &str)]) -> Result<AppConfig, ConfigError> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect();
        AppConfig::from_lookup(|key| map.get(key).cloned())
    }

    #[test]
    fn defaults_match_forge_go() {
        let config = load(&[]).expect("defaults are valid").telemetry;
        assert!(config.enabled);
        assert_eq!(config.exporter, TelemetryExporter::None);
        assert_eq!(config.logs_exporter, TelemetryLogsExporter::None);
        assert_eq!(config.logs_level, TelemetryLogLevel::Info);
        assert_eq!(config.otlp_timeout, Duration::from_secs(10));
        assert_eq!(config.metrics_interval, Duration::from_secs(60));
        assert_eq!(config.sample_ratio.get(), 1.0);
        assert!(!config.trust_traceparent);
        assert!(config.metrics_addr.is_none());
    }

    #[test]
    fn every_key_is_read() {
        let config = load(&[
            ("FORGE_TELEMETRY_SERVICE_NAME", "billing"),
            ("FORGE_TELEMETRY_SERVICE_VERSION", "1.2.3"),
            ("FORGE_TELEMETRY_EXPORTER", "otlp"),
            ("FORGE_TELEMETRY_LOGS_EXPORTER", "otlp"),
            ("FORGE_TELEMETRY_LOGS_LEVEL", "warn"),
            ("FORGE_TELEMETRY_OTLP_ENDPOINT", "http://collector:4318"),
            (
                "FORGE_TELEMETRY_OTLP_HEADERS",
                "Authorization=Bearer%20TOKEN,x-tenant=a",
            ),
            ("FORGE_TELEMETRY_OTLP_TIMEOUT", "2.5s"),
            ("FORGE_TELEMETRY_OTLP_COMPRESSION", "gzip"),
            ("FORGE_TELEMETRY_SAMPLE_RATIO", "0.25"),
            ("FORGE_TELEMETRY_TRUST_TRACEPARENT", "true"),
            ("FORGE_TELEMETRY_METRICS_ADDR", "localhost:9464"),
            ("FORGE_TELEMETRY_METRICS_INTERVAL", "1m30s"),
        ])
        .expect("valid")
        .telemetry;
        assert_eq!(config.service_name.as_deref(), Some("billing"));
        assert_eq!(config.service_version.as_deref(), Some("1.2.3"));
        assert_eq!(config.exporter, TelemetryExporter::Otlp);
        assert_eq!(config.logs_level, TelemetryLogLevel::Warn);
        assert_eq!(config.otlp_headers.len(), 2);
        assert_eq!(config.otlp_headers[0].name, "authorization");
        assert_eq!(config.otlp_headers[0].value.expose(), "Bearer TOKEN");
        assert_eq!(config.otlp_timeout, Duration::from_millis(2500));
        assert_eq!(config.otlp_compression, OtlpCompression::Gzip);
        assert_eq!(config.sample_ratio.get(), 0.25);
        assert!(config.trust_traceparent);
        assert_eq!(
            config.metrics_addr,
            Some("127.0.0.1:9464".parse().expect("address"))
        );
        assert_eq!(config.metrics_interval, Duration::from_secs(90));
    }

    #[test]
    fn header_values_are_redacted_and_never_echoed() {
        let config = load(&[(
            "FORGE_TELEMETRY_OTLP_HEADERS",
            "authorization=Bearer%20sk-live-secret",
        )])
        .expect("valid")
        .telemetry;
        let debug = format!("{config:?}");
        assert!(!debug.contains("sk-live-secret"), "{debug}");
        let json = serde_json::to_string(&config).expect("serializes");
        assert!(!json.contains("sk-live-secret"), "{json}");

        for bad in [
            "authorization",
            "bad name=sk-live-secret",
            "content-type=sk-live-secret",
            "a=sk-live-secret,A=sk-live-secret",
            "a=%zzsk-live-secret",
            "a=sk-live-secret%0a",
        ] {
            let error = load(&[("FORGE_TELEMETRY_OTLP_HEADERS", bad)]).expect_err(bad);
            assert!(!error.to_string().contains("sk-live-secret"), "{error}");
            assert!(error.to_string().contains("entry"), "{error}");
        }
    }

    #[test]
    fn otlp_needs_a_valid_endpoint_that_is_never_echoed() {
        let missing = load(&[("FORGE_TELEMETRY_EXPORTER", "otlp")]).expect_err("no endpoint");
        assert!(
            missing
                .to_string()
                .contains("FORGE_TELEMETRY_OTLP_ENDPOINT")
        );
        let logs_only =
            load(&[("FORGE_TELEMETRY_LOGS_EXPORTER", "otlp")]).expect_err("no endpoint");
        assert!(
            logs_only
                .to_string()
                .contains("FORGE_TELEMETRY_OTLP_ENDPOINT")
        );
        for bad in [
            "ftp://collector:4318",
            "http://user:pw@collector:4318",
            "http://collector:4318?token=x",
            "http://collector:4318#x",
            "http://collector:0",
            "http://collector:99999",
            "http://",
            "collector:4318",
        ] {
            let error = load(&[
                ("FORGE_TELEMETRY_EXPORTER", "otlp"),
                ("FORGE_TELEMETRY_OTLP_ENDPOINT", bad),
            ])
            .expect_err(bad);
            assert!(!error.to_string().contains("collector"), "{bad}: {error}");
        }
        for good in [
            "http://collector:4318",
            "https://otlp.example.com/prefix",
            "http://[::1]:4318",
            "http://127.0.0.1",
        ] {
            load(&[
                ("FORGE_TELEMETRY_EXPORTER", "otlp"),
                ("FORGE_TELEMETRY_OTLP_ENDPOINT", good),
            ])
            .unwrap_or_else(|error| panic!("{good}: {error}"));
        }
    }

    #[test]
    fn production_requires_https_unless_plaintext_is_allowed() {
        let plain = [
            ("FORGE_ENV", "production"),
            ("FORGE_TELEMETRY_EXPORTER", "otlp"),
            ("FORGE_TELEMETRY_OTLP_ENDPOINT", "http://collector:4318"),
        ];
        assert!(load(&plain).is_err());
        let mut allowed = plain.to_vec();
        allowed.push(("FORGE_TELEMETRY_OTLP_ALLOW_PLAINTEXT", "true"));
        assert!(load(&allowed).is_ok());
        assert!(
            load(&[
                ("FORGE_ENV", "production"),
                ("FORGE_TELEMETRY_EXPORTER", "otlp"),
                ("FORGE_TELEMETRY_OTLP_ENDPOINT", "https://collector:4318"),
            ])
            .is_ok()
        );
    }

    #[test]
    fn log_exporter_is_refused_in_production() {
        assert!(load(&[("FORGE_TELEMETRY_EXPORTER", "log")]).is_ok());
        assert!(
            load(&[
                ("FORGE_ENV", "production"),
                ("FORGE_TELEMETRY_EXPORTER", "log")
            ])
            .is_err()
        );
    }

    #[test]
    fn metrics_listener_rules() {
        assert!(load(&[("FORGE_TELEMETRY_METRICS_ADDR", "127.0.0.1:3000")]).is_err());
        assert!(load(&[("FORGE_TELEMETRY_METRICS_ADDR", "0.0.0.0:3000")]).is_err());
        assert!(load(&[("FORGE_TELEMETRY_METRICS_ADDR", "metrics.internal:9464")]).is_err());
        assert!(load(&[("FORGE_TELEMETRY_METRICS_ADDR", "0.0.0.0:9464")]).is_ok());
        let production = [
            ("FORGE_ENV", "production"),
            ("FORGE_TELEMETRY_METRICS_ADDR", "0.0.0.0:9464"),
        ];
        assert!(load(&production).is_err());
        let mut public = production.to_vec();
        public.push(("FORGE_TELEMETRY_METRICS_PUBLIC", "1"));
        assert!(load(&public).is_ok());
        assert!(
            load(&[
                ("FORGE_ENV", "production"),
                ("FORGE_TELEMETRY_METRICS_ADDR", "127.0.0.1:9464"),
            ])
            .is_ok()
        );
    }

    #[test]
    fn ranges_are_enforced() {
        for (key, value) in [
            ("FORGE_TELEMETRY_SAMPLE_RATIO", "1.5"),
            ("FORGE_TELEMETRY_SAMPLE_RATIO", "NaN"),
            ("FORGE_TELEMETRY_SAMPLE_RATIO", "-0.1"),
            ("FORGE_TELEMETRY_METRICS_INTERVAL", "500ms"),
            ("FORGE_TELEMETRY_METRICS_INTERVAL", "10"),
            ("FORGE_TELEMETRY_ENABLED", "yes"),
            ("FORGE_TELEMETRY_EXPORTER", "jaeger"),
            ("FORGE_TELEMETRY_LOGS_EXPORTER", "log"),
            ("FORGE_TELEMETRY_LOGS_LEVEL", "trace"),
            ("FORGE_TELEMETRY_OTLP_COMPRESSION", "zstd"),
        ] {
            assert!(load(&[(key, value)]).is_err(), "{key}={value}");
        }
        let timeout = [
            ("FORGE_TELEMETRY_EXPORTER", "otlp"),
            ("FORGE_TELEMETRY_OTLP_ENDPOINT", "http://collector:4318"),
            ("FORGE_TELEMETRY_OTLP_TIMEOUT", "2m"),
        ];
        assert!(load(&timeout).is_err());
    }

    #[test]
    fn disabled_telemetry_skips_cross_checks() {
        let config = load(&[
            ("FORGE_TELEMETRY_ENABLED", "false"),
            ("FORGE_TELEMETRY_EXPORTER", "otlp"),
        ])
        .expect("disabled telemetry is not validated further")
        .telemetry;
        assert!(!config.enabled);
    }

    #[test]
    fn go_durations_parse() {
        for (text, expected) in [
            ("0", Duration::ZERO),
            ("10s", Duration::from_secs(10)),
            ("1.5h", Duration::from_secs(5400)),
            ("1m30s", Duration::from_secs(90)),
            ("500ms", Duration::from_millis(500)),
            ("250us", Duration::from_micros(250)),
            ("250µs", Duration::from_micros(250)),
            ("7ns", Duration::from_nanos(7)),
            (".5s", Duration::from_millis(500)),
        ] {
            assert_eq!(parse_go_duration(text), Some(expected), "{text}");
        }
        for text in ["", "10", "s", "-1s", "1.2.3s", "1d", "1 s", "."] {
            assert_eq!(parse_go_duration(text), None, "{text}");
        }
    }
}
