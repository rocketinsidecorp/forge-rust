//! What a service is routed with: the deploy request, its defaults and
//! validation, and the JSON the control socket and state file carry.
//!
//! The JSON field names, the omission of zero values and the duration text
//! match Forge-Go's forge-proxy, so the two implementations can read each
//! other's state volume and control messages.

use std::net::IpAddr;

use serde::{Deserialize, Serialize};

use crate::duration::GoDuration;

/// Default health check path.
pub const DEFAULT_HEALTH_PATH: &str = "/health/ready";
/// Default time between health checks.
pub const DEFAULT_HEALTH_INTERVAL: GoDuration = GoDuration::from_secs(1);
/// Default timeout of one health check.
pub const DEFAULT_HEALTH_TIMEOUT: GoDuration = GoDuration::from_secs(2);
/// Default number of consecutive passing checks.
pub const DEFAULT_HEALTHY_COUNT: i64 = 3;
/// Default time a new target has to become healthy.
pub const DEFAULT_DEPLOY_TIMEOUT: GoDuration = GoDuration::from_secs(60);
/// Default time in-flight requests have on a previous target.
pub const DEFAULT_DRAIN_TIMEOUT: GoDuration = GoDuration::from_secs(30);
/// Default time a target has to send response headers.
pub const DEFAULT_RESPONSE_TIMEOUT: GoDuration = GoDuration::from_secs(30);
/// Default largest request body.
pub const DEFAULT_MAX_BODY_BYTES: i64 = 64 << 20;
/// Default largest request body read completely before forwarding.
pub const DEFAULT_BUFFER_BYTES: i64 = 1 << 20;

fn is_zero_duration(value: &GoDuration) -> bool {
    value.is_zero()
}

fn is_zero_i64(value: &i64) -> bool {
    *value == 0
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// Registers or updates a service and switches it to `target`.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DeployRequest {
    /// The routing entry; one service per application.
    #[serde(default)]
    pub service: String,
    /// `host:port` of the new container on the shared network.
    #[serde(default)]
    pub target: String,
    /// Request `Host` names routed to this service. A service without hosts
    /// receives requests whose host matches no other service.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hosts: Vec<String>,
    /// Serve the hosts over HTTPS and redirect plain HTTP to it.
    #[serde(default, skip_serializing_if = "is_false")]
    pub tls: bool,
    /// Health check path.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub health_path: String,
    /// Time between health checks.
    #[serde(default, skip_serializing_if = "is_zero_duration")]
    pub health_interval: GoDuration,
    /// Timeout of one health check.
    #[serde(default, skip_serializing_if = "is_zero_duration")]
    pub health_timeout: GoDuration,
    /// Consecutive passing checks required.
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub healthy_count: i64,
    /// Time allowed to become healthy.
    #[serde(default, skip_serializing_if = "is_zero_duration")]
    pub deploy_timeout: GoDuration,
    /// Time allowed for in-flight requests on the previous target.
    #[serde(default, skip_serializing_if = "is_zero_duration")]
    pub drain_timeout: GoDuration,
    /// Time allowed for the target to send response headers.
    #[serde(default, skip_serializing_if = "is_zero_duration")]
    pub response_timeout: GoDuration,
    /// Larger request bodies are refused with 413.
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub max_body_bytes: i64,
    /// The largest request body read completely before the request is
    /// forwarded, so slow clients do not hold application connections.
    /// Larger bodies (up to `max_body_bytes`) are streamed.
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub buffer_bytes: i64,
}

/// Reports a completed switch.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct DeployResult {
    /// The service.
    pub service: String,
    /// The new target.
    pub target: String,
    /// The target that served before, if any.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub previous: String,
    /// False when requests were still in flight on the previous target when
    /// the drain timeout expired; they continue until the old container is
    /// stopped.
    pub drained: bool,
    /// Requests still running on the previous target.
    #[serde(rename = "in_flight_remaining")]
    pub in_flight: i64,
    /// Health checks made before the switch.
    pub health_checks: i64,
    /// Why the routing state could not be saved, when it could not.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub state_save_failed: String,
}

/// One service for `forge-proxy status`.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ServiceStatus {
    /// The service.
    pub service: String,
    /// Routed host names.
    pub hosts: Vec<String>,
    /// HTTPS with redirects.
    pub tls: bool,
    /// The current target.
    pub target: String,
    /// The maintenance page is served.
    pub maintenance: bool,
    /// Requests running on the target.
    pub in_flight: i64,
    /// The health check path.
    pub health_path: String,
}

/// Whether `name` is an acceptable service name.
#[must_use]
pub fn valid_service_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 63
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes[1..]
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_' || *b == b'-')
}

