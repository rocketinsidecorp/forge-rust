//! `forge credentials` against temporary applications, with fake editors,
//! simulated signals and the failure paths Forge-Go tests.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use forge_credentials::{load_keys, parse_keys, read_file, seal, unseal, write_file};

use crate::CliError;
use crate::credentials::{Action, Context, WriteHook};
use crate::credentials_edit::Interrupts;

const SECRET: &str = "sk_live_DISTINCTIVE_9876";

struct App {
    root: tempfile::TempDir,
    env: HashMap<String, String>,
    tty: bool,
    stdout: String,
    stderr: String,
    /// Passed to `rotate-key` as its after-write hook.
    after_write: Option<WriteHook<'static>>,
}

type Editor<'e> = Box<dyn FnMut(&[String], &Path, &Interrupts) -> Result<(), String> + 'e>;

impl App {
    fn new() -> Self {
        let mut env = HashMap::new();
        env.insert("EDITOR".to_owned(), "fake-editor".to_owned());
        Self {
            root: tempfile::tempdir().expect("temporary directory"),
            env,
            tty: false,
            stdout: String::new(),
            stderr: String::new(),
            after_write: None,
        }
    }

    fn dir(&self) -> PathBuf {
        self.root.path().join("config/credentials")
    }

    fn run_with(
        &mut self,
        action: Action,
        environment: Option<&str>,
        editor: Editor<'_>,
        interrupts: &Interrupts,
        after_read: Option<Box<dyn FnMut() + '_>>,
    ) -> Result<(), CliError> {
        let env = self.env.clone();
        let lookup = move |name: &str| env.get(name).cloned();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut after_write = self.after_write.take();
        let result = {
            let mut context = Context {
                root: self.root.path().to_path_buf(),
                lookup: &lookup,
                stdout: &mut stdout,
                stderr: &mut stderr,
                stdout_is_terminal: self.tty,
                editor,
                interrupts,
                after_read,
                after_write: after_write
                    .as_mut()
                    .map(|hook| Box::new(move |step: &str| hook(step)) as WriteHook<'_>),
            };
            context.run(&action, environment.map(str::to_owned))
        };
        self.after_write = after_write;
        self.stdout = String::from_utf8(stdout).expect("utf8");
        self.stderr = String::from_utf8(stderr).expect("utf8");
        result
    }

    fn run(&mut self, action: Action) -> Result<(), CliError> {
        self.run_with(
            action,
            None,
            Box::new(|_, _, _| Ok(())),
            &Interrupts::none(),
            None,
        )
    }

    fn edit(&mut self, editor: Editor<'_>) -> Result<(), CliError> {
        self.run_with(Action::Edit, None, editor, &Interrupts::none(), None)
    }

    fn file(&self) -> Vec<u8> {
        read_file(&self.dir(), "development").expect("file")
    }

    fn plaintext(&self) -> String {
        let (keys, _) = load_keys(&self.dir(), "development", &|_| None).expect("keys");
        String::from_utf8(
            unseal("development", &self.file(), &keys)
                .expect("open")
                .to_vec(),
        )
        .expect("utf8")
    }
}

fn initialized() -> App {
    let mut app = App::new();
    app.run(Action::Init).expect("init");
    app
}

/// An editor that replaces the file with `content` and records where it was.
fn writing<'s>(content: &str, seen: &'s mut Option<PathBuf>) -> Editor<'s> {
    let content = content.to_owned();
    Box::new(move |command, path, _| {
        assert_eq!(command, ["fake-editor"]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            let mode = |path: &Path| fs::metadata(path).expect("meta").permissions().mode() & 0o777;
            assert_eq!(mode(path), 0o600);
            assert_eq!(mode(path.parent().expect("dir")), 0o700);
        }
        assert!(
            fs::read_to_string(path)
                .expect("plaintext")
                .contains("\"encryption\"")
        );
        *seen = Some(path.to_path_buf());
        fs::write(path, &content).map_err(|error| error.to_string())
    })
}

fn gone(path: Option<PathBuf>) {
    let path = path.expect("the editor ran");
    assert!(!path.exists(), "{} was left behind", path.display());
    assert!(
        !path.parent().expect("dir").exists(),
        "the private directory was left behind"
    );
}

