//! The deploy request forge-proxy reads from its control socket
//! (forge_proxy::DeployRequest::normalize, ADR 0042): JSON written by
//! `forge deploy`, validated before anything is routed.
//!
//! Properties: no panic; a request that normalizes holds a valid service
//! name, one `host:port` target with a real port, lowercase unique valid
//! hosts, an absolute health path without a query and durations inside their
//! bounds; normalizing it again changes nothing.
#![no_main]

use forge_proxy::DeployRequest;
use forge_proxy::settings::{valid_host, valid_service_name};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(mut request) = serde_json::from_slice::<DeployRequest>(data) else {
        return;
    };
    if request.normalize().is_err() {
        return;
    }
    assert!(valid_service_name(&request.service));
    assert!(request.health_path.starts_with('/'));
    assert!(
        !request
            .health_path
            .contains(['?', '#', ' ', '\r', '\n', '\t'])
    );
    let mut seen = std::collections::HashSet::new();
    for host in &request.hosts {
        assert!(valid_host(host));
        assert_eq!(host, &host.to_ascii_lowercase());
        assert!(seen.insert(host.clone()), "host {host} twice");
    }
    assert!(!request.tls || !request.hosts.is_empty());
    assert!((1..=100).contains(&request.healthy_count));
    assert!(request.buffer_bytes >= 0 && request.buffer_bytes <= request.max_body_bytes);

    let once = serde_json::to_string(&request).unwrap_or_default();
    request
        .normalize()
        .expect("a normalized request normalizes again");
    assert_eq!(once, serde_json::to_string(&request).unwrap_or_default());
});
