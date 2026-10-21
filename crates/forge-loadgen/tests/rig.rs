//! The load generator against a stand-in for a generated application: it
//! signs in, creates rows, measures four scenarios and reports them in the
//! shared formats. (The real application is exercised by `scripts/bench.sh`.)

use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Output};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

const TENANT: &str = "01941f29-7c00-7000-8000-000000000001";
const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const SESSION: &str = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

/// What the stand-in application lists on its index page.
struct App {
    rows: usize,
    created: AtomicUsize,
}

fn respond(stream: &mut TcpStream, status: &str, headers: &str, body: &str) {
    let text = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\n{headers}\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(text.as_bytes());
}

fn serve(mut stream: TcpStream, app: &App) {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        let (head, body_start) = loop {
            if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
                break (
                    String::from_utf8_lossy(&buffer[..end]).into_owned(),
                    end + 4,
                );
            }
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => return,
                Ok(read) => buffer.extend_from_slice(&chunk[..read]),
            }
        };
        let length = head
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length:")?
                    .trim()
                    .parse()
                    .ok()
            })
            .unwrap_or(0_usize);
        while buffer.len() < body_start + length {
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => return,
                Ok(read) => buffer.extend_from_slice(&chunk[..read]),
            }
        }
        let body = String::from_utf8_lossy(&buffer[body_start..body_start + length]).into_owned();
        buffer.drain(..body_start + length);
        let line = head.lines().next().unwrap_or_default().to_owned();
        let cookies = head
            .lines()
            .find_map(|line| line.strip_prefix("Cookie: "))
            .unwrap_or_default()
            .to_owned();
        let signed_in = cookies.contains(&format!("__Host-forge_session={SESSION}"));
        match line.as_str() {
            "GET /health/ready HTTP/1.1" => {
                respond(&mut stream, "200 OK", "", r#"{"status":"ready"}"#)
            }
            "GET / HTTP/1.1" => respond(&mut stream, "200 OK", "", "<h1>Welcome</h1>"),
            "GET /session/new HTTP/1.1" => respond(
                &mut stream,
                "200 OK",
                &format!("Set-Cookie: __Host-forge_csrf={TOKEN}; Path=/; Secure; HttpOnly\r\n"),
                &format!(r#"<form><input type="hidden" name="csrf_token" value="{TOKEN}"></form>"#),
            ),
            "POST /session HTTP/1.1" => {
                if body.starts_with(&format!(
                    "csrf_token={TOKEN}&tenant=bench&email=bench%40example.com&password="
                )) {
                    respond(
                        &mut stream,
                        "303 See Other",
                        &format!(
                            "Location: /\r\nSet-Cookie: __Host-forge_session={SESSION}; Path=/\r\nSet-Cookie: __Host-forge_csrf={SESSION}; Path=/\r\n"
                        ),
                        "",
                    );
                } else {
                    respond(&mut stream, "422 Unprocessable Entity", "", "no");
                }
            }
            post if post == format!("POST /tenants/{TENANT}/posts HTTP/1.1") => {
                if signed_in && body.starts_with(&format!("csrf_token={SESSION}&title=")) {
                    let id = app.created.fetch_add(1, Ordering::Relaxed);
                    respond(
                        &mut stream,
                        "303 See Other",
                        &format!("Location: /tenants/{TENANT}/posts/{id}\r\n"),
                        "",
                    );
                } else {
                    respond(&mut stream, "403 Forbidden", "", "no");
                }
            }
            get if get == format!("GET /tenants/{TENANT}/posts HTTP/1.1") && signed_in => {
                let rows: String = (0..app.rows)
                    .map(|id| format!("<tr><td><a href=\"/tenants/{TENANT}/posts/{id}/edit\">Edit</a></td></tr>"))
                    .collect();
                respond(&mut stream, "200 OK", "", &format!("<table>{rows}</table>"));
            }
            _ => respond(&mut stream, "404 Not Found", "", "not found"),
        }
    }
}

fn start(rows: usize) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0");
    let Ok(listener) = listener else {
        panic!("bind");
    };
    let Ok(address) = listener.local_addr() else {
        panic!("address");
    };
    let app = Arc::new(App {
        rows,
        created: AtomicUsize::new(0),
    });
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let app = Arc::clone(&app);
            thread::spawn(move || serve(stream, &app));
        }
    });
    address.port()
}

