//! The configuration reader's error paths (YAML subset, type errors and
//! validation messages), the secrets file, error rendering, `forge deploy
//! init` failures and the commands that depend on optional settings.

use std::path::Path;

use super::commands::Commands;
use super::config::{Config, DEFAULT_RETAIN, go_io_error};
use super::errors::{DeployError, RemoteError};
use super::init::init;
use super::secrets::Secrets;
use super::tests::{VALID_CONFIG, parse};
use super::yaml::{self, Tag, Value};

/// The smallest valid configuration: no registry, one proxied role.
const MINIMAL: &str = "\
service: shop
image: registry.example.com/acme/shop
servers:
  web:
    hosts: [192.0.2.10]
proxy:
  image: registry.example.com/forge-proxy
";

fn error_of(text: &str) -> String {
    match Config::parse(text.as_bytes()) {
        Ok(_) => panic!("configuration should be refused:\n{text}"),
        Err(error) => error,
    }
}

fn minimal_with(old: &str, new: &str) -> String {
    assert!(MINIMAL.contains(old), "fixture lacks {old:?}");
    MINIMAL.replacen(old, new, 1)
}

#[test]
fn the_minimal_configuration_takes_every_default() {
    let config = parse(MINIMAL);
    assert_eq!(config.retain, DEFAULT_RETAIN);
    assert_eq!(config.registry.username, "");
    assert_eq!(config.ssh.port, 0);
    assert_eq!(config.parallelism, 4);
    assert_eq!(config.migrate.role, "web");
    // `retain: 0` and `parallelism: 0` mean "unset" and take the defaults.
    let explicit = parse(&format!("{MINIMAL}retain: 0\nparallelism: 0\n"));
    assert_eq!(explicit.retain, DEFAULT_RETAIN);
    assert_eq!(explicit.parallelism, 4);
}

#[test]
fn null_sections_are_empty_and_scalars_keep_their_text() {
    // Null values decode as the empty value of their type.
    let config = parse(&format!(
        "{MINIMAL}hooks:\nenv:\nvolumes:\naccessories:\nssh:\nbuilder:\nmigrate:\n"
    ));
    assert_eq!(config.hooks, ".forge/hooks");
    assert!(config.accessories.is_empty());
    let config = parse(&minimal_with(
        "    hosts: [192.0.2.10]\n",
        "    hosts: [192.0.2.10]\n    proxy:\n    env:\n      clear:\n      secret:\n    cmd:\n",
    ));
    assert!(config.proxied("web"), "a null proxy keeps the web default");
    let config = parse(&format!("{MINIMAL}stop_timeout:\nboot_timeout:\n"));
    assert_eq!(config.stop_timeout.whole_seconds(), 35);
    // Quoted booleans and integers are text where text is expected.
    let config = parse(&format!(
        "{MINIMAL}accessories:\n  db:\n    image: postgres:18\n    host: 192.0.2.20\n    port: \"5432\"\n    cmd: [\"-c\", \"fsync=on\"]\n    env:\n      secret: [DB_PASSWORD]\n"
    ));
    assert_eq!(config.accessories["db"].port, "5432");
    assert_eq!(config.accessories["db"].cmd, ["-c", "fsync=on"]);
}

#[test]
fn yaml_one_one_booleans_are_accepted_for_boolean_fields() {
    for (text, want) in [
        ("yes", true),
        ("Yes", true),
        ("on", true),
        ("y", true),
        ("no", false),
        ("Off", false),
        ("n", false),
        ("False", false),
    ] {
        let config = if want {
            parse(&minimal_with(
                "    hosts: [192.0.2.10]\n",
                &format!("    hosts: [192.0.2.10]\n    proxy: {text}\n"),
            ))
        } else {
            // An unproxied role cannot be combined with a proxy section.
            parse(&format!(
                "service: shop\nimage: registry.example.com/acme/shop\nservers:\n  web:\n    hosts: [192.0.2.10]\n    proxy: {text}\n"
            ))
        };
        assert_eq!(config.proxied("web"), want, "proxy: {text}");
    }
}

