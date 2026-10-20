//! The client address of a request behind trusted reverse proxies (ADR 0032).

use std::{
    fmt,
    net::{IpAddr, Ipv6Addr},
    sync::Arc,
};

use forge_config::TrustedProxy;
use http::{HeaderMap, HeaderName};

use crate::{BoxResponseFuture, Middleware, Next, Request};

/// The only forwarding header read. `Forwarded` (RFC 7239) and `X-Real-IP`
/// are never read.
pub const X_FORWARDED_FOR: HeaderName = HeaderName::from_static("x-forwarded-for");

/// Most `X-Forwarded-For` entries examined per request. A chain longer than
/// this made only of trusted proxies is not a real deployment; the peer
/// address is used instead.
pub const MAX_FORWARDED_HOPS: usize = 32;

/// The networks whose `X-Forwarded-For` header is believed (ADR 0032).
///
/// The default trusts nobody, so the client address is always the TCP peer.
/// Every [`TrustedProxy`] was validated when it was made (no `/0`, no IPv4
/// prefix shorter than `/8`, no IPv6 prefix shorter than `/16`, no
/// IPv4-mapped prefix), so building the set cannot fail. It is immutable and
/// cheap to clone.
///
/// [`Server`](crate::Server) resolves every request with the set from
/// [`ServerConfig::trusted_proxies`](forge_config::ServerConfig::trusted_proxies)
/// (`FORGE_HTTP_TRUSTED_PROXIES`) before any [`Middleware`] runs, and
/// [`Request::client_ip`] returns the result.
///
/// ```
/// use forge_config::TrustedProxy;
/// use forge_http::{HeaderMap, HeaderValue, TrustedProxies};
///
/// let trusted = TrustedProxies::new(["172.18.0.0/16".parse::<TrustedProxy>()?]);
/// let mut headers = HeaderMap::new();
/// headers.insert("x-forwarded-for", HeaderValue::from_static("1.2.3.4, 203.0.113.9"));
///
/// // The proxy appended the address it saw; the client's own entry is ignored.
/// assert_eq!(trusted.resolve("172.18.0.2".parse()?, &headers), "203.0.113.9".parse::<std::net::IpAddr>()?);
/// // A client that connects directly cannot choose its address.
/// assert_eq!(trusted.resolve("198.51.100.7".parse()?, &headers), "198.51.100.7".parse::<std::net::IpAddr>()?);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Clone, Default, Eq, PartialEq)]
pub struct TrustedProxies {
    proxies: Arc<[TrustedProxy]>,
}

impl fmt::Debug for TrustedProxies {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_list()
            .entries(self.proxies.iter().map(ToString::to_string))
            .finish()
    }
}

impl TrustedProxies {
    /// The set of the given networks (for a single proxy, a `/32` or
    /// `/128`).
    #[must_use]
    pub fn new(proxies: impl IntoIterator<Item = TrustedProxy>) -> Self {
        Self {
            proxies: proxies.into_iter().collect(),
        }
    }

    /// The trusted networks.
    #[must_use]
    pub fn proxies(&self) -> &[TrustedProxy] {
        &self.proxies
    }

