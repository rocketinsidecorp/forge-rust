//! Stands in for the docker CLI in the real-sshd tests of `forge deploy`
//! (src/deploy/ssh_tests.rs), where no Docker daemon is available: the
//! Rust port of Forge-Go's internal/deploy/internal/fakedocker. It keeps
//! containers, images, networks and forge-proxy routes in a JSON file in
//! `$FAKE_DOCKER_STATE` and implements exactly the invocations `forge
//! deploy` issues, failing on anything else so the tests notice drift.
//!
//! Failure switches:
//! - an image tag containing `badmigrate` fails `run --rm ... migrate`;
//! - a file named `unhealthy` in the state directory makes forge-proxy
//!   deploy exit 4, like a target that never passes its health check.
//!
//! Test support only: built by `cargo test`, never installed.

use std::collections::BTreeMap;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde::{Deserialize, Serialize};

#[derive(Clone, Default, Deserialize, Serialize)]
struct Container {
    name: String,
    image: String,
    labels: BTreeMap<String, String>,
    state: String,
    seq: u64,
    env: BTreeMap<String, String>,
    args: Vec<String>,
    publish: Vec<String>,
}

#[derive(Default, Deserialize, Serialize)]
struct State {
    networks: Vec<String>,
    images: Vec<String>,
    containers: Vec<Container>,
    routes: BTreeMap<String, String>,
    maintenance: BTreeMap<String, bool>,
    logins: Vec<String>,
    one_offs: Vec<Vec<String>>,
    env_seen: BTreeMap<String, String>,
    seq: u64,
}

impl State {
    fn find(&mut self, name: &str) -> Option<&mut Container> {
        self.containers.iter_mut().find(|c| c.name == name)
    }
}

fn fail(code: u8, message: &str) -> ExitCode {
    eprintln!("{message}");
    ExitCode::from(code)
}

fn created(seq: u64) -> String {
    // 2026-10-19T10:00:00Z plus one second per container.
    let base = 1_792_404_000_u64 + seq;
    let seconds = base % 60;
    let minutes = base / 60 % 60;
    let hours = base / 3600 % 24;
    format!("2026-10-19 {hours:02}:{minutes:02}:{seconds:02} +0000 UTC")
}

fn main() -> ExitCode {
    let Some(dir) = std::env::var_os("FAKE_DOCKER_STATE").map(PathBuf::from) else {
        return fail(125, "FAKE_DOCKER_STATE is not set");
    };
    let args: Vec<String> = std::env::args().skip(1).collect();
    let lock_path = dir.join("lock");
    let Ok(lock_file) = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
    else {
        return fail(125, "cannot open lock");
    };
    // `logs --follow` blocks: it must not hold the state lock.
    if args.first().map(String::as_str) == Some("logs") {
        return logs(&dir, &args);
    }
    if rustix::fs::flock(&lock_file, rustix::fs::FlockOperation::LockExclusive).is_err() {
        return fail(125, "cannot lock state");
    }
    if let Ok(mut calls) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("calls.log"))
    {
        let _ = writeln!(calls, "docker {}", args.join(" "));
    }
    let path = dir.join("state.json");
    let mut state: State = std::fs::read(&path)
        .ok()
        .and_then(|content| serde_json::from_slice(&content).ok())
        .unwrap_or_default();
    let code = run(&dir, &mut state, &args);
    if let Ok(content) = serde_json::to_vec_pretty(&state) {
        let _ = std::fs::write(&path, content);
    }
    code
}

