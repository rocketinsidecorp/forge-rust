//! The README Quickstart, run as written (D21): a fresh application's
//! generated README carries the sequence from `forge new` to `forge dev`,
//! and these tests run its `bash` block line by line with the `forge` binary
//! of this build, failing on any output that asks for a manual edit. Then
//! they drive the running `forge dev` over HTTP: on PostgreSQL the owner
//! `forge setup` created signs in and lists, creates, shows, edits and
//! deletes products through the CSRF-protected forms, and uses
//! `/v1/products` as JSON with the session cookie and `X-CSRF-Token`; on
//! SQLite the same CRUD runs on the `--public` routes.
//!
//! On PostgreSQL two generators run after the Quickstart's lines and before
//! `forge dev` (then `forge migrate`): a global scaffold, `note`, which must
//! answer `403` to the signed-in owner, and a tenant-scoped JSON resource,
//! `order`, which takes the session with the CSRF header (D29). At the end
//! `forge dev` is stopped with SIGTERM and the test proves its application
//! stopped too: no process left, the port closed.
//!
//! Two lines are adapted, and only these: `forge new` also gets
//! `--forge-path` (the Forge of this checkout), and on PostgreSQL the
//! `docker compose up` line becomes a database and the migration and runtime
//! roles `docker/postgres/init.sql` would create, on the server of
//! `FORGE_TEST_DATABASE_URL` (CI's service container), written to
//! `.env.local`.
//!
//! They build a whole application, so they are ignored by default:
//!
//! ```sh
//! FORGE_TEST_DATABASE_URL=postgres://postgres:postgres@127.0.0.1:5432/postgres \
//!   cargo test -p forge-cli --test readme_quickstart -- --ignored
//! ```

use std::fs;
use std::io::{BufRead as _, Read as _, Write as _};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use forge_db::testing::RlsTestDatabase;

const OWNER_PASSWORD: &str = "quickstart owner passphrase 42";

fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// A command of the Quickstart: the `forge` of this build, the generated
/// applications' shared target directory, and no `FORGE_TEST_*` variable
/// (the application refuses unknown `FORGE_*` keys).
fn forge(directory: &Path, arguments: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_forge"));
    command.args(arguments).current_dir(directory).env(
        "CARGO_TARGET_DIR",
        Path::new(env!("CARGO_TARGET_TMPDIR")).join("readme-apps"),
    );
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("FORGE_TEST_") {
            command.env_remove(name);
        }
    }
    command
}

/// The first `bash` block after `## Quickstart` in `readme`.
fn quickstart(readme: &str) -> Vec<String> {
    let section = readme
        .split("\n## Quickstart\n")
        .nth(1)
        .unwrap_or_else(|| panic!("no Quickstart in the README:\n{readme}"));
    section
        .split("```bash\n")
        .nth(1)
        .and_then(|block| block.split("\n```").next())
        .unwrap_or_else(|| panic!("no bash block in the Quickstart:\n{section}"))
        .lines()
        .map(str::to_owned)
        .collect()
}

/// Splits a Quickstart line into words, honoring double quotes.
fn words(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quoted = false;
    for character in line.chars() {
        match character {
            '"' => quoted = !quoted,
            ' ' if !quoted => {
                if !word.is_empty() {
                    words.push(std::mem::take(&mut word));
                }
            }
            _ => word.push(character),
        }
    }
    if !word.is_empty() {
        words.push(word);
    }
    words
}

/// `forge dev`. Its application leads a process group of its own (so a
/// terminal's Ctrl-C reaches only the CLI), so it is stopped through `forge
/// dev`: SIGTERM, which stops and reaps the application, then, only past a
/// deadline, SIGKILL for `forge dev` and the application's group alike.
/// [`Dev::stop`] proves nothing survives; dropping a `Dev` (an assertion
/// failed) stops it the same way without asserting.
struct Dev {
    child: Option<Child>,
    port: u16,
}

/// How long `forge dev` gets to stop its application after SIGTERM: its own
/// grace period for the application, and some.
const STOP_DEADLINE: Duration = Duration::from_secs(30);

/// The processes whose parent is `parent` (`/proc/PID/stat`).
fn children_of(parent: i32) -> Vec<i32> {
    let Ok(entries) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().to_str()?.parse::<i32>().ok())
        .filter(|pid| {
            fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .and_then(|stat| {
                    // `PID (COMM) STATE PPID ...`; COMM may hold spaces or `)`.
                    let (_, rest) = stat.rsplit_once(')')?;
                    rest.split_whitespace().nth(1)?.parse::<i32>().ok()
                })
                == Some(parent)
        })
        .collect()
}

