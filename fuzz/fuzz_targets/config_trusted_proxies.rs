//! `FORGE_HTTP_TRUSTED_PROXIES` and the client address it decides
//! (forge_config::parse_trusted_proxies, forge_http::TrustedProxies).
//!
//! Input: NUL-separated fields: the proxy list, the TCP peer, then the
//! `X-Forwarded-For` header lines. Properties: the list parser never panics
//! and a refused entry is named by position, never by text; the resolved client is the
//! peer or a canonical entry of the header; a peer that is not trusted is
//! always the client.
#![no_main]

use std::net::IpAddr;

use forge_config::parse_trusted_proxies;
use forge_http::{HeaderMap, HeaderValue, TrustedProxies, X_FORWARDED_FOR};
use libfuzzer_sys::fuzz_target;

const MARKER: &str = "zq7-entry-marker";

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let mut fields = text.split('\0');
    let list = fields.next().unwrap_or_default();
    let peer = fields
        .next()
        .and_then(|peer| peer.trim().parse::<IpAddr>().ok());

    // A refused entry is reported by position, never by text.
    if !list.contains("zq7")
        && let Err(error) = parse_trusted_proxies(&format!("{list},{MARKER}{list}"))
    {
        let rendered = format!("{error} | {error:?}");
        assert!(!rendered.contains(MARKER), "an error echoed an entry");
    }
    let Ok(proxies) = parse_trusted_proxies(list) else {
        return;
    };
    let Some(peer) = peer else {
        return;
    };
    let trusted = TrustedProxies::new(proxies);

    let mut headers = HeaderMap::new();
    let mut entries: Vec<String> = Vec::new();
    for line in fields {
        if let Ok(value) = HeaderValue::from_str(line) {
            headers.append(X_FORWARDED_FOR, value);
            entries.extend(line.split(',').map(|entry| entry.to_owned()));
        }
    }

    let client = trusted.resolve(peer, &headers);
    assert_eq!(client, trusted.resolve(peer, &headers), "not deterministic");
    let peer = peer.to_canonical();
    // Entries are canonical: the zone of an IPv6 address is dropped.
    let canonical_entry = |client: IpAddr| {
        entries.iter().any(|entry| {
            let entry = entry.trim();
            let entry = entry.split('%').next().unwrap_or(entry);
            entry.parse::<IpAddr>().map(|ip| ip.to_canonical()) == Ok(client)
        })
    };
    assert!(
        client == peer || canonical_entry(client),
        "client {client} is neither the peer nor an entry of the header"
    );
    if !trusted.proxies().iter().any(|proxy| proxy.contains(peer)) {
        assert_eq!(client, peer, "an untrusted peer chose its address");
    }
});