fn run(dir: &Path, state: &mut State, args: &[String]) -> ExitCode {
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    match words.as_slice() {
        ["version", "--format", _] => {
            println!("27.3.1");
            ExitCode::SUCCESS
        }
        ["network", "inspect", "--format", _, name] => {
            if state.networks.iter().any(|n| n == name) {
                println!("172.18.0.0/16");
                ExitCode::SUCCESS
            } else {
                fail(1, &format!("Error: No such network: {name}"))
            }
        }
        ["network", "inspect", name] => {
            if state.networks.iter().any(|n| n == name) {
                println!("[]");
                ExitCode::SUCCESS
            } else {
                fail(1, &format!("Error: No such network: {name}"))
            }
        }
        ["network", "create", name] => {
            state.networks.push((*name).to_owned());
            println!("network-{name}");
            ExitCode::SUCCESS
        }
        ["login", rest @ ..] => {
            let mut password = String::new();
            let _ = std::io::stdin().read_to_string(&mut password);
            let user = rest
                .iter()
                .position(|w| *w == "--username")
                .and_then(|i| rest.get(i + 1))
                .copied()
                .unwrap_or_default();
            if !rest.contains(&"--password-stdin") {
                return fail(1, "password must come from stdin");
            }
            state
                .logins
                .push(format!("{user} password={}", password.trim_end()));
            println!("Login Succeeded");
            ExitCode::SUCCESS
        }
        ["pull", reference] => {
            if !state.images.iter().any(|i| i == reference) {
                state.images.push((*reference).to_owned());
            }
            ExitCode::SUCCESS
        }
        ["run", rest @ ..] => run_container(state, rest),
        ["container", "inspect", name] => {
            if state.find(name).is_some() {
                println!("[]");
                ExitCode::SUCCESS
            } else {
                fail(1, &format!("Error: No such container: {name}"))
            }
        }
        ["start", name] => match state.find(name) {
            Some(container) => {
                "running".clone_into(&mut container.state);
                println!("{name}");
                ExitCode::SUCCESS
            }
            None => fail(1, &format!("Error: No such container: {name}")),
        },
        ["stop", "-t", _, names @ ..] => {
            for name in names {
                match state.find(name) {
                    Some(container) => "exited".clone_into(&mut container.state),
                    None => return fail(1, &format!("Error: No such container: {name}")),
                }
                println!("{name}");
            }
            ExitCode::SUCCESS
        }
        ["rm", names @ ..] => {
            for name in names {
                match state.find(name) {
                    Some(container) if container.state == "running" => {
                        return fail(1, &format!("cannot remove running container {name}"));
                    }
                    Some(_) => state.containers.retain(|c| c.name != *name),
                    None => return fail(1, &format!("Error: No such container: {name}")),
                }
                println!("{name}");
            }
            ExitCode::SUCCESS
        }
        ["ps", rest @ ..] => ps(state, rest),
        ["inspect", "--format", format, name] => match state.find(name) {
            Some(container) if format.contains(".Config.Image") => {
                println!("{}", container.image);
                ExitCode::SUCCESS
            }
            Some(container) => {
                println!("{} none", container.state);
                ExitCode::SUCCESS
            }
            None => fail(1, &format!("Error: No such object: {name}")),
        },
        ["image", "ls", "--format", _, repository] => {
            for image in &state.images {
                if let Some((repo, tag)) = image.rsplit_once(':')
                    && repo == *repository
                {
                    println!("{}", serde_json::json!({"Repository": repo, "Tag": tag}));
                }
            }
            ExitCode::SUCCESS
        }
        ["image", "rm", reference] => {
            state.images.retain(|i| i != reference);
            println!("Untagged: {reference}");
            ExitCode::SUCCESS
        }
        [
            "exec",
            "forge-proxy",
            "/usr/local/bin/forge-proxy",
            rest @ ..,
        ] => proxy(dir, state, rest),
        ["exec", rest @ ..] => {
            let rest: Vec<&str> = rest
                .iter()
                .copied()
                .filter(|w| !w.starts_with("--"))
                .collect();
            let Some((name, command)) = rest.split_first() else {
                return fail(125, "exec needs a container");
            };
            if state.find(name).is_none() {
                return fail(1, &format!("Error: No such container: {name}"));
            }
            println!("exec in {name}: {}", command.join(" "));
            ExitCode::SUCCESS
        }
        _ => fail(
            125,
            &format!("fake docker: unsupported invocation: {}", args.join(" ")),
        ),
    }
}

