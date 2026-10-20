# ADR 0032: Trusted proxies and the client address

- Status: Accepted
- Date: 2026-10-20
- Refines: [ADR 0026](0026-authentication-pages-and-setup.md) (sign-in and reset throttles), [ADR 0027](0027-rate-limiting.md) (client identity), [ADR 0031](0031-live-updates-over-server-sent-events.md) (anonymous stream cap)
- Parity: Forge-Go ADR 0037 ("Trusted proxies and the client address"); closes [forge-rust#12](https://github.com/rocketinsidecorp/forge-rust/issues/12)

## Context

Sign-in and password-reset throttles (ADR 0026), rate limits (ADR 0027) and
the anonymous per-client stream cap (ADR 0031) all count "the client's
address". Until now that was always the TCP peer, because forwarding headers
are client-controlled unless a proxy you trust overwrites them.

Production applications usually run behind a reverse proxy (nginx, Traefik,
a cloud load balancer, Forge-Go's forge-proxy), and then the peer of every
request is the proxy. Every client shared one source:

- 300 sign-in attempts from anyone in 15 minutes refused every sign-in;
- the reset-request limits per source became limits for everyone;
- the 5-attempt limit per login *and source*, which keeps one attacker from
  locking a login out for everybody, became a limit per login from anywhere;
- `by_remote_addr` put everyone in one bucket, and every anonymous viewer of
  live updates shared the proxy's 16 stream slots.

## Decision

The same behavior, names and validation as Forge-Go ADR 0037.

### Configuration: `FORGE_HTTP_TRUSTED_PROXIES`, fail closed

`FORGE_HTTP_TRUSTED_PROXIES` (`ServerConfig::trusted_proxies`) is a
comma-separated list of CIDR prefixes or bare addresses (a bare address is a
`/32` or `/128`), such as `172.18.0.0/16, fd00:1::/64, 10.0.0.7`. **The
default is empty: trust nobody, use the peer.** The process refuses to start
on:

- an empty entry (`10.0.0.0/8,`) or a malformed one (a host name, a port,
  brackets, a zone such as `fe80::1%eth0`, a prefix length with a sign or a
  leading zero, or longer than the address);
- an overly broad prefix: an IPv4 prefix shorter than `/8` or an IPv6 prefix
  shorter than `/16`, `/0` included. They would trust most of the internet
  and let almost any client choose its address (two `/1` prefixes cover all
  of IPv4);
- an IPv4-mapped IPv6 prefix or address (`::ffff:10.0.0.0/104`): peers are
  compared in their IPv4 form, so it would silently never match; write the
  IPv4 form.

Errors name the variable and the entry's position, never its text:
`invalid value for FORGE_HTTP_TRUSTED_PROXIES: entry 2 is too broad: IPv4
prefixes shorter than /8 trust too many addresses; list the proxy networks
instead`.

The rules have one definition, `forge_config::TrustedProxy::new`. A
`TrustedProxy` value exists only after it accepted it (Forge-Go checks a
plain `netip.Prefix` with `config.CheckTrustedProxy` in three places; Rust
enforces the same rules through the type): parsing the variable, parsing one
entry (`FromStr`), deserializing a `ServerConfig` and building the resolver
all go through it, so neither a hand-built configuration nor a deserialized
one can carry an unsafe prefix. Prefixes are stored masked.

### One resolver, outermost, one header

`forge_http::TrustedProxies` resolves the client address once per request.
`Server::new` builds it from `ServerConfig::trusted_proxies` and applies it
to every request before the first `Middleware` layer runs;
`Request::client_ip()` returns the result. The value lives in a private field
that only `TrustedProxies::apply` sets, so a layer cannot forge it by
inserting an extension. A layer can still call the public
`TrustedProxies::apply` with another validated set and re-resolve, but the
result is still the peer or a parsed entry of the header. When no resolver ran (a `Router` dispatched without a
server, as tests do), `client_ip()` is the peer, the same as trusting nobody;
`TrustedProxies` is also a `Middleware` for such harnesses.
`Request::remote_addr()` keeps meaning the TCP peer.

Resolution, given the peer `P` and the trusted set `T`:

1. If `P` is not in `T`, the client is `P`. Every forwarding header is
   ignored.
2. Otherwise read `X-Forwarded-For` (all header lines, in order, as one list)
   **from the right**. Skip entries in `T`. The first entry not in `T` is the
   client: everything to its left was written by that client or by proxies
   nobody vouches for, and is never read.
3. If an entry reached on that walk is empty or not a bare IP address (a
   port, brackets, `for=` syntax, bytes that are not UTF-8, garbage), the
   client is `P`: a malformed chain never resolves to a value the client may
   have chosen.
4. If every entry is trusted, the client is the leftmost entry; with no
   header it is `P`. More than 32 trusted entries (`MAX_FORWARDED_HOPS`) are
   not a real deployment; the client is `P`.

Addresses are canonical: zone dropped, IPv4-mapped IPv6 as IPv4. The resolver
returns the full address; counters bucket IPv6 by `/64` as before, so a
client rotating through its `/64` still gets no fresh counter.

`Forwarded` (RFC 7239) and `X-Real-IP` are never read. One header keeps the
rule simple to audit; nginx (`$proxy_add_x_forwarded_for`), Traefik, Caddy,
forge-proxy and the common cloud load balancers all set `X-Forwarded-For`.
`X-Forwarded-Proto` is still not interpreted; that is a separate decision.

### Consumers

| Consumer | Before | Now |
| --- | --- | --- |
| Generated sign-in and password-reset throttles (`ThrottleKey` source) | `request.remote_addr().ip()` | `request.client_ip()` |
| Rate limiting | `by_remote_addr` only | `forge_ratelimit::by_client_ip`, the documented per-client key; `by_remote_addr` keeps meaning the peer |
| Live-update anonymous stream cap | `peer_address` | `forge_broadcast::client_ip_address` by default (`EventStreams::client_address` still replaces it; `peer_address` still means the peer) |

All three bucket IPv4 whole and IPv6 by `/64`.

### Deployment

Forge-Rust has no `forge deploy` or forge-proxy yet (Forge-Go configures the
variable automatically from the `forge` Docker network's subnets). Operators
set `FORGE_HTTP_TRUSTED_PROXIES` to the networks of their own proxies; the
[deployment guide](../deployment.md#client-addresses-behind-a-proxy) shows
nginx, Traefik and Docker Compose setups. When `forge deploy` is ported it
must set the variable the way Forge-Go ADR 0037 does.

## Consequences

- Behind a configured proxy each client has its own sign-in, reset,
  rate-limit and stream counters.
- Without configuration nothing changes: the peer is the client, as before.
- **Residual:** anything inside a listed network (other containers on the
  same Docker network, accessories, a compromised dependency in one of them,
  processes on the host through the network's gateway address) can send
  requests with a chosen `X-Forwarded-For` straight to the application and
  pick its throttle key. That gives it what it already has by being inside
  the network, plus the ability to spend other clients' per-source budgets;
  list only proxy networks, and narrow the list where untrusted services
  share hosts.
- **Residual:** an application reachable both through the proxy and directly
  from a listed network trusts that network's headers; publish no
  application port and list only proxy networks.
- **Residual:** a too-broad list re-opens spoofing. Configuration is the
  operator's; the minimum prefix lengths (`/8`, `/16`) only stop the obvious
  mistakes, and a `/8` still trusts 16 million addresses.
- **Residual:** a proxy that forwards the client's `X-Forwarded-For`
  unchanged (neither appending nor overwriting) lets clients choose their
  address; the proxy must append the connecting address or overwrite the
  header.
- The audit records of Forge-Rust carry no source address digest yet, so no
  audit consumer changed; when one is added it must use `client_ip()`.
- A successful sign-in still does not clear the source counter: one client's
  counter now covers that client only.

## Verification

- `forge-config`: default empty, masked parsing, minimum prefix lengths,
  every invalid entry rejected without echo (including zones, ports,
  brackets, `/+8`, `/08`), deserialization through the same rules.
- `forge-http`: the Forge-Go resolution table, non-UTF-8 entries, the other
  headers never read, the 32-hop limit, a deterministic generated property
  test (20 000 random chains: the result is always the peer or an entry of
  the header, in canonical form, and never anything but the peer for an
  untrusted peer), and a real server resolving before its first layer while
  ignoring the header from untrusted peers.
- `forge-ratelimit`, `forge-broadcast`: clients behind one proxy get separate
  buckets and slots, forged prefixes do not escape, direct clients cannot
  choose a bucket, IPv6 `/64` rotation is one client.
- `forge` (`tests/client_ip.rs`): configuration → server → throttle over TCP,
  including two clients behind one proxy and spoofed headers without trust.
- Generated application: `clients_behind_one_trusted_proxy_have_their_own_sign_in_counters`;
  the e2e script refuses an unsafe value without echoing it.