impl Dev {
    /// Stops `forge dev` and its application; returns the application
    /// processes that were running, and whether `forge dev` stopped by
    /// itself within [`STOP_DEADLINE`].
    fn shut_down(&mut self) -> (Vec<i32>, bool) {
        use rustix::process::{Pid, Signal, kill_process, kill_process_group};
        let Some(mut child) = self.child.take() else {
            return (Vec::new(), true);
        };
        let raw = i32::try_from(child.id()).unwrap_or_default();
        let Some(pid) = Pid::from_raw(raw) else {
            let _ = child.kill();
            let _ = child.wait();
            return (Vec::new(), false);
        };
        let applications = children_of(raw);
        let _ = kill_process(pid, Signal::TERM);
        let (exited, waited) = mpsc::channel();
        let waiter = std::thread::spawn(move || {
            let status = child.wait();
            let _ = exited.send(());
            status
        });
        let stopped = waited.recv_timeout(STOP_DEADLINE).is_ok();
        if !stopped {
            // forge dev did not stop: kill it and the application's group.
            let _ = kill_process(pid, Signal::KILL);
            for application in &applications {
                if let Some(group) = Pid::from_raw(*application) {
                    let _ = kill_process_group(group, Signal::KILL);
                }
            }
        }
        let _ = waiter.join();
        (applications, stopped)
    }

    /// Stops `forge dev` and asserts that its application stopped with it:
    /// its processes are gone and its port refuses connections.
    fn stop(mut self) {
        let (applications, stopped) = self.shut_down();
        assert!(
            stopped,
            "forge dev did not stop within {STOP_DEADLINE:?} of SIGTERM"
        );
        assert!(!applications.is_empty(), "forge dev ran no application");
        for application in applications {
            let alive = rustix::process::Pid::from_raw(application)
                .is_some_and(|pid| rustix::process::test_kill_process(pid).is_ok());
            assert!(
                !alive,
                "the application (pid {application}) outlived forge dev"
            );
        }
        assert!(
            TcpStream::connect(("127.0.0.1", self.port)).is_err(),
            "port {} still accepts connections after forge dev stopped",
            self.port
        );
    }
}

impl Drop for Dev {
    fn drop(&mut self) {
        let _ = self.shut_down();
    }
}

impl Dev {
    /// Starts `forge dev` on an ephemeral loopback port and returns it with
    /// the port of the application's `listening` log line.
    fn start(root: &Path, arguments: &[&str]) -> (Self, u16) {
        let mut command = forge(root, arguments);
        command
            .env("FORGE_HTTP_ADDR", "127.0.0.1:0")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .unwrap_or_else(|error| panic!("forge dev: {error}"));
        let (lines, reported) = mpsc::channel::<String>();
        for stream in [
            child
                .stdout
                .take()
                .map(|out| Box::new(out) as Box<dyn std::io::Read + Send>),
            child
                .stderr
                .take()
                .map(|err| Box::new(err) as Box<dyn std::io::Read + Send>),
        ]
        .into_iter()
        .flatten()
        {
            let lines = lines.clone();
            std::thread::spawn(move || {
                for line in std::io::BufReader::new(stream)
                    .lines()
                    .map_while(Result::ok)
                {
                    let _ = lines.send(line);
                }
            });
        }
        let mut dev = Self {
            child: Some(child),
            port: 0,
        };
        // The first build compiles Forge and every dependency.
        let deadline = Instant::now() + Duration::from_secs(1_800);
        let mut log = String::new();
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match reported.recv_timeout(left) {
                Ok(line) => {
                    if let Some(address) = listening_address(&line) {
                        dev.port = address.port();
                        return (dev, address.port());
                    }
                    log.push_str(&line);
                    log.push('\n');
                }
                Err(_) => break,
            }
        }
        panic!("forge dev did not report where the application listens:\n{log}");
    }
}

fn listening_address(line: &str) -> Option<SocketAddr> {
    if !line.contains("listening") || line.contains("metrics listening") {
        return None;
    }
    let (_, rest) = line.split_once("address=")?;
    rest.split(|character: char| character.is_whitespace() || character == '"')
        .next()?
        .parse()
        .ok()
}

