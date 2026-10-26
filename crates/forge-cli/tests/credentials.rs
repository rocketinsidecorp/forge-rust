//! `forge credentials` as a subprocess with a real editor process: the
//! editor runs without a shell and without FORGE_* variables, and SIGTERM
//! during the edit wipes the plaintext and leaves the credentials unchanged.

#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

fn forge(root: &Path, arguments: &[&str], env: &[(&str, &str)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_forge"));
    command
        .args(arguments)
        .current_dir(root)
        .env_remove("FORGE_MASTER_KEY")
        .env_remove("FORGE_ENV")
        .env_remove("VISUAL");
    for (name, value) in env {
        command.env(name, value);
    }
    command.output().expect("forge executable should start")
}

/// An editor executable (a script; forge itself never starts a shell).
fn editor(dir: &Path, body: &str) -> PathBuf {
    let path = dir.join("editor.sh");
    write_executable(&path, &format!("#!/bin/sh\n{body}\n"));
    path
}

fn wait_for(path: &Path) -> String {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if let Ok(text) = fs::read_to_string(path)
            && text.ends_with('\n')
        {
            return text.trim().to_owned();
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("{} never appeared", path.display());
}

#[test]
fn edit_runs_the_editor_without_forge_variables_and_wipes_its_file() {
    let app = tempfile::tempdir().expect("app");
    let scratch = tempfile::tempdir().expect("scratch");
    let root = app.path();
    assert!(forge(root, &["credentials", "init"], &[]).status.success());
    let key = fs::read_to_string(root.join("config/credentials/development.key")).expect("key");
    let seen = scratch.path().join("seen");
    let env_dump = scratch.path().join("env");
    let script = editor(
        scratch.path(),
        &format!(
            "env > '{}'\nprintf '{{\"api_key\": \"sk_live_from_editor\"}}' > \"$1\"\necho \"$1\" > '{}'",
            env_dump.display(),
            seen.display()
        ),
    );
    let script = script.to_string_lossy().into_owned();
    let output = forge(
        root,
        &["credentials", "edit"],
        &[
            ("EDITOR", &script),
            ("FORGE_MASTER_KEY", key.trim()),
            ("FORGE_SECRET_THING", "x"),
        ],
    );
    assert!(output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .starts_with("saved config/credentials/development.json.enc")
    );
    let edited = PathBuf::from(wait_for(&seen));
    assert!(!edited.exists());
    assert!(!edited.parent().expect("dir").exists());
    let env = fs::read_to_string(&env_dump).expect("env dump");
    assert!(!env.contains("FORGE_"), "the editor saw FORGE_* variables");
    assert!(!env.contains(key.trim()));

    let shown = forge(root, &["credentials", "show"], &[]);
    assert!(shown.status.success(), "{shown:?}");
    assert_eq!(
        String::from_utf8_lossy(&shown.stdout),
        "{\n  \"api_key\": \"[REDACTED]\"\n}\n"
    );
    let revealed = forge(root, &["credentials", "show", "--reveal"], &[]);
    assert!(!revealed.status.success(), "a pipe is not a terminal");
    assert!(!String::from_utf8_lossy(&revealed.stdout).contains("sk_live_from_editor"));
}

#[test]
fn sigterm_during_the_edit_wipes_the_plaintext_and_keeps_the_credentials() {
    let app = tempfile::tempdir().expect("app");
    let scratch = tempfile::tempdir().expect("scratch");
    let root = app.path();
    assert!(forge(root, &["credentials", "init"], &[]).status.success());
    let encrypted = root.join("config/credentials/development.json.enc");
    let before = fs::read(&encrypted).expect("file");
    let seen = scratch.path().join("seen");
    let script = editor(
        scratch.path(),
        &format!(
            "printf '{{\"changed\": true}}' > \"$1\"\necho \"$1\" > '{}'\nexec sleep 30",
            seen.display()
        ),
    );
    let mut child = Command::new(env!("CARGO_BIN_EXE_forge"))
        .args(["credentials", "edit"])
        .current_dir(root)
        .env("EDITOR", &script)
        .env_remove("VISUAL")
        .env_remove("FORGE_MASTER_KEY")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("forge");
    let edited = PathBuf::from(wait_for(&seen));
    assert!(
        edited.exists(),
        "the plaintext exists while the editor runs"
    );
    let pid = rustix::process::Pid::from_raw(i32::try_from(child.id()).expect("pid")).expect("pid");
    rustix::process::kill_process(pid, rustix::process::Signal::TERM).expect("SIGTERM");
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(status) = child.try_wait().expect("wait") {
            break status;
        }
        assert!(Instant::now() < deadline, "forge did not stop");
        std::thread::sleep(Duration::from_millis(20));
    };
    let output = child.wait_with_output().expect("output");
    assert!(!status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("interrupted by SIGTERM"), "{stderr}");
    assert!(
        !edited.parent().expect("dir").exists(),
        "the private directory was left behind"
    );
    assert_eq!(fs::read(&encrypted).expect("file"), before);
}

#[test]
fn sighup_during_the_edit_wipes_the_plaintext_and_keeps_the_credentials() {
    let app = tempfile::tempdir().expect("app");
    let scratch = tempfile::tempdir().expect("scratch");
    let root = app.path();
    assert!(forge(root, &["credentials", "init"], &[]).status.success());
    let encrypted = root.join("config/credentials/development.json.enc");
    let before = fs::read(&encrypted).expect("file");
    let seen = scratch.path().join("seen");
    let script = editor(
        scratch.path(),
        &format!(
            "printf '{{\"changed\": true}}' > \"$1\"\necho \"$1\" > '{}'\nexec sleep 30",
            seen.display()
        ),
    );
    let mut child = Command::new(env!("CARGO_BIN_EXE_forge"))
        .args(["credentials", "edit"])
        .current_dir(root)
        .env("EDITOR", &script)
        .env_remove("VISUAL")
        .env_remove("FORGE_MASTER_KEY")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("forge");
    let edited = PathBuf::from(wait_for(&seen));
    assert!(
        edited.exists(),
        "the plaintext exists while the editor runs"
    );
    let pid = rustix::process::Pid::from_raw(i32::try_from(child.id()).expect("pid")).expect("pid");
    rustix::process::kill_process(pid, rustix::process::Signal::HUP).expect("SIGHUP");
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(status) = child.try_wait().expect("wait") {
            break status;
        }
        assert!(Instant::now() < deadline, "forge did not stop");
        std::thread::sleep(Duration::from_millis(20));
    };
    let output = child.wait_with_output().expect("output");
    assert!(!status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("interrupted by SIGHUP"), "{stderr}");
    assert!(
        !edited.parent().expect("dir").exists(),
        "the private directory was left behind"
    );
    assert_eq!(fs::read(&encrypted).expect("file"), before);
}