#[test]
fn type_errors_name_the_line_the_yaml_type_and_the_target() {
    let cases: Vec<(String, &str)> = vec![
        (
            format!("{MINIMAL}retain: [1]\n"),
            "cannot unmarshal !!seq into int",
        ),
        (
            format!("{MINIMAL}retain: {{a: 1}}\n"),
            "cannot unmarshal !!map into int",
        ),
        (
            format!("{MINIMAL}retain: true\n"),
            "cannot unmarshal !!bool `true` into int",
        ),
        (
            format!("{MINIMAL}retain: seven\n"),
            "cannot unmarshal !!str `seven` into int",
        ),
        (
            format!("{MINIMAL}retain: \"a-rather-long-text\"\n"),
            "cannot unmarshal !!str `a-rathe...` into int",
        ),
        (
            format!("{MINIMAL}retain: 1e30\n"),
            "cannot unmarshal !!float `1e30` into int",
        ),
        (
            format!("{MINIMAL}retain: .inf\n"),
            "cannot unmarshal !!float `.inf` into int",
        ),
        (
            format!("{MINIMAL}hooks: [a]\n"),
            "cannot unmarshal !!seq into string",
        ),
        (
            format!("{MINIMAL}volumes: x\n"),
            "cannot unmarshal !!str `x` into []string",
        ),
        (
            format!("{MINIMAL}env:\n  clear: [a]\n"),
            "cannot unmarshal !!seq into map[string]string",
        ),
        (
            format!("{MINIMAL}builder: x\n"),
            "cannot unmarshal !!str `x` into deploy.Builder",
        ),
        (
            minimal_with(
                "servers:\n  web:\n    hosts: [192.0.2.10]\n",
                "servers: x\n",
            ),
            "into map[string]deploy.Role",
        ),
        (
            format!("{MINIMAL}accessories: x\n"),
            "into map[string]deploy.Accessory",
        ),
        (
            format!("{MINIMAL}ssh:\n  colour: blue\n"),
            "field colour not found in type deploy.SSH",
        ),
        (
            format!("{MINIMAL}registry:\n  colour: blue\n"),
            "field colour not found in type deploy.Registry",
        ),
        (
            format!("{MINIMAL}env:\n  colour: blue\n"),
            "field colour not found in type deploy.Env",
        ),
        (
            format!("{MINIMAL}migrate:\n  colour: blue\n"),
            "field colour not found in type deploy.Migrate",
        ),
        (
            format!("{MINIMAL}builder:\n  colour: blue\n"),
            "field colour not found in type deploy.Builder",
        ),
        (
            format!("{MINIMAL}accessories:\n  db:\n    colour: blue\n"),
            "field colour not found in type deploy.Accessory",
        ),
        (
            minimal_with(
                "    hosts: [192.0.2.10]\n",
                "    hosts: [192.0.2.10]\n    colour: blue\n",
            ),
            "field colour not found in type deploy.Role",
        ),
        (
            minimal_with(
                "    hosts: [192.0.2.10]\n",
                "    hosts: [192.0.2.10]\n    proxy: maybe\n",
            ),
            "cannot unmarshal !!str `maybe` into bool",
        ),
        (
            minimal_with(
                "    hosts: [192.0.2.10]\n",
                "    hosts: [192.0.2.10]\n    proxy: [x]\n",
            ),
            "cannot unmarshal !!seq into bool",
        ),
        (
            minimal_with(
                "    hosts: [192.0.2.10]\n",
                "    hosts: [192.0.2.10]\n    volumes: [\"data:/data\"]\n    cmd: []\n",
            ),
            "",
        ),
        (format!("{MINIMAL}proxy:\n  image: x\n"), "already defined"),
    ];
    for (text, want) in cases {
        if want.is_empty() {
            // A valid combination: role volumes and an empty cmd.
            Config::parse(text.as_bytes()).expect("valid");
            continue;
        }
        let error = match Config::parse(text.as_bytes()) {
            Ok(_) => "accepted".to_owned(),
            Err(error) => error,
        };
        assert!(error.contains(want), "want {want:?} in {error} for\n{text}");
    }
}