/// Runs the Quickstart of a new application created with `new_flags` in a
/// temporary directory: every line but `forge dev`, which is started last
/// and returned. `database` stands for the `docker compose up` line.
fn run_quickstart(
    parent: &Path,
    new_flags: &[&str],
    database: &mut dyn FnMut(&Path),
    extra: &[&[&str]],
) -> (PathBuf, Dev, u16) {
    let forge_path = workspace().join("crates/forge");
    let forge_path = forge_path.to_string_lossy().into_owned();
    let mut new = vec!["new", "shop"];
    new.extend_from_slice(new_flags);
    new.extend(["--forge-path", &forge_path]);
    let created = forge(parent, &new)
        .output()
        .unwrap_or_else(|error| panic!("forge new: {error}"));
    assert!(created.status.success(), "{created:?}");
    let root = parent.join("shop");
    let readme = fs::read_to_string(root.join("README.md")).unwrap_or_default();
    let lines = quickstart(&readme);
    let expected_new = format!(
        "forge new shop{}",
        new_flags
            .iter()
            .map(|flag| format!(" {flag}"))
            .collect::<String>()
    );
    assert_eq!(lines.first(), Some(&expected_new), "{lines:?}");
    assert_eq!(
        lines.get(1).map(String::as_str),
        Some("cd shop"),
        "{lines:?}"
    );
    assert_eq!(
        lines.last().map(String::as_str),
        Some("forge dev"),
        "{lines:?}"
    );
    for line in &lines[2..lines.len() - 1] {
        if line.starts_with("docker compose up -d --wait") {
            database(&root);
            continue;
        }
        let words = words(line);
        assert_eq!(words.first().map(String::as_str), Some("forge"), "{line}");
        let arguments: Vec<&str> = words[1..].iter().map(String::as_str).collect();
        let output = forge(&root, &arguments)
            .env("FORGE_SETUP_PASSWORD", OWNER_PASSWORD)
            .stdin(Stdio::null())
            .output()
            .unwrap_or_else(|error| panic!("{line}: {error}"));
        let printed = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.status.success(), "{line}:\n{printed}");
        for manual in ["by hand", "Wire the", "paste"] {
            assert!(
                !printed.contains(manual),
                "{line} asks for a manual edit:\n{printed}"
            );
        }
    }
    // Generators run after the Quickstart's, before `forge dev`.
    for arguments in extra {
        let output = forge(&root, arguments)
            .stdin(Stdio::null())
            .output()
            .unwrap_or_else(|error| panic!("{arguments:?}: {error}"));
        assert!(output.status.success(), "{arguments:?}: {output:?}");
    }
    let (dev, port) = Dev::start(&root, &["dev"]);
    (root, dev, port)
}

/// One HTTP/1.1 response.
struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(header, _)| header.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// A client with a cookie jar: HTML forms like a browser, JSON like an API
/// client of the same session.
struct Client {
    port: u16,
    cookies: Vec<(String, String)>,
}

impl Client {
    fn new(port: u16) -> Self {
        Self {
            port,
            cookies: Vec::new(),
        }
    }

    fn send(&mut self, method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> Reply {
        let mut stream = TcpStream::connect(("127.0.0.1", self.port))
            .unwrap_or_else(|error| panic!("connect: {error}"));
        stream
            .set_read_timeout(Some(Duration::from_secs(60)))
            .unwrap_or_else(|error| panic!("{error}"));
        let mut request = format!(
            "{method} {path} HTTP/1.1\r\nHost: localhost:{}\r\nConnection: close\r\n\
             Content-Length: {}\r\n",
            self.port,
            body.len()
        );
        if !self.cookies.is_empty() {
            let cookies: Vec<String> = self
                .cookies
                .iter()
                .map(|(name, value)| format!("{name}={value}"))
                .collect();
            request.push_str(&format!("Cookie: {}\r\n", cookies.join("; ")));
        }
        for (name, value) in headers {
            request.push_str(&format!("{name}: {value}\r\n"));
        }
        request.push_str("\r\n");
        request.push_str(body);
        stream
            .write_all(request.as_bytes())
            .unwrap_or_else(|error| panic!("send: {error}"));
        let mut raw = Vec::new();
        stream
            .read_to_end(&mut raw)
            .unwrap_or_else(|error| panic!("receive: {error}"));
        let raw = String::from_utf8_lossy(&raw).into_owned();
        let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((raw.as_str(), ""));
        let mut lines = head.lines();
        let status = lines
            .next()
            .and_then(|line| line.split(' ').nth(1))
            .and_then(|code| code.parse().ok())
            .unwrap_or_else(|| panic!("status line in {head}"));
        let headers: Vec<(String, String)> = lines
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (name.trim().to_owned(), value.trim().to_owned()))
            .collect();
        for (name, value) in &headers {
            if !name.eq_ignore_ascii_case("set-cookie") {
                continue;
            }
            let Some((cookie, content)) = value
                .split(';')
                .next()
                .and_then(|pair| pair.split_once('='))
            else {
                continue;
            };
            self.cookies.retain(|(existing, _)| existing != cookie);
            if !content.is_empty() && !value.contains("Max-Age=0") {
                self.cookies.push((cookie.to_owned(), content.to_owned()));
            }
        }
        Reply {
            status,
            headers,
            body: body.to_owned(),
        }
    }