    /// Whether nobody is trusted.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.proxies.is_empty()
    }

    fn trusts(&self, address: IpAddr) -> bool {
        self.proxies.iter().any(|proxy| proxy.contains(address))
    }

    /// The address of the client that sent a request from TCP peer `peer`
    /// with `headers`.
    ///
    /// When the peer is not a trusted proxy, the result is the peer and every
    /// forwarding header is ignored. When it is, `X-Forwarded-For` (all
    /// header lines, in order, as one list) is read from the right (the
    /// entry the peer itself appended) towards the left, skipping trusted
    /// proxies, and the first untrusted address is the client: entries to
    /// its left were written by the client or by proxies nobody vouches for,
    /// and are never read. If an entry on that walk is empty or not a bare IP
    /// address (a port, brackets, `for=` syntax), the result is the peer: a
    /// malformed chain never resolves to a value the client may have chosen.
    /// If every entry is trusted, the leftmost one is the client, unless
    /// there are more than [`MAX_FORWARDED_HOPS`], when it is the peer.
    /// `Forwarded` and `X-Real-IP` are never read.
    ///
    /// Addresses are canonical: zone dropped, IPv4-mapped IPv6 addresses as
    /// IPv4. The result is a full address; counters bucket IPv6 by `/64`
    /// themselves.
    #[must_use]
    pub fn resolve(&self, peer: IpAddr, headers: &HeaderMap) -> IpAddr {
        let peer = peer.to_canonical();
        if !self.trusts(peer) {
            return peer;
        }
        let mut client = peer;
        let mut hops = 0;
        for value in headers.get_all(X_FORWARDED_FOR).iter().rev() {
            for entry in value.as_bytes().rsplit(|byte| *byte == b',') {
                let Some(address) = forwarded_address(entry) else {
                    return peer;
                };
                if !self.trusts(address) {
                    return address;
                }
                hops += 1;
                if hops > MAX_FORWARDED_HOPS {
                    return peer;
                }
                client = address;
            }
        }
        client
    }

    /// Resolves `request` (see [`TrustedProxies::resolve`]) and records the
    /// result as its [`Request::client_ip`]. The server does this for every
    /// request before the first [`Middleware`]; call it yourself only when
    /// driving a [`Router`](crate::Router) without a server, as tests do.
    pub fn apply(&self, request: &mut Request) {
        request.client_ip = Some(self.resolve(request.remote_addr.ip(), &request.headers));
    }
}

/// The address an `X-Forwarded-For` entry names, in canonical form, or
/// `None` when the entry is not a bare IP address. Surrounding whitespace is
/// ignored and an IPv6 zone (`fe80::1%eth0`) is dropped.
fn forwarded_address(entry: &[u8]) -> Option<IpAddr> {
    let text = std::str::from_utf8(entry).ok()?.trim();
    let address = match text.split_once('%') {
        // Only IPv6 addresses have zones, and a zone is never empty.
        Some((address, zone)) if !zone.is_empty() => IpAddr::V6(address.parse::<Ipv6Addr>().ok()?),
        Some(_) => return None,
        None => text.parse().ok()?,
    };
    Some(address.to_canonical())
}