#[test]
fn init_creates_a_private_key_the_file_and_ignore_entries_once() {
    let mut app = App::new();
    fs::write(app.root.path().join(".gitignore"), "/target").expect("gitignore");
    app.run(Action::Init).expect("init");
    let key = fs::read_to_string(app.dir().join("development.key")).expect("key");
    assert!(
        app.stdout
            .contains("  create  config/credentials/development.key")
    );
    assert!(
        app.stdout
            .contains("  create  config/credentials/development.json.enc")
    );
    assert!(
        app.stdout
            .contains("FORGE_MASTER_KEY=$(cat config/credentials/development.key)")
    );
    assert!(
        !app.stdout.contains(key.trim()),
        "init never prints the key"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        let mode = fs::metadata(app.dir().join("development.key"))
            .expect("meta")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    assert_eq!(
        fs::read_to_string(app.root.path().join(".gitignore")).expect("gitignore"),
        "/target\n# Credential keys: never commit.\n/config/credentials/*.key\n"
    );
    assert_eq!(
        fs::read_to_string(app.root.path().join(".dockerignore")).expect("dockerignore"),
        "config/credentials/*.key\n"
    );
    // Production gets its own key; the ignore entries are not repeated.
    app.run_with(
        Action::Init,
        Some("production"),
        Box::new(|_, _, _| Ok(())),
        &Interrupts::none(),
        None,
    )
    .expect("production");
    assert!(app.dir().join("production.key").exists());
    let gitignore = fs::read_to_string(app.root.path().join(".gitignore")).expect("gitignore");
    assert_eq!(gitignore.matches("/config/credentials/*.key").count(), 1);
    // A second init refuses and keeps the key.
    let error = app.run(Action::Init).expect_err("exists");
    assert!(
        error
            .to_string()
            .contains("already exists; use `forge credentials edit --environment development`")
    );
    assert_eq!(
        fs::read_to_string(app.dir().join("development.key")).expect("key"),
        key
    );
}

#[test]
fn edit_saves_valid_changes_and_wipes_the_plaintext() {
    let mut app = initialized();
    let mut seen = None;
    let document = format!("{{\"stripe\": {{\"api_key\": \"{SECRET}\"}}}}");
    app.edit(writing(&document, &mut seen)).expect("edit");
    gone(seen);
    assert!(
        app.stdout
            .starts_with("saved config/credentials/development.json.enc (key ")
    );
    assert_eq!(app.plaintext(), format!("{document}\n"));
    assert!(
        !String::from_utf8(app.file())
            .expect("utf8")
            .contains(SECRET)
    );
}

#[test]
fn a_failed_editor_leaves_the_original_intact_and_removes_the_temporary_file() {
    let mut app = initialized();
    let before = app.file();
    let mut seen = None;
    let error = app
        .edit(Box::new(|_, path, _| {
            fs::write(path, format!("{{\"half\": \"{SECRET}\"}}")).expect("write");
            seen = Some(path.to_path_buf());
            Err("exit status: 1".to_owned())
        }))
        .expect_err("editor failed");
    assert!(
        error
            .to_string()
            .contains("editor failed; credentials are unchanged")
    );
    assert_eq!(app.file(), before);
    gone(seen);
}

#[test]
fn invalid_documents_are_refused_without_echoing_them() {
    for bad in [
        format!("{{\"a\": \"{SECRET}\",, }}"),
        format!("{{\"a\": \"{SECRET}\", \"a\": \"x\"}}"),
        format!("[\"{SECRET}\"]"),
    ] {
        let mut app = initialized();
        let before = app.file();
        let mut seen = None;
        let error = app.edit(writing(&bad, &mut seen)).expect_err("invalid");
        let message = error.to_string();
        assert!(
            message.contains("credentials are unchanged (your edits were discarded)"),
            "{message}"
        );
        assert!(!message.contains(SECRET), "{message}");
        assert_eq!(app.file(), before);
        gone(seen);
    }
}

#[test]
fn an_unchanged_document_is_not_rewritten() {
    let mut app = initialized();
    let before = app.file();
    app.edit(Box::new(|_, _, _| Ok(()))).expect("edit");
    assert_eq!(app.stdout, "no changes\n");
    assert_eq!(app.file(), before);
}

#[test]
fn termination_during_or_outside_the_editor_cleans_up() {
    // SIGTERM while the editor runs.
    let mut app = initialized();
    let before = app.file();
    let mut seen = None;
    let interrupts = Interrupts::none();
    let error = app
        .run_with(
            Action::Edit,
            None,
            Box::new(|_, path, signals| {
                fs::write(path, "{\"changed\": true}").expect("write");
                seen = Some(path.to_path_buf());
                signals.raise_terminate();
                Ok(())
            }),
            &interrupts,
            None,
        )
        .expect_err("terminated");
    assert!(
        error.to_string().contains("interrupted by SIGTERM"),
        "{error}"
    );
    assert_eq!(app.file(), before);
    gone(seen);

    // SIGTERM before the editor starts: the editor never runs.
    let interrupts = Interrupts::none();
    interrupts.raise_terminate();
    let mut ran = false;
    let error = app
        .run_with(
            Action::Edit,
            None,
            Box::new(|_, _, _| {
                ran = true;
                Ok(())
            }),
            &interrupts,
            None,
        )
        .expect_err("terminated");
    assert!(error.to_string().contains("SIGTERM"));
    assert!(!ran);

    // Ctrl-C typed in the editor belongs to the editor.
    let interrupts = Interrupts::none();
    app.run_with(
        Action::Edit,
        None,
        Box::new(|_, path, signals| {
            signals.raise_interrupt();
            fs::write(path, "{\"kept\": 1}").map_err(|error| error.to_string())
        }),
        &interrupts,
        None,
    )
    .expect("saved");
    assert_eq!(app.plaintext(), "{\"kept\": 1}\n");
}

#[test]
fn a_file_changed_while_editing_is_not_overwritten() {
    let mut app = initialized();
    let dir = app.dir();
    let (keys, _) = load_keys(&dir, "development", &|_| None).expect("keys");
    let theirs = seal("development", b"{\"theirs\": true}", &keys[0]).expect("seal");
    let concurrent = theirs.clone();
    let error = app
        .edit(Box::new(move |_, path, _| {
            write_file(&dir, "development", &concurrent, true).expect("concurrent write");
            fs::write(path, "{\"mine\": true}").map_err(|error| error.to_string())
        }))
        .expect_err("conflict");
    assert!(error.to_string().contains("changed while you were editing"));
    assert_eq!(app.file(), theirs);
}

#[test]
fn edit_needs_an_editor_a_key_and_a_file() {
    let mut app = App::new();
    app.env.clear();
    assert!(
        app.run(Action::Edit)
            .expect_err("no editor")
            .to_string()
            .contains("$VISUAL or $EDITOR")
    );
    app.env
        .insert("EDITOR".to_owned(), "fake-editor".to_owned());
    let error = app.run(Action::Edit).expect_err("no key").to_string();
    assert!(
        error.contains("forge credentials init --environment development"),
        "{error}"
    );
    app.run(Action::Init).expect("init");
    fs::remove_file(app.dir().join("development.json.enc")).expect("remove");
    let error = app.run(Action::Edit).expect_err("no file").to_string();
    assert!(error.contains("development.json.enc not found"), "{error}");
}

#[test]
fn show_redacts_by_default_and_reveal_needs_a_terminal() {
    let mut app = initialized();
    let mut seen = None;
    app.edit(writing(
        &format!("{{\"stripe\": {{\"api_key\": \"{SECRET}\"}}, \"n\": [1, 2]}}"),
        &mut seen,
    ))
    .expect("edit");
    app.run(Action::Show { reveal: false }).expect("show");
    assert_eq!(
        app.stdout,
        "{\n  \"n\": [\n    \"[REDACTED]\",\n    \"[REDACTED]\"\n  ],\n  \"stripe\": {\n    \"api_key\": \"[REDACTED]\"\n  }\n}\n"
    );
    let error = app
        .run(Action::Show { reveal: true })
        .expect_err("not a terminal");
    assert!(error.to_string().contains("needs an interactive terminal"));
    assert!(app.stdout.is_empty());
    app.tty = true;
    app.run(Action::Show { reveal: true }).expect("reveal");
    assert!(app.stdout.contains(SECRET));
}

#[test]
fn show_fails_closed_with_a_wrong_or_exposed_key() {
    let mut app = initialized();
    let other =
        forge_encryption::encode_key(&forge_encryption::generate_key().expect("key")).to_string();
    app.env.insert("FORGE_MASTER_KEY".to_owned(), other);
    let error = app
        .run(Action::Show { reveal: false })
        .expect_err("wrong key");
    assert!(
        error.to_string().contains("no available key matches"),
        "{error}"
    );
    app.env.remove("FORGE_MASTER_KEY");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        fs::set_permissions(
            app.dir().join("development.key"),
            fs::Permissions::from_mode(0o644),
        )
        .expect("chmod");
        let error = app
            .run(Action::Show { reveal: false })
            .expect_err("exposed key");
        assert!(error.to_string().contains("chmod 600"), "{error}");
    }
}