    fn page(&mut self, path: &str) -> Reply {
        self.send("GET", path, &[("Accept", "text/html")], "")
    }

    fn form(&mut self, path: &str, fields: &[(&str, &str)]) -> Reply {
        let body: Vec<String> = fields
            .iter()
            .map(|(name, value)| format!("{name}={}", encode(value)))
            .collect();
        self.send(
            "POST",
            path,
            &[
                ("Accept", "text/html"),
                ("Content-Type", "application/x-www-form-urlencoded"),
            ],
            &body.join("&"),
        )
    }

    fn json(&mut self, method: &str, path: &str, body: &str, csrf: bool) -> Reply {
        let token = self.csrf();
        let mut headers = vec![
            ("Accept", "application/json"),
            ("Content-Type", "application/json"),
        ];
        if csrf {
            headers.push(("X-CSRF-Token", token.as_str()));
        }
        self.send(method, path, &headers, body)
    }

    fn csrf(&self) -> String {
        self.cookies
            .iter()
            .find(|(name, _)| name == "__Host-forge_csrf")
            .map(|(_, value)| value.clone())
            .unwrap_or_default()
    }
}

fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' => {
                char::from(byte).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

/// The `version` field of the form or JSON in `body`.
fn version(body: &str) -> String {
    body.split("name=\"version\" value=\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .unwrap_or_else(|| panic!("no version field in {body}"))
        .to_owned()
}

/// Lists, creates, shows, edits and deletes a product through the forms,
/// then creates one through the JSON API (with and without the CSRF header)
/// and lists it.
fn crud(client: &mut Client) {
    let index = client.page("/products");
    assert_eq!(index.status, 200, "{}", index.body);
    assert!(index.body.contains("Products"), "{}", index.body);

    let refused = client.form("/products", &[("name", "Lamp"), ("price", "1999")]);
    assert_eq!(refused.status, 403, "a form without its CSRF token");

    let token = client.csrf();
    assert!(!token.is_empty());
    let created = client.form(
        "/products",
        &[("csrf_token", &token), ("name", "Lamp"), ("price", "1999")],
    );
    assert_eq!(created.status, 303, "{}", created.body);
    let show = created.header("location").unwrap_or_default().to_owned();
    assert!(
        show.starts_with("/products/") && show.len() == "/products/".len() + 36,
        "{show}"
    );
    let page = client.page(&show);
    assert_eq!(page.status, 200);
    assert!(page.body.contains("Lamp"), "{}", page.body);

    let edit = client.page(&format!("{show}/edit"));
    assert_eq!(edit.status, 200);
    let token = client.csrf();
    let updated = client.form(
        &show,
        &[
            ("csrf_token", &token),
            ("_method", "PATCH"),
            ("version", &version(&edit.body)),
            ("name", "Desk"),
            ("price", "2500"),
        ],
    );
    assert_eq!(updated.status, 303, "{}", updated.body);
    let page = client.page(&show);
    assert!(page.body.contains("Desk"), "{}", page.body);

    let token = client.csrf();
    let deleted = client.form(
        &show,
        &[
            ("csrf_token", &token),
            ("_method", "DELETE"),
            ("version", &version(&page.body)),
        ],
    );
    assert_eq!(deleted.status, 303, "{}", deleted.body);
    assert_eq!(client.page(&show).status, 404);
}

/// The JSON API with the session of `client`.
fn json_api(client: &mut Client, needs_csrf: bool) {
    let chair = r#"{"name":"Chair","price":4900}"#;
    if needs_csrf {
        let refused = client.json("POST", "/v1/products", chair, false);
        assert_eq!(refused.status, 403, "{}", refused.body);
    }
    let created = client.json("POST", "/v1/products", chair, true);
    assert_eq!(created.status, 201, "{}", created.body);
    assert!(
        created.body.contains("\"name\":\"Chair\""),
        "{}",
        created.body
    );
    let listed = client.send("GET", "/v1/products", &[("Accept", "application/json")], "");
    assert_eq!(listed.status, 200, "{}", listed.body);
    assert!(listed.body.starts_with("{\"items\":["), "{}", listed.body);
    assert!(listed.body.contains("\"price\":4900"), "{}", listed.body);
}

#[test]
#[ignore = "slow: builds a generated application; needs FORGE_TEST_DATABASE_URL"]
fn readme_quickstart_postgresql() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap_or_else(|error| panic!("{error}"));
    let parent = tempfile::tempdir().unwrap_or_else(|error| panic!("{error}"));
    let mut databases = Vec::new();
    // A global resource next to the Quickstart's: in an application with
    // Forge identity it stays closed (403) to everyone, the owner too.
    let notes: &[&str] = &["g", "scaffold", "note", "body:text", "--global"];
    // A tenant-scoped JSON resource (an SQL keyword as its name, too): the
    // session cookie, the CSRF header and the owner's permission (D29).
    let orders: &[&str] = &["g", "resource", "order", "total:integer", "--tenant-scoped"];
    let migrate: &[&str] = &["migrate"];
    let (_root, dev, port) = run_quickstart(
        parent.path(),
        &[],
        &mut |root| {
            // `docker compose up -d --wait db`: a database with the migration
            // and runtime roles of docker/postgres/init.sql.
            let database = runtime
                .block_on(RlsTestDatabase::create(std::iter::empty()))
                .unwrap_or_else(|error| panic!("FORGE_TEST_DATABASE_URL: {error}"));
            fs::write(
                root.join(".env.local"),
                format!(
                    "FORGE_DATABASE_URL={}\nFORGE_MIGRATION_DATABASE_URL={}\n",
                    database.runtime_url().expose(),
                    database.migrator_url().expose()
                ),
            )
            .unwrap_or_else(|error| panic!("{error}"));
            databases.push(database);
        },
        &[notes, orders, migrate],
    );
    assert_eq!(databases.len(), 1, "the Quickstart starts the database");

    let mut client = Client::new(port);
    let signed_out = client.page("/products");
    assert_eq!(signed_out.status, 303);
    assert_eq!(
        signed_out.header("location"),
        Some("/session/new?return_to=%2Fproducts")
    );
    let api = client.send("GET", "/v1/products", &[("Accept", "application/json")], "");
    assert_eq!(api.status, 401, "{}", api.body);

    assert_eq!(client.page("/session/new").status, 200);
    let token = client.csrf();
    let wrong = client.form(
        "/session",
        &[
            ("csrf_token", &token),
            ("tenant", "acme"),
            ("email", "you@example.com"),
            ("password", "not the password"),
        ],
    );
    assert_eq!(wrong.status, 422, "{}", wrong.body);
    let token = client.csrf();
    let signed_in = client.form(
        "/session",
        &[
            ("csrf_token", &token),
            ("tenant", "acme"),
            ("email", "you@example.com"),
            ("password", OWNER_PASSWORD),
        ],
    );
    assert_eq!(signed_in.status, 303, "{}", signed_in.body);

    crud(&mut client);
    json_api(&mut client, true);

    // Never public by accident: the global resource refuses reads and
    // writes, through the forms and the JSON API alike.
    assert_eq!(client.page("/notes").status, 403);
    let token = client.csrf();
    let write = client.form("/notes", &[("csrf_token", &token), ("body", "mine")]);
    assert_eq!(write.status, 403, "{}", write.body);
    let api = client.json("POST", "/v1/notes", r#"{"body":"mine"}"#, true);
    assert_eq!(api.status, 403, "{}", api.body);

    // The resource's JSON API: the owner's session with the CSRF header
    // creates, without it is refused, and signed out is unauthenticated.
    let order = r#"{"total":1999}"#;
    let refused = client.json("POST", "/v1/orders", order, false);
    assert_eq!(refused.status, 403, "{}", refused.body);
    let created = client.json("POST", "/v1/orders", order, true);
    assert_eq!(created.status, 201, "{}", created.body);
    let listed = client.send("GET", "/v1/orders", &[("Accept", "application/json")], "");
    assert_eq!(listed.status, 200, "{}", listed.body);
    assert!(listed.body.contains("\"total\":1999"), "{}", listed.body);
    let mut stranger = Client::new(port);
    let signed_out = stranger.send("GET", "/v1/orders", &[("Accept", "application/json")], "");
    assert_eq!(signed_out.status, 401, "{}", signed_out.body);
    dev.stop();
}

#[test]
#[ignore = "slow: builds a generated application"]
fn readme_quickstart_sqlite3() {
    let parent = tempfile::tempdir().unwrap_or_else(|error| panic!("{error}"));
    let (_root, dev, port) = run_quickstart(
        parent.path(),
        &["-d", "sqlite3"],
        &mut |_| panic!("a SQLite Quickstart starts no database server"),
        &[],
    );
    let mut client = Client::new(port);
    // --public: no sign-in, the forms still need their CSRF token.
    crud(&mut client);
    json_api(&mut client, false);
    dev.stop();
}