/// The resolver as a layer, for a [`Router`](crate::Router) dispatched
/// without a [`Server`](crate::Server). Register it first, so it is the
/// outermost layer. The server already resolves every request itself.
impl Middleware for TrustedProxies {
    fn handle<'a>(&'a self, mut request: Request, next: Next<'a>) -> BoxResponseFuture<'a> {
        self.apply(&mut request);
        next.run(request)
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use bytes::Bytes;
    use http::{HeaderValue, Method, StatusCode};
    use uuid::Uuid;

    use super::*;
    use crate::{Response, Router};

    fn trust(networks: &[&str]) -> TrustedProxies {
        TrustedProxies::new(
            networks
                .iter()
                .map(|network| network.parse::<TrustedProxy>().expect("valid test network")),
        )
    }

    fn headers(xff: &[&str]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for value in xff {
            headers.append(
                X_FORWARDED_FOR,
                HeaderValue::from_str(value).expect("valid header value"),
            );
        }
        headers
    }

    fn ip(text: &str) -> IpAddr {
        text.parse().expect("valid test address")
    }

    fn peer(text: &str) -> IpAddr {
        text.parse::<SocketAddr>().expect("valid test peer").ip()
    }

    fn request(remote: &str, xff: &[&str]) -> Request {
        Request::new_for_test(
            Method::GET,
            "/".parse().expect("uri"),
            headers(xff),
            Bytes::new(),
            remote.parse().expect("valid test peer"),
            Uuid::now_v7(),
        )
    }

    #[test]
    fn resolves_like_forge_go() {
        // The proxy network of a deployment, an IPv6 proxy network and a
        // second, inner load balancer.
        let trusted = trust(&["172.18.0.0/16", "fd00:1::/64", "10.0.0.7/32"]);
        let cases: &[(&str, &str, &[&str], &str)] = &[
            // Negative: the peer is not a trusted proxy, every header is ignored.
            (
                "untrusted peer spoofing",
                "198.51.100.7:4000",
                &["203.0.113.9"],
                "198.51.100.7",
            ),
            (
                "untrusted peer spoofing a trusted address",
                "198.51.100.7:4000",
                &["172.18.0.2"],
                "198.51.100.7",
            ),
            (
                "untrusted peer, no header",
                "198.51.100.7:4000",
                &[],
                "198.51.100.7",
            ),
            // The trusted proxy's entry is the client.
            (
                "trusted peer",
                "172.18.0.2:5000",
                &["203.0.113.9"],
                "203.0.113.9",
            ),
            (
                "trusted peer, spaces",
                "172.18.0.2:5000",
                &["  203.0.113.9  "],
                "203.0.113.9",
            ),
            // An appending proxy: the client's own entries are left of the
            // one the proxy wrote and are never reached.
            (
                "client-supplied prefix ignored",
                "172.18.0.2:5000",
                &["1.2.3.4, 203.0.113.9"],
                "203.0.113.9",
            ),
            (
                "client-supplied trusted-looking prefix",
                "172.18.0.2:5000",
                &["172.18.0.9, 203.0.113.9"],
                "203.0.113.9",
            ),
            // Several trusted hops are skipped.
            (
                "trusted chain",
                "172.18.0.2:5000",
                &["203.0.113.9, 10.0.0.7"],
                "203.0.113.9",
            ),
            (
                "trusted chain over header lines",
                "172.18.0.2:5000",
                &["1.2.3.4, 203.0.113.9", "10.0.0.7"],
                "203.0.113.9",
            ),
            (
                "every hop trusted",
                "172.18.0.2:5000",
                &["10.0.0.7, 172.18.0.3"],
                "10.0.0.7",
            ),
            (
                "trusted peer, no header",
                "172.18.0.2:5000",
                &[],
                "172.18.0.2",
            ),
            // Malformed entries on the walk fall back to the peer, never to
            // a value left of them.
            ("empty header", "172.18.0.2:5000", &[""], "172.18.0.2"),
            ("garbage", "172.18.0.2:5000", &["not-an-ip"], "172.18.0.2"),
            (
                "trailing comma",
                "172.18.0.2:5000",
                &["203.0.113.9,"],
                "172.18.0.2",
            ),
            (
                "empty element between trusted hops",
                "172.18.0.2:5000",
                &["203.0.113.9,,10.0.0.7"],
                "172.18.0.2",
            ),
            (
                "port in entry",
                "172.18.0.2:5000",
                &["203.0.113.9:1234"],
                "172.18.0.2",
            ),
            (
                "bracketed v6",
                "172.18.0.2:5000",
                &["[2001:db8::1]"],
                "172.18.0.2",
            ),
            (
                "garbage after the client is never read",
                "172.18.0.2:5000",
                &["garbage, 203.0.113.9"],
                "203.0.113.9",
            ),
            (
                "Forwarded-style value",
                "172.18.0.2:5000",
                &["for=203.0.113.9"],
                "172.18.0.2",
            ),
            ("empty zone", "172.18.0.2:5000", &["fe80::1%"], "172.18.0.2"),
            (
                "zoned v4",
                "172.18.0.2:5000",
                &["203.0.113.9%eth0"],
                "172.18.0.2",
            ),
            // IPv6.
            (
                "v6 trusted peer, v6 client",
                "[fd00:1::2]:5000",
                &["2001:db8:1:2::abcd"],
                "2001:db8:1:2::abcd",
            ),
            (
                "v6 untrusted peer",
                "[2001:db8::5]:5000",
                &["203.0.113.9"],
                "2001:db8::5",
            ),
            (
                "v6 zone stripped",
                "[fd00:1::2]:5000",
                &["fe80::1%eth0"],
                "fe80::1",
            ),
            (
                "v4-mapped peer is its v4 form",
                "[::ffff:172.18.0.2]:5000",
                &["203.0.113.9"],
                "203.0.113.9",
            ),
            (
                "v4-mapped client is its v4 form",
                "172.18.0.2:5000",
                &["::ffff:203.0.113.9"],
                "203.0.113.9",
            ),
            (
                "v4-mapped untrusted peer",
                "[::ffff:198.51.100.7]:5000",
                &[],
                "198.51.100.7",
            ),
        ];
        for (name, remote, xff, want) in cases {
            assert_eq!(
                trusted.resolve(peer(remote), &headers(xff)),
                ip(want),
                "{name}: {remote} {xff:?}"
            );
        }
    }

    #[test]
    fn non_utf8_entries_fall_back_to_the_peer_only_when_reached() {
        let trusted = trust(&["172.18.0.0/16"]);
        let mut map = HeaderMap::new();
        map.append(
            X_FORWARDED_FOR,
            HeaderValue::from_bytes(b"203.0.113.\xff").expect("obs-text"),
        );
        assert_eq!(trusted.resolve(ip("172.18.0.2"), &map), ip("172.18.0.2"));
        let mut map = HeaderMap::new();
        map.append(
            X_FORWARDED_FOR,
            HeaderValue::from_bytes(b"\xff, 203.0.113.9").expect("obs-text"),
        );
        assert_eq!(trusted.resolve(ip("172.18.0.2"), &map), ip("203.0.113.9"));
    }

    #[test]
    fn trusting_nobody_uses_the_peer() {
        for trusted in [TrustedProxies::default(), TrustedProxies::new([])] {
            let mut map = headers(&["203.0.113.9"]);
            map.insert("x-real-ip", HeaderValue::from_static("203.0.113.10"));
            map.insert("forwarded", HeaderValue::from_static("for=203.0.113.11"));
            assert_eq!(trusted.resolve(ip("172.18.0.2"), &map), ip("172.18.0.2"));
            assert!(trusted.is_empty());
        }
    }

    #[test]
    fn other_forwarding_headers_are_never_read() {
        let trusted = trust(&["172.18.0.0/16"]);
        let mut map = HeaderMap::new();
        map.insert("x-real-ip", HeaderValue::from_static("203.0.113.10"));
        map.insert("forwarded", HeaderValue::from_static("for=203.0.113.11"));
        assert_eq!(trusted.resolve(ip("172.18.0.2"), &map), ip("172.18.0.2"));
    }

    #[test]
    fn overlong_trusted_chains_fall_back_to_the_peer() {
        let trusted = trust(&["10.0.0.0/8"]);
        let chain = |hops: usize| vec!["10.0.0.1"; hops].join(",");
        // Exactly MAX_FORWARDED_HOPS trusted hops still resolve (to the leftmost).
        assert_eq!(
            trusted.resolve(ip("10.9.9.9"), &headers(&[&chain(MAX_FORWARDED_HOPS)])),
            ip("10.0.0.1")
        );
        // One more falls back to the peer, also when spread over header lines.
        assert_eq!(
            trusted.resolve(ip("10.9.9.9"), &headers(&[&chain(MAX_FORWARDED_HOPS + 1)])),
            ip("10.9.9.9")
        );
        let lines: Vec<String> = (0..=MAX_FORWARDED_HOPS)
            .map(|_| "10.0.0.1".to_owned())
            .collect();
        let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
        assert_eq!(
            trusted.resolve(ip("10.9.9.9"), &headers(&lines)),
            ip("10.9.9.9")
        );
        // An untrusted client left of an overlong chain is never reached.
        let spoofed = format!("203.0.113.9,{}", chain(MAX_FORWARDED_HOPS + 1));
        assert_eq!(
            trusted.resolve(ip("10.9.9.9"), &headers(&[&spoofed])),
            ip("10.9.9.9")
        );
    }

    #[test]
    fn client_ip_is_the_peer_without_the_resolver_and_the_resolved_address_with_it() {
        // Without the resolver, the client is the peer: trusting nobody.
        let unresolved = request("[::ffff:198.51.100.7]:4000", &["203.0.113.9"]);
        assert_eq!(unresolved.client_ip(), ip("198.51.100.7"));

        let trusted = trust(&["172.18.0.0/16"]);
        let mut resolved = request("172.18.0.2:5000", &["203.0.113.9"]);
        trusted.apply(&mut resolved);
        assert_eq!(resolved.client_ip(), ip("203.0.113.9"));
        assert_eq!(
            resolved.remote_addr().ip(),
            ip("172.18.0.2"),
            "the peer is kept"
        );
    }

    #[tokio::test]
    async fn the_layer_resolves_before_later_layers_and_handlers() {
        let mut router = Router::new();
        router.layer(trust(&["172.18.0.0/16"]));
        router
            .route(Method::GET, "/", |request: Request| async move {
                Response::new(StatusCode::OK, request.client_ip().to_string())
            })
            .expect("route");
        let behind = router
            .dispatch(request("172.18.0.2:5000", &["203.0.113.9"]))
            .await;
        assert_eq!(behind.body().as_ref(), b"203.0.113.9");
        let direct = router
            .dispatch(request("198.51.100.7:4000", &["203.0.113.9"]))
            .await;
        assert_eq!(direct.body().as_ref(), b"198.51.100.7");
    }

    /// Deterministic generator (xorshift) for the property test below; the
    /// workspace has no property-testing dependency.
    struct Xorshift(u64);

    impl Xorshift {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, bound: usize) -> usize {
            usize::try_from(self.next() % bound as u64).unwrap_or(0)
        }
    }