/// Whether `label` is a lower-case DNS label.
fn valid_label(label: &str) -> bool {
    let bytes = label.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 63
        && bytes[0] != b'-'
        && bytes[bytes.len() - 1] != b'-'
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
}

/// Whether `host` is a lower-case DNS name acceptable for routing and
/// certificate issuance. IP addresses and wildcards are refused.
#[must_use]
pub fn valid_host(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && host.parse::<IpAddr>().is_err()
        && host.split('.').all(valid_label)
}

/// Splits `host:port` like Go's `net.SplitHostPort` (brackets for IPv6).
pub(crate) fn split_host_port(address: &str) -> Option<(&str, &str)> {
    if let Some(rest) = address.strip_prefix('[') {
        let (host, after) = rest.split_once(']')?;
        let port = after.strip_prefix(':')?;
        if host.contains('[') || host.contains(']') || port.contains(':') {
            return None;
        }
        return Some((host, port));
    }
    let (host, port) = address.rsplit_once(':')?;
    if host.contains(':') {
        return None;
    }
    Some((host, port))
}

fn validate_target(target: &str) -> Result<(), String> {
    let Some((host, port)) = split_host_port(target) else {
        return Err("target must be host:port".to_owned());
    };
    let bytes = host.as_bytes();
    let host_ok = !bytes.is_empty()
        && bytes.len() <= 253
        && bytes[0].is_ascii_alphanumeric()
        && bytes[1..]
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    if !host_ok {
        return Err("target host is invalid".to_owned());
    }
    match port.parse::<u32>() {
        Ok(number) if (1..=65535).contains(&number) && !port.starts_with('+') => Ok(()),
        _ => Err("target port must be 1-65535".to_owned()),
    }
}

impl DeployRequest {
    /// Applies defaults and validates the request.
    ///
    /// # Errors
    ///
    /// The first problem, in the words Forge-Go uses.
    pub fn normalize(&mut self) -> Result<(), String> {
        if !valid_service_name(&self.service) {
            return Err("service name must match ^[a-z0-9][a-z0-9_-]{0,62}$".to_owned());
        }
        validate_target(&self.target)?;
        let mut seen = std::collections::HashSet::new();
        for host in &mut self.hosts {
            let lowered = host.to_ascii_lowercase();
            if !valid_host(&lowered) {
                return Err(format!(
                    "host {} is not a valid DNS name",
                    go_quote(&lowered)
                ));
            }
            if !seen.insert(lowered.clone()) {
                return Err(format!("host {} is listed twice", go_quote(&lowered)));
            }
            *host = lowered;
        }
        if self.tls && self.hosts.is_empty() {
            return Err("tls requires at least one host".to_owned());
        }
        if self.health_path.is_empty() {
            DEFAULT_HEALTH_PATH.clone_into(&mut self.health_path);
        }
        if !self.health_path.starts_with('/')
            || self
                .health_path
                .chars()
                .any(|c| matches!(c, ' ' | '\t' | '\r' | '\n' | '?' | '#'))
        {
            return Err("health path must be an absolute path without query".to_owned());
        }
        let hour = GoDuration::from_secs(3600);
        let minute = GoDuration::from_secs(60);
        for (value, default, max, name) in [
            (
                &mut self.health_interval,
                DEFAULT_HEALTH_INTERVAL,
                minute,
                "health interval",
            ),
            (
                &mut self.health_timeout,
                DEFAULT_HEALTH_TIMEOUT,
                minute,
                "health timeout",
            ),
            (
                &mut self.deploy_timeout,
                DEFAULT_DEPLOY_TIMEOUT,
                hour,
                "deploy timeout",
            ),
            (
                &mut self.drain_timeout,
                DEFAULT_DRAIN_TIMEOUT,
                hour,
                "drain timeout",
            ),
            (
                &mut self.response_timeout,
                DEFAULT_RESPONSE_TIMEOUT,
                hour,
                "response timeout",
            ),
        ] {
            if value.is_zero() {
                *value = default;
            }
            if value.as_nanos() < 0 || *value > max {
                return Err(format!("{name} must be between 0 and {max}"));
            }
        }
        if self.healthy_count == 0 {
            self.healthy_count = DEFAULT_HEALTHY_COUNT;
        }
        if !(1..=100).contains(&self.healthy_count) {
            return Err("healthy count must be between 1 and 100".to_owned());
        }
        if self.max_body_bytes == 0 {
            self.max_body_bytes = DEFAULT_MAX_BODY_BYTES;
        }
        if self.buffer_bytes == 0 {
            self.buffer_bytes = DEFAULT_BUFFER_BYTES;
        }
        if self.max_body_bytes < 0 || self.buffer_bytes < 0 {
            return Err("body limits must be positive".to_owned());
        }
        self.buffer_bytes = self.buffer_bytes.min(self.max_body_bytes);
        Ok(())
    }
}