#[test]
fn durations_must_be_strings_that_parse() {
    for (text, want) in [
        (
            "{MINIMAL}stop_timeout: 30\n",
            "duration must be a string such as \"30s\"",
        ),
        (
            "{MINIMAL}stop_timeout: [30]\n",
            "duration must be a string such as \"30s\"",
        ),
        ("{MINIMAL}stop_timeout: soon\n", "invalid duration \"soon\""),
    ] {
        let error = error_of(&text.replace("{MINIMAL}", MINIMAL));
        assert!(error.contains(want), "want {want:?} in {error}");
    }
    // A quoted number is a string, so it is read as a duration (and a bare
    // number has no unit).
    let error = error_of(&format!("{MINIMAL}stop_timeout: \"30\"\n"));
    assert!(error.contains("invalid duration \"30\""), "{error}");
}

#[test]
fn validation_names_every_problem_it_finds() {
    let cases: Vec<(String, &str)> = vec![
        (
            minimal_with(
                "image: registry.example.com/acme/shop\n",
                "image: registry.example.com/acme/shop\nregistry:\n  server: \"not a host\"\n",
            ),
            "registry.server must be a host name",
        ),
        (
            minimal_with(
                "image: registry.example.com/acme/shop\n",
                "image: registry.example.com/acme/shop\nregistry:\n  username: \"ac me\"\n  password: REGISTRY_PASSWORD\n",
            ),
            "registry.username contains whitespace",
        ),
        (
            minimal_with("servers:\n  web:\n    hosts: [192.0.2.10]\n", "servers:\n"),
            "servers must define at least one role",
        ),
        (
            minimal_with("  web:", "  Bad_Role:"),
            "servers.Bad_Role: role name must match",
        ),
        (
            minimal_with("[192.0.2.10]", "[192.0.2.10, 192.0.2.10]"),
            "is listed twice",
        ),
        (
            format!(
                "{}env:\n  secret: [SHARED]\n",
                minimal_with(
                    "    hosts: [192.0.2.10]\n",
                    "    hosts: [192.0.2.10]\n    env:\n      clear: {SHARED: x}\n"
                )
            ),
            "servers.web.env.clear.SHARED is also a secret in env.secret",
        ),
        (
            format!(
                "{}env:\n  clear: {{SHARED: x}}\n",
                minimal_with(
                    "    hosts: [192.0.2.10]\n",
                    "    hosts: [192.0.2.10]\n    env:\n      secret: [SHARED]\n"
                )
            ),
            "servers.web.env.secret SHARED is also in env.clear",
        ),
        (
            format!("{MINIMAL}env:\n  secret: [\"bad name\"]\n"),
            "env.secret: \"bad name\" is not a valid secret name",
        ),
        (
            format!(
                "{MINIMAL}accessories:\n  Bad_Name:\n    image: postgres:18\n    host: 192.0.2.20\n"
            ),
            "accessories.Bad_Name: name must match",
        ),
        (
            format!("{MINIMAL}builder:\n  context: -x\n"),
            "builder.context and builder.dockerfile must not start with '-'",
        ),
        (
            format!("{MINIMAL}ssh:\n  port: 70000\n"),
            "ssh.port must be between 1 and 65535",
        ),
        (
            minimal_with(
                "  image: registry.example.com/forge-proxy\n",
                "  image: registry.example.com/forge-proxy\n  hosts: [a.example.com, A.example.com]\n",
            ),
            "proxy.hosts: \"a.example.com\" is listed twice",
        ),
        (
            minimal_with(
                "  image: registry.example.com/forge-proxy\n",
                "  image: registry.example.com/forge-proxy\n  acme_email: nobody\n",
            ),
            "proxy.acme_email is not an e-mail address",
        ),
        (
            minimal_with(
                "  image: registry.example.com/forge-proxy\n",
                "  image: registry.example.com/forge-proxy\n  app_port: 70000\n",
            ),
            "proxy.app_port must be between 1 and 65535",
        ),
        (
            minimal_with(
                "  image: registry.example.com/forge-proxy\n",
                "  image: registry.example.com/forge-proxy\n  healthcheck:\n    healthy_count: 500\n",
            ),
            "proxy.healthcheck.healthy_count must be between 1 and 100",
        ),
        (
            minimal_with(
                "  image: registry.example.com/forge-proxy\n",
                "  image: registry.example.com/forge-proxy\n  max_body_bytes: -1\n",
            ),
            "proxy.max_body_bytes must be positive",
        ),
        (
            minimal_with(
                "    hosts: [192.0.2.10]\n",
                "    hosts: [192.0.2.10]\n    proxy: false\n",
            ),
            "",
        ),
    ];
    for (text, want) in cases {
        if want.is_empty() {
            // A proxy section without a proxied role is refused.
            let error = error_of(&text);
            assert!(error.contains("no role uses it"), "{error}");
            continue;
        }
        let error = error_of(&text);
        assert!(error.contains(want), "want {want:?} in {error}");
    }
}