fn rotate(keep_old: bool, print_key: bool, write_key_file: bool) -> Action {
    Action::RotateKey {
        keep_old,
        print_key,
        write_key_file,
    }
}

#[test]
fn rotate_key_replaces_the_key_file_and_keeps_the_content() {
    let mut app = initialized();
    let content = app.plaintext();
    let old_key = fs::read_to_string(app.dir().join("development.key")).expect("key");
    let old_file = app.file();
    app.run(rotate(false, false, false)).expect("rotate");
    assert!(
        app.stderr
            .contains("the previous key is no longer accepted")
    );
    assert!(app.stdout.is_empty(), "nothing printed without --print-key");
    let new_key = fs::read_to_string(app.dir().join("development.key")).expect("key");
    assert_ne!(new_key, old_key);
    assert_eq!(new_key.lines().count(), 1);
    assert_eq!(app.plaintext(), content);
    let old = parse_keys(&old_key).expect("old");
    assert!(
        unseal("development", &app.file(), &old).is_err(),
        "the old key no longer opens it"
    );
    assert!(unseal("development", &old_file, &old).is_ok());
}

#[test]
fn rotate_key_keep_old_reads_old_data_then_rewrites_it() {
    let mut app = initialized();
    let content = app.plaintext();
    let old_key = fs::read_to_string(app.dir().join("development.key")).expect("key");
    let old_file = app.file();
    app.run(rotate(true, false, false)).expect("rotate");
    assert!(
        app.stderr
            .contains("the previous key list stays readable (1 key(s))")
    );
    let (keys, _) = load_keys(&app.dir(), "development", &|_| None).expect("keys");
    assert_eq!(keys.len(), 2);
    assert_eq!(keys[1].id(), parse_keys(&old_key).expect("old")[0].id());
    // Deployments still holding the old file keep working with the new list...
    assert_eq!(
        unseal("development", &old_file, &keys)
            .expect("old file")
            .to_vec(),
        content.as_bytes()
    );
    // ...and the file itself was rewritten under the new active key.
    assert_eq!(
        forge_credentials::key_id(&app.file()).expect("id"),
        keys[0].id()
    );
    assert_eq!(app.plaintext(), content);
}