/// Go's `%q` for ASCII text.
pub(crate) fn go_quote(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_else(|_| format!("\"{text}\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(target: &str) -> DeployRequest {
        DeployRequest {
            service: "app".to_owned(),
            target: target.to_owned(),
            ..DeployRequest::default()
        }
    }

    #[test]
    fn defaults_are_applied() {
        let mut req = request("shop-web-v1-abc:8080");
        req.hosts = vec!["App.Example.com".to_owned()];
        req.normalize().expect("valid");
        assert_eq!(req.hosts, ["app.example.com"]);
        assert_eq!(req.health_path, DEFAULT_HEALTH_PATH);
        assert_eq!(req.health_interval, DEFAULT_HEALTH_INTERVAL);
        assert_eq!(req.healthy_count, 3);
        assert_eq!(req.max_body_bytes, 64 << 20);
        assert_eq!(req.buffer_bytes, 1 << 20);
        let mut small = request("a:80");
        small.max_body_bytes = 100;
        small.normalize().expect("valid");
        assert_eq!(
            small.buffer_bytes, 100,
            "buffer is capped by the body limit"
        );
    }

    #[test]
    fn invalid_requests_are_refused() {
        let cases: Vec<(&str, DeployRequest)> = vec![
            (
                "bad service",
                DeployRequest {
                    service: "Bad Name".into(),
                    target: "a:80".into(),
                    ..Default::default()
                },
            ),
            ("no port", request("a")),
            ("bad port", request("a:0")),
            ("port range", request("a:65536")),
            ("option target", request("-oops:80")),
            (
                "ip host",
                DeployRequest {
                    hosts: vec!["10.0.0.1".into()],
                    ..request("a:80")
                },
            ),
            (
                "wildcard",
                DeployRequest {
                    hosts: vec!["*.example.com".into()],
                    ..request("a:80")
                },
            ),
            (
                "dup host",
                DeployRequest {
                    hosts: vec!["a.example.com".into(), "A.example.com".into()],
                    ..request("a:80")
                },
            ),
            (
                "tls no hosts",
                DeployRequest {
                    tls: true,
                    ..request("a:80")
                },
            ),
            (
                "health path",
                DeployRequest {
                    health_path: "health".into(),
                    ..request("a:80")
                },
            ),
            (
                "health query",
                DeployRequest {
                    health_path: "/health?x".into(),
                    ..request("a:80")
                },
            ),
            (
                "count",
                DeployRequest {
                    healthy_count: 1000,
                    ..request("a:80")
                },
            ),
            (
                "timeout",
                DeployRequest {
                    deploy_timeout: GoDuration::from_secs(-1),
                    ..request("a:80")
                },
            ),
            (
                "long timeout",
                DeployRequest {
                    health_timeout: GoDuration::from_secs(61),
                    ..request("a:80")
                },
            ),
            (
                "negative body",
                DeployRequest {
                    max_body_bytes: -1,
                    ..request("a:80")
                },
            ),
        ];
        for (name, mut req) in cases {
            assert!(req.normalize().is_err(), "{name} accepted");
        }
    }

    #[test]
    fn json_matches_forge_go() {
        let mut req = request("shop-web-v1-abc:8080");
        req.hosts = vec!["shop.example.com".to_owned()];
        req.tls = true;
        req.normalize().expect("valid");
        let json = serde_json::to_string(&req).expect("serializes");
        assert_eq!(
            json,
            r#"{"service":"app","target":"shop-web-v1-abc:8080","hosts":["shop.example.com"],"tls":true,"health_path":"/health/ready","health_interval":"1s","health_timeout":"2s","healthy_count":3,"deploy_timeout":"1m0s","drain_timeout":"30s","response_timeout":"30s","max_body_bytes":67108864,"buffer_bytes":1048576}"#
        );
        let minimal = serde_json::to_string(&request("a:80")).expect("serializes");
        assert_eq!(minimal, r#"{"service":"app","target":"a:80"}"#);
        assert!(serde_json::from_str::<DeployRequest>(r#"{"service":"a","x":1}"#).is_err());
        assert!(
            serde_json::from_str::<DeployRequest>(r#"{"service":"a","drain_timeout":5}"#).is_err()
        );
    }

    #[test]
    fn split_host_port_matches_go() {
        assert_eq!(split_host_port("a:80"), Some(("a", "80")));
        assert_eq!(split_host_port("[::1]:80"), Some(("::1", "80")));
        assert_eq!(split_host_port("a"), None);
        assert_eq!(split_host_port("::1:80"), None);
    }
}
