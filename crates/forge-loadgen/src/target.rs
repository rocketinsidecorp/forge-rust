//! The application under test: sign in once, create the rows the scenarios
//! read, and drive a scenario in a closed loop of workers.
//!
//! The scenarios are those of Forge-Go's load generator
//! (docs/benchmarks.md) with the requests of a generated Forge-Rust
//! application: the CSRF field is `csrf_token`, the scaffold lives under
//! `/{collection}` for the tenant the session signed in to, and sessions are
//! the cookies `__Host-forge_session`, `__Host-forge_csrf` and
//! `__Host-forge_tenant` (set by hand, because they are `Secure` and the
//! application is on plain HTTP).

use std::collections::BTreeMap;
use std::net::{SocketAddr, ToSocketAddrs as _};
use std::thread;
use std::time::{Duration, Instant};

use crate::Config;
use crate::http::{Connection, Request};
use crate::report::Sample;

pub const HEALTH: &str = "health";
pub const WELCOME: &str = "welcome";
pub const INDEX_HTML: &str = "index_html";
pub const SHOW_JSON: &str = "show_json";
pub const CREATE_FORM: &str = "create_form";

/// Every scenario name Forge-Go's load generator knows.
pub const ALL: [&str; 5] = [HEALTH, WELCOME, INDEX_HTML, SHOW_JSON, CREATE_FORM];

/// The scenarios Forge-Rust can run. `show_json` is missing: a generated
/// application has no JSON scaffold and no bearer-token login yet
/// (docs/cli.md, "Refusals"), so there is no JSON row to show.
pub const AVAILABLE: [&str; 4] = [HEALTH, WELCOME, INDEX_HTML, CREATE_FORM];

const SESSION_COOKIE: &str = "__Host-forge_session";
const TENANT_COOKIE: &str = "__Host-forge_tenant";
const CSRF_COOKIE: &str = "__Host-forge_csrf";
const CSRF_FIELD: &str = "csrf_token";

/// The signed-in state and one request per scenario.
pub struct Target {
    pool: Vec<Connection>,
    scenarios: BTreeMap<&'static str, Request>,
}

/// Percent-encodes a form value.
#[must_use]
pub fn form_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn form(fields: &[(&str, &str)]) -> String {
    fields
        .iter()
        .map(|(name, value)| format!("{name}={}", form_encode(value)))
        .collect::<Vec<_>>()
        .join("&")
}

/// The value of the hidden `csrf_token` field of a page.
fn token_field(page: &str) -> Option<&str> {
    let marker = format!("name=\"{CSRF_FIELD}\" value=\"");
    let start = page.find(&marker)? + marker.len();
    page[start..]
        .split('"')
        .next()
        .filter(|token| !token.is_empty())
}

fn resolve(base: &str) -> Result<(SocketAddr, String), String> {
    let authority = base.trim_end_matches('/').strip_prefix("http://").ok_or(
        "the base URL must be http://HOST:PORT (the benchmark runs on loopback, without TLS)",
    )?;
    let addr = authority
        .to_socket_addrs()
        .map_err(|error| format!("cannot resolve {authority}: {error}"))?
        .next()
        .ok_or_else(|| format!("{authority} resolves to no address"))?;
    Ok((addr, authority.to_owned()))
}

impl Target {
    /// Waits for readiness, signs in, creates `cfg.rows` rows and builds the
    /// scenarios' requests.
    ///
    /// # Errors
    ///
    /// When the application is not ready or answers the setup requests
    /// unexpectedly.
    pub fn new(cfg: &Config) -> Result<Self, String> {
        let (addr, host) = resolve(&cfg.base)?;
        let mut connection = Connection::new(addr);
        let step = |connection: &mut Connection, request: &Request| {
            let reply = connection
                .send(request, true)
                .map_err(|error| format!("{}: {error}", request.label))?;
            if reply.status == request.want {
                Ok(reply)
            } else {
                Err(format!(
                    "{} answered {}, want {}",
                    request.label, reply.status, request.want
                ))
            }
        };
        wait_ready(&mut connection, &host, &cfg.base)?;

        let page = step(
            &mut connection,
            &Request::new(
                &host,
                "GET",
                "/session/new",
                &[("Accept", "text/html".to_owned())],
                "",
                200,
            ),
        )
        .map_err(|error| format!("open the sign-in page: {error}"))?;
        let anonymous = page.set_cookie(CSRF_COOKIE);
        let text = page.text();
        let (Some(cookie), Some(token)) = (anonymous, token_field(&text)) else {
            return Err("the sign-in page carries no CSRF cookie and token".to_owned());
        };
        let signed_in = step(
            &mut connection,
            &Request::new(
                &host,
                "POST",
                "/session",
                &[
                    (
                        "Content-Type",
                        "application/x-www-form-urlencoded".to_owned(),
                    ),
                    ("Cookie", format!("{CSRF_COOKIE}={cookie}")),
                ],
                &form(&[
                    (CSRF_FIELD, token),
                    ("tenant", &cfg.tenant),
                    ("email", &cfg.email),
                    ("password", &cfg.password),
                ]),
                303,
            ),
        )
        .map_err(|error| format!("sign in: {error}"))?;
        let (Some(session), Some(csrf)) = (
            signed_in.set_cookie(SESSION_COOKIE),
            signed_in.set_cookie(CSRF_COOKIE),
        ) else {
            return Err("sign-in set no session and CSRF cookies".to_owned());
        };
        // The tenant the session signed in to, which tenant-scoped pages use.
        let tenant = signed_in
            .set_cookie(TENANT_COOKIE)
            .unwrap_or_else(|| cfg.tenant_id.clone());
        let cookies =
            format!("{SESSION_COOKIE}={session}; {CSRF_COOKIE}={csrf}; {TENANT_COOKIE}={tenant}");

        let collection = format!("/{}", cfg.collection);
        let create = Request::new(
            &host,
            "POST",
            &collection,
            &[
                (
                    "Content-Type",
                    "application/x-www-form-urlencoded".to_owned(),
                ),
                ("Cookie", cookies.clone()),
            ],
            &form(&[
                (CSRF_FIELD, &csrf),
                ("title", "Benchmark post"),
                ("body", "A short body written by the load generator."),
            ]),
            303,
        );
        for _ in 0..cfg.rows {
            step(&mut connection, &create).map_err(|error| format!("create a row: {error}"))?;
        }
        let index = Request::new(
            &host,
            "GET",
            &collection,
            &[("Cookie", cookies), ("Accept", "text/html".to_owned())],
            "",
            200,
        );
        // The page size is the generated application's, not a parameter; say
        // so when it is not the number of rows the report claims.
        let listed = step(&mut connection, &index)
            .map_err(|error| format!("read the index: {error}"))?
            .text()
            .matches("/edit\"")
            .count();
        if listed != cfg.rows {
            return Err(format!(
                "the index lists {listed} rows, want {}: set the scaffold's PAGE_SIZE to {} (scripts/bench.sh does)",
                cfg.rows, cfg.rows
            ));
        }

        let mut scenarios = BTreeMap::new();
        scenarios.insert(
            HEALTH,
            Request::new(&host, "GET", "/health/ready", &[], "", 200),
        );
        scenarios.insert(
            WELCOME,
            Request::new(
                &host,
                "GET",
                "/",
                &[("Accept", "text/html".to_owned())],
                "",
                200,
            ),
        );
        scenarios.insert(INDEX_HTML, index);
        scenarios.insert(CREATE_FORM, create);
        Ok(Self {
            pool: (0..cfg.concurrency)
                .map(|_| Connection::new(addr))
                .collect(),
            scenarios,
        })
    }