    fn random_entry(random: &mut Xorshift) -> String {
        const PIECES: &[&str] = &[
            "172.18.0.3",
            "172.18.255.254",
            "10.0.0.7",
            "203.0.113.9",
            "198.51.100.7",
            "::ffff:203.0.113.9",
            "::ffff:172.18.0.4",
            "2001:db8:1:2::abcd",
            "fe80::1%eth0",
            "fd00:1::9",
            "",
            " ",
            "garbage",
            "203.0.113.9:80",
            "[2001:db8::1]",
            "for=203.0.113.9",
            "1.2.3",
            "%",
            "::",
            "0.0.0.0",
        ];
        let mut entry = PIECES[random.below(PIECES.len())].to_owned();
        if random.below(8) == 0 {
            entry = format!("  {entry}\t");
        }
        if random.below(16) == 0 && !entry.is_empty() {
            // Replace one byte with a character of address syntax.
            let mut bytes = entry.into_bytes();
            let at = random.below(bytes.len());
            bytes[at] = b" ,.:%[]0123456789abcdef"[random.below(23)];
            entry = String::from_utf8(bytes).unwrap_or_default();
        }
        entry
    }

    #[test]
    fn the_result_is_always_the_peer_or_an_entry_of_the_header() {
        let trusted = trust(&["172.18.0.0/16", "10.0.0.7"]);
        let peers = [
            ip("172.18.0.2"),
            ip("198.51.100.7"),
            ip("::ffff:172.18.0.2"),
        ];
        let mut random = Xorshift(0x9E37_79B9_7F4A_7C15);
        for _ in 0..20_000 {
            let lines: Vec<String> = (0..random.below(4))
                .map(|_| {
                    (0..=random.below(5))
                        .map(|_| random_entry(&mut random))
                        .collect::<Vec<_>>()
                        .join(",")
                })
                .collect();
            let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
            let peer = peers[random.below(peers.len())];
            let got = trusted.resolve(peer, &headers(&lines));
            if got == peer.to_canonical() {
                continue;
            }
            assert!(
                trusted.trusts(peer),
                "an untrusted peer resolved to {got} from {lines:?}"
            );
            let in_header = lines
                .iter()
                .flat_map(|line| line.split(','))
                .filter_map(|entry| forwarded_address(entry.as_bytes()))
                .any(|address| address == got);
            assert!(
                in_header,
                "{got} is neither the peer nor an entry of {lines:?}"
            );
            assert_eq!(got, got.to_canonical(), "canonical form");
        }
    }
}