#[test]
fn rotate_key_replaces_the_key_and_prints_it_only_when_asked() {
    let app = tempfile::tempdir().expect("app");
    let root = app.path();
    assert!(forge(root, &["credentials", "init"], &[]).status.success());
    let key_file = root.join("config/credentials/development.key");
    let first = fs::read_to_string(&key_file).expect("key");

    // The report goes to stderr; stdout stays empty unless the key is asked
    // for.
    let rotated = forge(
        root,
        &["credentials", "rotate-key", "--write-key-file"],
        &[],
    );
    assert!(rotated.status.success(), "{rotated:?}");
    assert!(rotated.stdout.is_empty(), "{rotated:?}");
    let report = String::from_utf8_lossy(&rotated.stderr);
    assert!(
        report.starts_with("re-encrypted config/credentials/development.json.enc with key "),
        "{report}"
    );
    assert!(
        report.contains("the previous key is no longer accepted"),
        "{report}"
    );
    let second = fs::read_to_string(&key_file).expect("key");
    assert_ne!(first, second, "the key file holds the new key");
    assert!(forge(root, &["credentials", "show"], &[]).status.success());

    let kept = forge(
        root,
        &[
            "credentials",
            "rotate-key",
            "--keep-old",
            "--write-key-file",
        ],
        &[],
    );
    assert!(kept.status.success(), "{kept:?}");
    assert!(
        String::from_utf8_lossy(&kept.stderr).contains("the previous key list stays readable"),
        "{}",
        String::from_utf8_lossy(&kept.stderr)
    );

    let printed = forge(root, &["credentials", "rotate-key", "--print-key"], &[]);
    assert!(printed.status.success(), "{printed:?}");
    let stdout = String::from_utf8_lossy(&printed.stdout).into_owned();
    let new_key = stdout
        .lines()
        .next()
        .expect("the key is the first line")
        .to_owned();
    assert_eq!(
        fs::read_to_string(&key_file).expect("key").trim(),
        new_key,
        "a key file source keeps the printed key in the key file"
    );
    let shown = forge(
        root,
        &["credentials", "show"],
        &[("FORGE_MASTER_KEY", &new_key)],
    );
    assert!(shown.status.success(), "{shown:?}");
}

/// Writes an executable (mode 0755) script at `path` without this process
/// ever holding a write descriptor on it.
///
/// Writing the file here (`std::fs::write`) leaves a writable descriptor open
/// in this process for a moment. Any other test thread that forks in that
/// window (`Command::spawn`) inherits the descriptor until it execs, and
/// while any process holds the inode open for writing `execve` of it fails
/// with `ETXTBSY` ("Text file busy"). `O_CLOEXEC` only closes the descriptor
/// at exec, so it does not close that window. Creating the file from a
/// short-lived child keeps the writer out of this process, so forks on other
/// threads cannot inherit it, and the child has exited before this returns.
fn write_executable(path: &Path, body: &str) {
    use std::io::Write as _;
    use std::process::{Command, Stdio};

    let mut child = Command::new("/bin/sh")
        .args(["-c", "umask 022; cat > \"$1\" && chmod 755 \"$1\"", "sh"])
        .arg(path)
        .stdin(Stdio::piped())
        .spawn()
        .expect("spawn sh to write the executable");
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(body.as_bytes())
        .expect("send the script body");
    let status = child.wait().expect("wait for sh");
    assert!(
        status.success(),
        "writing {} failed: {status}",
        path.display()
    );
}