    /// Runs `scenario` in a closed loop of `workers` for `window`: each worker
    /// sends its next request when the previous one is answered, on its own
    /// connection (kept across windows).
    ///
    /// # Panics
    ///
    /// When `scenario` is not one of [`AVAILABLE`].
    pub fn drive(&mut self, scenario: &str, workers: usize, window: Duration) -> Sample {
        let Some(request) = self.scenarios.get(scenario) else {
            panic!("unknown scenario {scenario}");
        };
        let started = Instant::now();
        let deadline = started + window;
        let parts: Vec<(Vec<Duration>, usize)> = thread::scope(|scope| {
            let handles: Vec<_> = self
                .pool
                .iter_mut()
                .take(workers)
                .map(|connection| scope.spawn(move || worker(connection, request, deadline)))
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap_or_default())
                .collect()
        });
        let mut sample = Sample {
            elapsed: started.elapsed(),
            ..Sample::default()
        };
        for (latencies, errors) in parts {
            sample.latencies.extend(latencies);
            sample.errors += errors;
        }
        sample.ok = sample.latencies.len();
        sample.latencies.sort_unstable();
        sample
    }
}

fn worker(
    connection: &mut Connection,
    request: &Request,
    deadline: Instant,
) -> (Vec<Duration>, usize) {
    let mut latencies = Vec::with_capacity(1 << 16);
    let mut errors = 0;
    while Instant::now() < deadline {
        let began = Instant::now();
        match connection.send(request, false) {
            Ok(reply) if reply.status == request.want => latencies.push(began.elapsed()),
            _ => errors += 1,
        }
    }
    (latencies, errors)
}

/// Polls readiness so a server that is still starting is not measured.
fn wait_ready(connection: &mut Connection, host: &str, base: &str) -> Result<(), String> {
    let request = Request::new(host, "GET", "/health/ready", &[], "", 200);
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let failure = match connection.send(&request, false) {
            Ok(reply) if reply.status == 200 => return Ok(()),
            Ok(reply) => format!("answered {}", reply.status),
            Err(error) => error.to_string(),
        };
        if Instant::now() >= deadline {
            return Err(format!("the application is not ready at {base}: {failure}"));
        }
        thread::sleep(Duration::from_millis(250));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_values_are_percent_encoded() {
        assert_eq!(form_encode("a b&c=d/é"), "a%20b%26c%3Dd%2F%C3%A9");
        assert_eq!(form_encode("Az09-._~"), "Az09-._~");
        assert_eq!(form(&[("a", "1 2"), ("b", "x")]), "a=1%202&b=x");
    }

    #[test]
    fn finds_the_token_of_the_hidden_field() {
        let page = r#"<form><input type="hidden" name="csrf_token" value="abc123"><input name="tenant"></form>"#;
        assert_eq!(token_field(page), Some("abc123"));
        assert_eq!(token_field("<form></form>"), None);
        assert_eq!(token_field(r#"name="csrf_token" value="""#), None);
    }

    #[test]
    fn only_loopback_http_urls_resolve() {
        assert!(resolve("https://127.0.0.1:1").is_err());
        let resolved = resolve("http://127.0.0.1:18080/");
        assert_eq!(
            resolved.map(|(addr, host)| (addr.port(), host)),
            Ok((18080, "127.0.0.1:18080".to_owned()))
        );
    }

    #[test]
    fn show_json_is_known_but_not_available() {
        assert!(ALL.contains(&SHOW_JSON));
        assert!(!AVAILABLE.contains(&SHOW_JSON));
    }
}