#[test]
fn a_role_that_is_not_called_web_is_not_proxied_unless_asked() {
    let config = parse(
        "service: shop\nimage: registry.example.com/acme/shop\nservers:\n  api:\n    hosts: [192.0.2.10]\n  jobs:\n    hosts: [192.0.2.11]\n",
    );
    // Neither role is `web` and none sets `proxy: true`: the first role by
    // name is the primary one.
    assert_eq!(config.primary_role(), "api");
    assert!(!config.proxied("api"));
}

#[test]
fn an_oversized_configuration_file_is_refused_before_it_is_parsed() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("deploy.yml");
    let mut text = String::from(MINIMAL);
    text.push_str("# ");
    text.push_str(&"x".repeat(1024 * 1024));
    std::fs::write(&path, text).expect("write");
    let error = Config::load(&path).expect_err("too large");
    assert!(error.contains("is larger than 1048576 bytes"), "{error}");
    // A directory cannot be read as a file.
    let error = Config::load(directory.path()).expect_err("directory");
    assert!(error.starts_with("read deploy config: open "), "{error}");
}

#[test]
fn io_errors_read_like_go_path_errors() {
    let path = Path::new("config/deploy.yml");
    let denied = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
    assert_eq!(
        go_io_error(path, &denied),
        "open config/deploy.yml: permission denied"
    );
    let other = std::io::Error::other("disk on fire");
    assert_eq!(
        go_io_error(path, &other),
        "open config/deploy.yml: disk on fire"
    );
}

// --- the YAML subset -------------------------------------------------------

#[test]
fn yaml_scalars_resolve_like_yaml_v3() {
    assert_eq!(yaml::resolve("1e+5"), Tag::Float);
    assert_eq!(yaml::resolve("-1.5E-3"), Tag::Float);
    assert_eq!(yaml::resolve(".5"), Tag::Float);
    assert_eq!(yaml::resolve("1e"), Tag::Str, "an exponent needs digits");
    assert_eq!(yaml::resolve("1e+"), Tag::Str);
    assert_eq!(yaml::resolve("."), Tag::Str);
    assert_eq!(yaml::resolve("0x1F"), Tag::Int);
    assert_eq!(yaml::resolve("0o17"), Tag::Int);
    assert_eq!(yaml::resolve("0b101"), Tag::Int);
    assert_eq!(yaml::resolve("017"), Tag::Int);
    assert_eq!(yaml::resolve("1_000"), Tag::Int);
    assert_eq!(yaml::resolve("-"), Tag::Str);
    assert_eq!(yaml::resolve("+"), Tag::Str);
    assert_eq!(yaml::parse_go_int("-"), None);
    assert_eq!(yaml::parse_go_int("+"), None);
    assert_eq!(yaml::parse_go_int("0x"), None);
    assert_eq!(yaml::parse_go_int("-0x10"), Some(-16));
    assert_eq!(yaml::parse_go_int("99999999999999999999"), None);
}