fn run_container(state: &mut State, rest: &[&str]) -> ExitCode {
    let mut container = Container::default();
    let mut one_off = false;
    let mut env_file = String::new();
    let mut index = 0;
    while index < rest.len() {
        let word = rest[index];
        match word {
            "--rm" => one_off = true,
            "--detach" | "--interactive" | "--tty" => {}
            "--name" | "--network" | "--restart" | "--stop-timeout" | "--env-file" | "--env"
            | "--label" | "--volume" | "--publish" => {
                let Some(value) = rest.get(index + 1) else {
                    return fail(125, &format!("{word} needs a value"));
                };
                match word {
                    "--name" => (*value).clone_into(&mut container.name),
                    "--env-file" => (*value).clone_into(&mut env_file),
                    "--env" => {
                        if let Some((key, val)) = value.split_once('=') {
                            container.env.insert(key.to_owned(), val.to_owned());
                        }
                    }
                    "--label" => {
                        if let Some((key, val)) = value.split_once('=') {
                            container.labels.insert(key.to_owned(), val.to_owned());
                        }
                    }
                    "--publish" => container.publish.push((*value).to_owned()),
                    _ => {}
                }
                index += 1;
            }
            _ if word.starts_with("--") => return fail(125, &format!("unsupported flag {word}")),
            _ => break,
        }
        index += 1;
    }
    let Some((image, args)) = rest[index..].split_first() else {
        return fail(125, "run needs an image");
    };
    let env_content = if env_file.is_empty() {
        String::new()
    } else {
        match std::fs::read_to_string(&env_file) {
            Ok(content) => content,
            Err(error) => return fail(125, &format!("open {env_file}: {error}")),
        }
    };
    if one_off {
        let mut record = vec![(*image).to_owned()];
        record.extend(args.iter().map(|a| (*a).to_owned()));
        state.one_offs.push(record);
        if image.contains("badmigrate") && args.first() == Some(&"migrate") {
            return fail(1, "migration 0002_add_index failed: syntax error");
        }
        println!("ran {}", args.join(" "));
        return ExitCode::SUCCESS;
    }
    if state.find(&container.name).is_some() {
        return fail(
            125,
            &format!(
                "Conflict. The container name \"/{}\" is already in use",
                container.name
            ),
        );
    }
    for line in env_content.lines() {
        if let Some((key, value)) = line.split_once('=') {
            container
                .env
                .entry(key.to_owned())
                .or_insert_with(|| value.to_owned());
        }
    }
    state.seq += 1;
    container.seq = state.seq;
    (*image).clone_into(&mut container.image);
    container.args = args.iter().map(|a| (*a).to_owned()).collect();
    "running".clone_into(&mut container.state);
    state.env_seen.insert(container.name.clone(), env_content);
    println!("{}", container.name);
    state.containers.push(container);
    ExitCode::SUCCESS
}

fn ps(state: &State, rest: &[&str]) -> ExitCode {
    let mut filters = Vec::new();
    let mut latest = false;
    let mut index = 0;
    while index < rest.len() {
        match rest[index] {
            "--filter" => {
                filters.push(rest.get(index + 1).copied().unwrap_or_default());
                index += 1;
            }
            "--format" => index += 1,
            "--latest" => latest = true,
            "--all" | "--no-trunc" | "--quiet" => {}
            other => return ExitCode::from(fail_code(&format!("unsupported ps argument {other}"))),
        }
        index += 1;
    }
    let mut matching: Vec<&Container> = state
        .containers
        .iter()
        .filter(|c| {
            filters.iter().all(|filter| {
                if let Some(label) = filter.strip_prefix("label=") {
                    let (key, value) = label.split_once('=').unwrap_or((label, ""));
                    c.labels.get(key).is_some_and(|v| v == value)
                } else if let Some(status) = filter.strip_prefix("status=") {
                    c.state == status
                } else {
                    false
                }
            })
        })
        .collect();
    matching.sort_by_key(|c| std::cmp::Reverse(c.seq));
    if latest {
        if let Some(container) = matching.first() {
            println!("{}", container.name);
        }
        return ExitCode::SUCCESS;
    }
    for container in matching {
        let labels: Vec<String> = container
            .labels
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        println!(
            "{}",
            serde_json::json!({
                "Names": container.name, "Labels": labels.join(","), "State": container.state,
                "Image": container.image, "CreatedAt": created(container.seq),
            })
        );
    }
    ExitCode::SUCCESS
}