/// A key list already at `MAX_KEYS`, the key that sealed the file last.
fn full_key_list(app: &App) -> Vec<forge_encryption::Key> {
    let (current, _) = load_keys(&app.dir(), "development", &|_| None).expect("keys");
    let mut keys: Vec<_> = (1..forge_credentials::MAX_KEYS)
        .map(|_| {
            forge_credentials::new_key(forge_encryption::generate_key().expect("random"))
                .expect("key")
        })
        .collect();
    keys.extend(current);
    assert_eq!(keys.len(), forge_credentials::MAX_KEYS);
    keys
}

#[test]
fn rotate_key_with_a_full_key_list_rotates_and_keep_old_fails_cleanly() {
    let mut app = initialized();
    let content = app.plaintext();
    let keys = full_key_list(&app);
    forge_credentials::write_key_file(&app.dir(), "development", &keys, true).expect("full list");
    let key_file = app.dir().join("development.key");
    let (key_before, file_before) = (fs::read_to_string(&key_file).expect("key"), app.file());

    // --keep-old would need MAX_KEYS + 1 keys: refused before any output or
    // write, with or without --print-key.
    for print_key in [false, true] {
        let error = app
            .run(rotate(true, print_key, false))
            .expect_err("too many keys");
        assert!(
            error.to_string().contains("would keep 9 keys")
                && error.to_string().contains("at most 8")
                && error.to_string().contains("nothing was changed"),
            "{error}"
        );
        assert!(app.stdout.is_empty(), "nothing printed");
        assert_eq!(fs::read_to_string(&key_file).expect("key"), key_before);
        assert_eq!(app.file(), file_before);
    }

    // Without --keep-old the full list rotates to the new key alone.
    app.run(rotate(false, true, false)).expect("rotate");
    let printed = parse_keys(app.stdout.trim()).expect("printed key list");
    assert_eq!(printed.len(), 1);
    let (stored, _) = load_keys(&app.dir(), "development", &|_| None).expect("keys");
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].id(), printed[0].id());
    assert_eq!(app.plaintext(), content);
    assert!(unseal("development", &app.file(), &keys).is_err());
}

