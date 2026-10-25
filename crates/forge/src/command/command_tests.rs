//! The command context beyond option parsing: output, the secret sources'
//! failures, the testing constructors and the database accessors.

use std::io::{self, Read, Write};

use super::*;

/// A writer that always fails.
struct Broken;

impl Write for Broken {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        Err(io::Error::other("closed pipe"))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Read for Broken {
    fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
        Err(io::Error::other("unreadable"))
    }
}

impl BufRead for Broken {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        Err(io::Error::other("unreadable"))
    }

    fn consume(&mut self, _: usize) {}
}

fn context() -> CommandContext {
    CommandContext::for_testing(&["--tenant", "acme"])
}

#[test]
fn the_testing_context_has_no_database_input_or_environment() {
    let mut context = context();
    assert_eq!(context.app_name(), "test");
    assert_eq!(context.arguments(), ["--tenant", "acme"]);
    assert!(context.config().database.url.is_none());
    assert_eq!(
        context
            .options(&["--tenant"], &[])
            .expect("options")
            .value("--tenant"),
        Some("acme")
    );
    let debug = format!("{context:?}");
    assert!(
        debug.contains("arguments: 2") && debug.contains("database: None"),
        "{debug}"
    );
    // Output is discarded, and without input, a variable or a terminal
    // there is no secret to read.
    context.say("hello").expect("sink");
    let error = context
        .read_secret("FORGE_SETUP_PASSWORD", false, "Password")
        .expect_err("no source");
    assert!(error.is_usage(), "{error}");
}

#[test]
fn stdin_and_the_environment_can_be_supplied_to_a_testing_context() {
    let mut piped = context().with_stdin("hunter2-hunter2\nnext line\n");
    assert_eq!(
        piped
            .read_secret("FORGE_SETUP_PASSWORD", true, "Password")
            .expect("one line"),
        "hunter2-hunter2"
    );
    let mut environment = context()
        .with_env("FORGE_SETUP_PASSWORD", "from-env")
        .with_env("FORGE_OTHER", "other");
    assert_eq!(
        environment
            .read_secret("FORGE_SETUP_PASSWORD", false, "Password")
            .expect("variable"),
        "from-env"
    );
    assert_eq!(
        environment
            .read_secret("FORGE_OTHER", false, "Password")
            .expect("the earlier variable stays"),
        "other"
    );
    // Both given is ambiguous, whichever variable is set.
    assert!(
        environment
            .read_secret("FORGE_OTHER", true, "Password")
            .expect_err("ambiguous")
            .is_usage()
    );
}

#[test]
fn a_failing_output_input_or_prompt_is_a_failure_not_a_usage_error() {
    let mut context = context();
    context.output = Box::new(Broken);
    let error = context.say("hello").expect_err("closed pipe");
    assert!(!error.is_usage());
    assert_eq!(error.to_string(), "cannot write output: closed pipe");

    let mut context = self::context();
    context.input = Box::new(Broken);
    let error = context
        .read_secret("FORGE_SETUP_PASSWORD", true, "Password")
        .expect_err("unreadable");
    assert!(!error.is_usage());
    assert_eq!(error.to_string(), "cannot read standard input: unreadable");

    // The hidden prompt fails on the first or the confirmation entry.
    for failing_from in [0_usize, 1] {
        let mut context = self::context();
        context.terminal = true;
        let mut calls = 0_usize;
        context.hidden_prompt = Box::new(move |_| {
            calls += 1;
            if calls > failing_from {
                Err(io::Error::from(io::ErrorKind::Interrupted))
            } else {
                Ok("first".to_owned())
            }
        });
        let error = context
            .read_secret("FORGE_SETUP_PASSWORD", false, "Password")
            .expect_err("interrupted");
        assert!(!error.is_usage(), "{error}");
        assert!(
            error.to_string().starts_with("cannot read the password"),
            "{error}"
        );
    }
}

#[test]
fn a_prompt_names_what_is_being_asked_and_asks_twice() {
    let mut context = context();
    context.terminal = true;
    let prompts = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = std::sync::Arc::clone(&prompts);
    context.hidden_prompt = Box::new(move |prompt| {
        seen.lock().expect("lock").push(prompt.to_owned());
        Ok("same".to_owned())
    });
    assert_eq!(
        context
            .read_secret("FORGE_SETUP_PASSWORD", false, "Owner password")
            .expect("matching entries"),
        "same"
    );
    assert_eq!(
        *prompts.lock().expect("lock"),
        ["Owner password: ", "Owner password (again): "]
    );
}

#[test]
fn the_process_context_reads_the_process_environment() {
    let mut context = CommandContext::for_process(
        "demo",
        "setup",
        vec!["--tenant".to_owned()],
        AppConfig::default(),
        None,
    );
    assert_eq!(context.app_name(), "demo");
    assert_eq!(context.arguments(), ["--tenant"]);
    // `PATH` is set wherever tests run; it stands for any variable.
    let path = std::env::var("PATH").expect("PATH is set");
    assert_eq!(
        context
            .read_secret("PATH", false, "Password")
            .expect("from the environment"),
        path
    );
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn only_the_attached_backend_is_available() {
    let database =
        forge_db::SqliteDatabase::connect(&forge_config::SecretString::new("sqlite::memory:"))
            .await
            .expect("memory database");
    let context = context().with_sqlite(database.clone());

    context
        .sqlite()
        .expect("attached")
        .ping()
        .await
        .expect("ping");
    assert!(format!("{context:?}").contains("database: Some"));
    #[cfg(feature = "postgres")]
    {
        let error = context.postgres().expect_err("not attached");
        assert!(error.to_string().contains("App::postgres"), "{error}");
    }
    #[cfg(feature = "mysql")]
    {
        let error = context.mysql().expect_err("not attached");
        assert!(error.to_string().contains("App::mysql"), "{error}");
    }
    database.close().await;
}

#[cfg(feature = "postgres")]
#[tokio::test]
async fn a_postgres_pool_can_be_attached() {
    // A lazy pool never connects: only the attachment is checked.
    let url = forge_config::SecretString::new("postgres://app:secret@127.0.0.1:1/app");
    let database = forge_db::PgDatabase::connect_lazy(&url).expect("lazy pool");
    let context = context().with_postgres(database);

    assert!(context.postgres().is_ok());
    assert!(format!("{context:?}").contains("database: Some"));
    #[cfg(feature = "sqlite")]
    assert!(context.sqlite().is_err());
}

#[cfg(feature = "mysql")]
#[tokio::test]
async fn a_mysql_pool_can_be_attached() {
    let url = forge_config::SecretString::new("mysql://app:secret@127.0.0.1:1/app");
    let database = forge_db::MySqlDatabase::connect_lazy(&url).expect("lazy pool");
    let context = context().with_mysql(database);

    assert!(context.mysql().is_ok());
    assert!(format!("{context:?}").contains("database: Some"));
    #[cfg(feature = "postgres")]
    assert!(context.postgres().is_err());
}