fn fail_code(message: &str) -> u8 {
    eprintln!("{message}");
    125
}

fn proxy(dir: &Path, state: &mut State, rest: &[&str]) -> ExitCode {
    let Some((command, rest)) = rest.split_first() else {
        return fail(125, "forge-proxy needs a command");
    };
    let mut options = BTreeMap::new();
    let mut positional = Vec::new();
    let mut index = 0;
    while index < rest.len() {
        let word = rest[index];
        if let Some(name) = word.strip_prefix('-') {
            if name == "tls" {
                options.insert("tls", "true".to_owned());
            } else if let Some(value) = rest.get(index + 1) {
                options.insert(name, (*value).to_owned());
                index += 1;
            }
        } else {
            positional.push(word);
        }
        index += 1;
    }
    let service = options.get("service").cloned().unwrap_or_default();
    match *command {
        "deploy" => {
            let target = options.get("target").cloned().unwrap_or_default();
            let name = target.split(':').next().unwrap_or_default().to_owned();
            let Some(app) = state.find(&name) else {
                return fail(4, &format!("target {target} not healthy: request failed"));
            };
            if app.state != "running" {
                return fail(4, &format!("target {target} not healthy: request failed"));
            }
            if dir.join("unhealthy").exists() {
                return fail(
                    4,
                    &format!(
                        "target {target} not healthy within {}: status 503",
                        options.get("deploy-timeout").cloned().unwrap_or_default()
                    ),
                );
            }
            let previous = state
                .routes
                .insert(service.clone(), target.clone())
                .unwrap_or_default();
            println!(
                "{}",
                serde_json::json!({"ok": true, "deploy": {"service": service, "target": target, "previous": previous, "drained": true}})
            );
            ExitCode::SUCCESS
        }
        "status" => {
            let services: Vec<serde_json::Value> = state
                .routes
                .iter()
                .map(|(service, target)| {
                    serde_json::json!({
                        "service": service, "target": target, "hosts": [],
                        "maintenance": state.maintenance.get(service).copied().unwrap_or(false),
                    })
                })
                .collect();
            println!("{}", serde_json::json!({"ok": true, "services": services}));
            ExitCode::SUCCESS
        }
        "maintenance" => {
            let [toggle] = positional.as_slice() else {
                return fail(2, "maintenance needs on|off");
            };
            state.maintenance.insert(service, *toggle == "on");
            println!("{{\"ok\":true}}");
            ExitCode::SUCCESS
        }
        other => fail(125, &format!("unsupported forge-proxy command {other}")),
    }
}

/// Prints a few lines, including the container's database URL so the tests
/// can check redaction, then blocks with `--follow` until killed.
fn logs(dir: &Path, args: &[String]) -> ExitCode {
    let follow = args.iter().any(|a| a == "--follow");
    let Some(name) = args.last() else {
        return fail(1, "logs needs a container");
    };
    let state: State = std::fs::read(dir.join("state.json"))
        .ok()
        .and_then(|content| serde_json::from_slice(&content).ok())
        .unwrap_or_default();
    if !state.containers.iter().any(|c| c.name == *name) {
        return fail(1, &format!("no such container {name}"));
    }
    let secret = state
        .env_seen
        .get(name)
        .and_then(|content| {
            content.lines().find_map(|line| {
                line.trim()
                    .strip_prefix("FORGE_DATABASE_URL=")
                    .map(str::to_owned)
            })
        })
        .unwrap_or_default();
    let mut out = std::io::stdout();
    let _ = writeln!(out, "2026-10-19T10:00:00Z booted {name}");
    let _ = writeln!(out, "2026-10-19T10:00:01Z connecting to {secret}");
    let _ = writeln!(out, "2026-10-19T10:00:02Z ERROR something failed in {name}");
    let _ = out.flush();
    if !follow {
        return ExitCode::SUCCESS;
    }
    let pid = std::process::id();
    let _ = std::fs::write(dir.join(format!("logs-{pid}.pid")), pid.to_string());
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}