#[test]
fn rotate_key_keep_old_with_a_full_environment_list_prints_nothing() {
    let mut app = initialized();
    let keys = full_key_list(&app);
    let list = keys
        .iter()
        .map(|key| forge_encryption::encode_key(key.material()).to_string())
        .collect::<Vec<_>>()
        .join(",");
    fs::remove_file(app.dir().join("development.key")).expect("remove");
    app.env.insert("FORGE_MASTER_KEY".to_owned(), list);
    let before = app.file();
    let error = app
        .run(rotate(true, true, false))
        .expect_err("too many keys");
    assert!(error.to_string().contains("at most 8"), "{error}");
    assert!(app.stdout.is_empty(), "nothing printed");
    assert!(!app.dir().join("development.key").exists());
    assert_eq!(app.file(), before);
}

/// Records, after each file `rotate-key` writes, whether the credentials
/// file on disk opens with the key file on disk.
fn readable_at_every_step(app: &mut App) -> std::rc::Rc<std::cell::RefCell<Vec<(String, bool)>>> {
    let steps = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let (dir, record) = (app.dir(), std::rc::Rc::clone(&steps));
    app.after_write = Some(Box::new(move |step: &str| {
        let readable = load_keys(&dir, "development", &|_| None).is_ok_and(|(keys, _)| {
            read_file(&dir, "development")
                .is_ok_and(|file| unseal("development", &file, &keys).is_ok())
        });
        record.borrow_mut().push((step.to_owned(), readable));
    }));
    steps
}

#[test]
fn rotate_key_keeps_the_pair_on_disk_readable_at_every_step() {
    for keep_old in [false, true] {
        let mut app = initialized();
        // The file is sealed with the last key of the list, not the first.
        let mut keys = full_key_list(&app);
        if keep_old {
            keys.drain(..keys.len() - 3);
        }
        forge_credentials::write_key_file(&app.dir(), "development", &keys, true).expect("list");
        let steps = readable_at_every_step(&mut app);
        app.run(rotate(keep_old, false, false)).expect("rotate");
        let mut expected = vec![
            ("key file with the new key".to_owned(), true),
            ("credentials file".to_owned(), true),
        ];
        if !keep_old {
            expected.push(("key file without the old keys".to_owned(), true));
        }
        assert_eq!(*steps.borrow(), expected, "keep_old: {keep_old}");
    }
}

#[cfg(unix)]
#[test]
fn rotate_key_refuses_a_writable_directory_before_showing_a_key() {
    use std::os::unix::fs::PermissionsExt as _;

    let mut app = initialized();
    let key_text = fs::read_to_string(app.dir().join("development.key")).expect("key");
    fs::remove_file(app.dir().join("development.key")).expect("remove");
    app.env
        .insert("FORGE_MASTER_KEY".to_owned(), key_text.trim().to_owned());
    let before = app.file();
    fs::set_permissions(app.dir(), fs::Permissions::from_mode(0o775)).expect("chmod");
    let error = app
        .run(rotate(false, true, true))
        .expect_err("writable directory");
    fs::set_permissions(app.dir(), fs::Permissions::from_mode(0o700)).expect("chmod");
    assert!(error.to_string().contains("chmod 700"), "{error}");
    assert!(app.stdout.is_empty(), "no key was printed: {}", app.stdout);
    assert!(!app.dir().join("development.key").exists());
    assert_eq!(app.file(), before);
}