fn loadgen(port: u16, extra: &[&str]) -> Output {
    let output = Command::new(env!("CARGO_BIN_EXE_loadgen"))
        .env("BENCH_PASSWORD", "correct horse")
        .args([
            "-base",
            &format!("http://127.0.0.1:{port}"),
            "-tenant-id",
            TENANT,
            "-concurrency",
            "4",
            "-duration",
            "300ms",
            "-warmup",
            "100ms",
            "-runs",
            "2",
            "-rows",
            "3",
        ])
        .args(extra)
        .output();
    let Ok(output) = output else {
        panic!("loadgen did not start");
    };
    output
}

#[test]
fn measures_the_four_scenarios_and_prints_the_shared_table() {
    let output = loadgen(start(3), &[]);
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        output.status.success(),
        "{text}{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        text.starts_with("concurrency 4, 300ms per run, 100ms warm-up, 2 runs, index of 3 rows; figures are medians over the runs\n\n| Scenario | Requests/s | Requests/s (min-max) | p50 ms | p95 ms | p99 ms | Errors |\n"),
        "{text}"
    );
    for scenario in ["health", "welcome", "index_html", "create_form"] {
        let row = text
            .lines()
            .find(|line| line.starts_with(&format!("| `{scenario}` |")))
            .unwrap_or_else(|| panic!("{scenario} in {text}"));
        assert!(row.ends_with("| 0 |"), "{row}");
    }
    assert!(!text.contains("show_json"));
}

#[test]
fn prints_json_with_the_shared_keys() {
    let output = loadgen(
        start(3),
        &["-format", "json", "-scenarios", "health,index_html"],
    );
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        output.status.success(),
        "{text}{}",
        String::from_utf8_lossy(&output.stderr)
    );
    for key in [
        "\"concurrency\": 4",
        "\"duration_seconds\": 0.3",
        "\"index_rows\": 3",
        "\"scenario\": \"health\"",
        "\"scenario\": \"index_html\"",
        "\"median\": {",
        "\"errors\": 0",
        "\"runs\": 2",
        "\"warmup_seconds\": 0.1",
        "\"requests_per_second\"",
        "\"p99_ms\"",
    ] {
        assert!(text.contains(key), "{key} in {text}");
    }
    assert!(!text.contains("create_form"));
}

#[test]
fn refuses_an_index_that_does_not_list_the_claimed_rows() {
    let output = loadgen(start(2), &[]);
    assert!(!output.status.success());
    let message = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        message.contains("the index lists 2 rows, want 3"),
        "{message}"
    );
}

#[test]
fn refuses_show_json_and_a_missing_password_without_touching_the_network() {
    let output = loadgen(1, &["-scenarios", "show_json"]);
    assert!(String::from_utf8_lossy(&output.stderr).contains("show_json is not available"));
    let output = Command::new(env!("CARGO_BIN_EXE_loadgen"))
        .env_remove("BENCH_PASSWORD")
        .args(["-tenant-id", TENANT])
        .output();
    let Ok(output) = output else {
        panic!("loadgen did not start");
    };
    assert!(String::from_utf8_lossy(&output.stderr).contains("BENCH_PASSWORD is required"));
}

#[test]
fn an_unreachable_application_is_waited_for_not_measured() {
    // A port nothing listens on: the load generator keeps polling readiness
    // (for up to 30 s) instead of crashing or reporting numbers.
    let listener = TcpListener::bind("127.0.0.1:0");
    let Ok(listener) = listener else {
        panic!("bind");
    };
    let Ok(address) = listener.local_addr() else {
        panic!("address");
    };
    drop(listener);
    let mut child = Command::new(env!("CARGO_BIN_EXE_loadgen"))
        .env("BENCH_PASSWORD", "x")
        .args([
            "-base",
            &format!("http://127.0.0.1:{}", address.port()),
            "-tenant-id",
            TENANT,
        ])
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap_or_else(|error| panic!("{error}"));
    thread::sleep(std::time::Duration::from_millis(800));
    // Still waiting for readiness: no report, no crash.
    assert!(
        child
            .try_wait()
            .map(|status| status.is_none())
            .unwrap_or(false)
    );
    let _ = child.kill();
    let _ = child.wait();
}