#[test]
fn yaml_quoted_scalars_decode_their_escapes() {
    let node = yaml::parse(
        "key: \"\\0\\a\\b\\t\\n\\v\\f\\r\\e\\ \\\"\\/\\\\\\N\\_\\L\\P\\x41\\u00e9\\U0001F600\"\n",
    )
    .expect("parse")
    .expect("a document");
    let Value::Map(entries) = node.value else {
        panic!("a mapping");
    };
    let Value::Scalar { text, plain } = &entries[0].1.value else {
        panic!("a scalar");
    };
    assert!(!plain);
    assert_eq!(
        text,
        "\0\u{7}\u{8}\t\n\u{b}\u{c}\r\u{1b} \"/\\\u{85}\u{a0}\u{2028}\u{2029}A\u{e9}\u{1F600}"
    );
    // Single quotes double themselves and interpret nothing.
    let node = yaml::parse("key: 'it''s \\n'\n")
        .expect("parse")
        .expect("a document");
    let Value::Map(entries) = node.value else {
        panic!("a mapping");
    };
    assert_eq!(
        entries[0].1.value,
        Value::Scalar {
            text: "it's \\n".to_owned(),
            plain: false
        }
    );
}

#[test]
fn yaml_refuses_what_it_does_not_read_with_a_line_number() {
    let mut failures = Vec::new();
    for (text, want) in [
        (
            "--- text\nkey: 1\n",
            "yaml: line 1: content after --- is not supported",
        ),
        ("%YAML 1.2\nkey: 1\n", "directives are not supported"),
        ("key: 1\n...\nkey: 2\n", "exactly one YAML document"),
        ("key: 'open\n", "found unexpected end of stream"),
        ("key: \"open\n", "found unexpected end of stream"),
        ("key: \"open\\", "found unexpected end of stream"),
        (
            "key: \"\\xZZ\"\n",
            "did not find expected hexdecimal number",
        ),
        (
            "key: \"\\UFFFFFFFF\"\n",
            "found invalid Unicode character escape code",
        ),
        ("key: \"\\q\"\n", "found unknown escape character"),
        (
            "key: \"a\" b\n",
            "did not find expected key after a quoted scalar",
        ),
        (
            "\"a\"b: 1\n",
            "did not find expected key after a quoted scalar",
        ),
        (
            "\"a\":b\n",
            "did not find expected key after a quoted scalar",
        ),
        ("key: 1\nplain\n", "could not find expected ':'"),
        ("key: 1\n- item\n", "did not find expected key"),
        ("- a\nkey: 1\n", "did not find expected key"),
        ("  key: 1\nother: 2\n", "did not find expected key"),
        (
            "scalar\n  more\n",
            "multi-line plain scalars are not supported",
        ),
        (
            "key: 1\n  more\n",
            "multi-line plain scalars are not supported",
        ),
        ("- a: 1\n  - b\n", "did not find expected"),
        (
            "- one\n  two\n",
            "multi-line plain scalars are not supported",
        ),
        ("key: [a, b\n", "did not find expected"),
        ("key: [[a] b]\n", "did not find expected ',' or ']'"),
        ("key: {a: 1 b: 2}\n", "did not find expected ',' or '}'"),
        ("key: {[a]: 1}\n", "flow mapping keys must be scalars"),
        (
            "key: [a] extra\n",
            "did not find expected <document end> after flow collection",
        ),
        ("key: &anchor x\n", "anchors are not supported"),
        ("key: *alias\n", "aliases are not supported"),
        ("key: !!str x\n", "tags are not supported"),
        ("key: |\n  text\n", "block scalars"),
        ("key: ? x\n", "complex keys"),
        ("key: @x\n", "cannot start any token"),
        ("\tkey: 1\n", "tab character"),
        (
            "    a: 1\n  b: 2\n",
            "did not find expected key (bad indentation)",
        ),
        ("key: [\n", "unclosed flow collection"),
    ] {
        let error = match yaml::parse(text) {
            Ok(_) => "accepted".to_owned(),
            Err(error) => error.to_string(),
        };
        if !error.contains(want) {
            failures.push(format!("{text:?}: error = {error}, want {want:?}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn yaml_reads_empty_items_flow_pairs_and_empty_flow_values() {
    let node = yaml::parse("items:\n  -\n  - a\n  -\n    k: v\n")
        .expect("parse")
        .expect("a document");
    let Value::Map(entries) = node.value else {
        panic!("a mapping");
    };
    let Value::Seq(items) = &entries[0].1.value else {
        panic!("a sequence");
    };
    assert_eq!(items.len(), 3);
    assert_eq!(items[0].value, Value::Null);
    assert!(matches!(items[2].value, Value::Map(_)));
    // `[a: b]` is a one-pair mapping, `{a: }` has an empty value, and a
    // trailing comma closes a flow mapping.
    let node = yaml::parse("pair: [a: b]\nempty: {a: , b: 1,}\n")
        .expect("parse")
        .expect("a document");
    let Value::Map(entries) = node.value else {
        panic!("a mapping");
    };
    let Value::Seq(pair) = &entries[0].1.value else {
        panic!("a sequence");
    };
    assert!(matches!(pair[0].value, Value::Map(_)));
    let Value::Map(flow) = &entries[1].1.value else {
        panic!("a flow mapping");
    };
    assert_eq!(flow[0].1.value, Value::Null);
    assert_eq!(flow.len(), 2);
    // A document can be a single scalar.
    let node = yaml::parse("hello\n").expect("parse").expect("a document");
    assert_eq!(
        node.value,
        Value::Scalar {
            text: "hello".to_owned(),
            plain: true
        }
    );
    // A comment-only document is nothing.
    assert_eq!(yaml::parse("# nothing\n\n").expect("parse"), None);
}

// --- secrets ----------------------------------------------------------------

#[test]
fn the_secrets_file_reports_what_it_cannot_read_without_values() {
    let directory = tempfile::tempdir().expect("tempdir");
    let lookup = |_: &str| None;
    // A path below a regular file is neither found nor readable.
    let file = directory.path().join("file");
    std::fs::write(&file, "x").expect("file");
    let error = Secrets::load(&file.join("secrets"), &lookup).expect_err("not a directory");
    assert!(error.starts_with("secrets file: open "), "{error}");
    // A directory with owner-only permissions passes the mode check and
    // fails when read.
    let folder = directory.path().join("folder");
    {
        use std::os::unix::fs::DirBuilderExt as _;
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&folder)
            .expect("folder");
    }
    let error = Secrets::load(&folder, &lookup).expect_err("a directory");
    assert!(error.starts_with("secrets file: open "), "{error}");
}

#[test]
fn secrets_and_redactors_never_show_values() {
    let secrets = Secrets::from_values([("API_TOKEN", "super-secret-value"), ("SHORT", "abc")]);
    let debug = format!("{secrets:?}");
    assert!(debug.contains("API_TOKEN") && !debug.contains("super-secret-value"));
    let redactor = secrets.redactor();
    // Values under four characters are not credentials and are not redacted.
    assert_eq!(format!("{redactor:?}"), "Redactor(1 values)");
    assert_eq!(
        redactor.redact("token super-secret-value abc"),
        "token [REDACTED] abc"
    );
}

// --- errors -----------------------------------------------------------------

#[test]
fn remote_errors_are_cut_at_a_character_boundary() {
    let long = "é".repeat(1500);
    let error = RemoteError {
        host: "192.0.2.10".to_owned(),
        exit_code: 2,
        stderr: format!("  {long}  "),
        stdout: String::new(),
    };
    let text = error.to_string();
    assert!(text.starts_with("192.0.2.10: remote command exited with status 2: é"));
    assert!(text.ends_with("..."), "{text}");
    // 3000 bytes cut at 2000 would split a two-byte character.
    assert!(text.len() < 2100, "{}", text.len());
    let silent = RemoteError {
        host: "h".to_owned(),
        exit_code: 9,
        stderr: " \n".to_owned(),
        stdout: String::new(),
    };
    assert_eq!(silent.to_string(), "h: remote command exited with status 9");
    let converted = DeployError::from("plain message".to_owned());
    assert_eq!(converted.message, "plain message");
    assert_eq!(converted.exit_code(), 1);
    assert_eq!(DeployError::from(silent).remote_exit, Some(9));
}

// --- init -------------------------------------------------------------------

#[test]
fn init_reports_an_ignore_file_it_cannot_read_and_leaves_matching_ones_alone() {
    let root = tempfile::tempdir().expect("tempdir");
    // `.gitignore` is a directory: reading it fails with something other
    // than "not found".
    std::fs::create_dir(root.path().join(".gitignore")).expect("directory");
    let (changed, result) = init(root.path(), "shop");
    let error = result.expect_err("unreadable ignore file");
    assert!(error.starts_with("read "), "{error}");
    assert_eq!(changed, ["config/deploy.yml", ".forge/secrets"]);

    // An ignore file that already lists the entry (also in the `/entry` and
    // `entry/` forms) is not touched.
    let root = tempfile::tempdir().expect("tempdir");
    std::fs::write(root.path().join(".gitignore"), "/.forge/secrets\n").expect("gitignore");
    std::fs::write(root.path().join(".dockerignore"), ".forge/\n").expect("dockerignore");
    let (changed, result) = init(root.path(), "shop");
    result.expect("init");
    assert_eq!(changed, ["config/deploy.yml", ".forge/secrets"]);
    assert_eq!(
        std::fs::read_to_string(root.path().join(".gitignore")).expect("read"),
        "/.forge/secrets\n"
    );
}

// --- commands ---------------------------------------------------------------

#[test]
fn proxy_maintenance_and_acme_settings_reach_the_commands() {
    let commands = Commands::new(parse(VALID_CONFIG));
    assert_eq!(
        commands.proxy_maintenance(true).script,
        "docker exec forge-proxy /usr/local/bin/forge-proxy maintenance -service shop on"
    );
    assert!(commands.proxy_maintenance(false).script.ends_with(" off"));

    let with_email = parse(&VALID_CONFIG.replacen(
        "  ssl: true\n",
        "  ssl: true\n  acme_email: ops@example.com\n",
        1,
    ));
    let boot = Commands::new(with_email).boot_proxy().script;
    assert!(boot.contains("run -acme-email ops@example.com"), "{boot}");

    // The interactive forms attach a terminal.
    let argv = vec!["bin/console".to_owned()];
    assert!(
        commands
            .exec_one_off("web", &argv, true)
            .contains("--interactive --tty")
    );
    assert!(
        commands
            .exec_reuse("web", &argv, true)
            .contains("docker exec --interactive --tty")
    );
    assert!(
        !commands
            .exec_reuse("web", &argv, false)
            .contains("--interactive")
    );
}

#[test]
fn a_secret_named_by_role_and_globally_is_listed_once() {
    let config = parse(
        &VALID_CONFIG.replacen(
            "    env:\n      clear:\n        WORKERS: \"4\"\n",
            "    env:\n      clear:\n        WORKERS: \"4\"\n      secret: [FORGE_DATABASE_URL, JOBS_ONLY]\n",
            1,
        ),
    );
    let (_, secrets) = Commands::new(config).role_env("jobs");
    assert_eq!(secrets, ["FORGE_DATABASE_URL", "JOBS_ONLY"]);
}