#[test]
fn a_failed_rotation_removes_a_key_file_that_did_not_exist() {
    let mut app = initialized();
    let key_text = fs::read_to_string(app.dir().join("development.key")).expect("key");
    fs::remove_file(app.dir().join("development.key")).expect("remove");
    app.env
        .insert("FORGE_MASTER_KEY".to_owned(), key_text.trim().to_owned());
    let before = app.file();
    // Once the key file is written, the credentials file cannot be replaced:
    // a non-empty directory takes its name.
    let encrypted = app.dir().join("development.json.enc");
    app.after_write = Some(Box::new(move |step: &str| {
        if step == "key file with the new key" {
            fs::remove_file(&encrypted).expect("remove");
            fs::create_dir(&encrypted).expect("directory");
            fs::write(encrypted.join("blocker"), b"x").expect("blocker");
        }
    }));
    app.run(rotate(false, false, true))
        .expect_err("the credentials file cannot be written");
    assert!(
        !app.dir().join("development.key").exists(),
        "the restore must not leave the new key list behind"
    );
    let encrypted = app.dir().join("development.json.enc");
    fs::remove_dir_all(&encrypted).expect("cleanup");
    fs::write(&encrypted, &before).expect("restore");
}

#[test]
fn rotate_key_from_the_environment_needs_a_destination_for_the_new_key() {
    let mut app = initialized();
    let key_text = fs::read_to_string(app.dir().join("development.key")).expect("key");
    fs::remove_file(app.dir().join("development.key")).expect("remove");
    app.env
        .insert("FORGE_MASTER_KEY".to_owned(), key_text.trim().to_owned());
    let before = app.file();
    let error = app
        .run(rotate(false, false, false))
        .expect_err("no destination");
    assert!(error.to_string().contains("pass --print-key"));
    assert_eq!(app.file(), before, "nothing was re-encrypted");
    app.run(rotate(false, true, false)).expect("print");
    let printed = parse_keys(app.stdout.trim()).expect("printed key list");
    assert_eq!(printed.len(), 1);
    assert!(!app.dir().join("development.key").exists());
    assert!(unseal("development", &app.file(), &printed).is_ok());
}

#[test]
fn rotate_key_refuses_when_the_file_changes_and_leaves_the_key_file_alone() {
    let mut app = initialized();
    let key_before = fs::read_to_string(app.dir().join("development.key")).expect("key");
    let dir = app.dir();
    let change = move || {
        let (keys, _) = load_keys(&dir, "development", &|_| None).expect("keys");
        let other = seal("development", b"{\"other\": 1}", &keys[0]).expect("seal");
        write_file(&dir, "development", &other, true).expect("write");
    };
    let error = app
        .run_with(
            rotate(false, true, false),
            None,
            Box::new(|_, _, _| Ok(())),
            &Interrupts::none(),
            Some(Box::new(change)),
        )
        .expect_err("changed");
    assert!(error.to_string().contains("changed during rotation"));
    assert!(app.stdout.is_empty(), "no key was printed");
    assert_eq!(
        fs::read_to_string(app.dir().join("development.key")).expect("key"),
        key_before
    );
}

#[test]
fn environments_are_validated_and_default_to_forge_env() {
    let mut app = App::new();
    let error = app
        .run_with(
            Action::Init,
            Some("../prod"),
            Box::new(|_, _, _| Ok(())),
            &Interrupts::none(),
            None,
        )
        .expect_err("invalid");
    assert!(error.to_string().contains("invalid environment name"));
    app.env.insert("FORGE_ENV".to_owned(), "staging".to_owned());
    app.run(Action::Init).expect("init");
    assert!(app.dir().join("staging.json.enc").exists());
}
