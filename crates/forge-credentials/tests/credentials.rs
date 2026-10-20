//! Behaviour of the credentials files, keys and store, including the
//! negative paths: wrong key, tampering, truncation, environment swaps,
//! exposed key files and secret-free errors.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::Path;

use forge_credentials::{
    CredentialsError, KeySource, Options, file_name, init, key_file_name, load_keys, new_key,
    parse_keys, read_file, seal, unseal, write_file, write_key_file,
};
use forge_encryption::{encode_key, generate_key};

const SECRET: &str = "sk_live_DISTINCTIVE_4242";

fn no_env(_: &str) -> Option<String> {
    None
}

fn document() -> Vec<u8> {
    format!("{{\n  \"stripe\": {{\n    \"api_key\": \"{SECRET}\"\n  }},\n  \"empty\": \"\",\n  \"port\": 5432\n}}\n")
        .into_bytes()
}

#[test]
fn sealed_files_open_only_with_their_key_and_environment() {
    let first = new_key(generate_key().unwrap()).unwrap();
    let second = new_key(generate_key().unwrap()).unwrap();
    let file = seal("production", &document(), &first).unwrap();
    let text = String::from_utf8(file.clone()).unwrap();
    assert!(!text.contains(SECRET));
    assert!(text.contains(&format!("\"key_id\": \"{}\"", first.id())));

    assert_eq!(
        &*unseal("production", &file, std::slice::from_ref(&first)).unwrap(),
        document().as_slice()
    );
    // Previous keys in the list are accepted; selection is by key id.
    assert!(unseal("production", &file, &[second.clone(), first.clone()]).is_ok());
    // Wrong key.
    assert!(matches!(
        unseal("production", &file, std::slice::from_ref(&second)),
        Err(CredentialsError::NoMatchingKey(_))
    ));
    // A key that claims the right id but has other material.
    let impostor = forge_encryption::Key::new(first.id(), generate_key().unwrap()).unwrap();
    assert!(matches!(
        unseal("production", &file, &[impostor]),
        Err(CredentialsError::Decrypt)
    ));
    // Another environment, even with the same key.
    assert!(matches!(
        unseal("staging", &file, std::slice::from_ref(&first)),
        Err(CredentialsError::Decrypt)
    ));
}

#[test]
fn tampered_truncated_and_foreign_files_are_refused() {
    let key = new_key(generate_key().unwrap()).unwrap();
    let file = seal("production", &document(), &key).unwrap();
    let mut envelope: serde_json::Value = serde_json::from_slice(&file).unwrap();
    let keys = [key.clone()];
    let reseal = |value: &serde_json::Value| serde_json::to_vec(value).unwrap();

    // Every ciphertext character flipped.
    let ciphertext = envelope["ciphertext"].as_str().unwrap().to_owned();
    for index in (0..ciphertext.len()).step_by(7) {
        let mut bytes = ciphertext.clone().into_bytes();
        bytes[index] = if bytes[index] == b'A' { b'B' } else { b'A' };
        let mut tampered = envelope.clone();
        tampered["ciphertext"] = String::from_utf8(bytes).unwrap().into();
        assert!(matches!(
            unseal("production", &reseal(&tampered), &keys),
            Err(CredentialsError::Decrypt | CredentialsError::Format(_))
        ));
    }
    // A truncated ciphertext and a truncated file.
    let mut short = envelope.clone();
    short["ciphertext"] = ciphertext[..ciphertext.len() - 8].into();
    assert!(unseal("production", &reseal(&short), &keys).is_err());
    for cut in [1, file.len() / 2, file.len() - 3] {
        assert!(matches!(
            unseal("production", &file[..cut], &keys),
            Err(CredentialsError::Format(_))
        ));
    }
    // Downgrade or relabel: version, algorithm, nonce and unknown fields.
    envelope["version"] = 2.into();
    let error = unseal("production", &reseal(&envelope), &keys).unwrap_err();
    assert!(
        error.to_string().contains("unsupported version 2"),
        "{error}"
    );
    envelope["version"] = 1.into();
    let mut other = envelope.clone();
    other["algorithm"] = "AES-128-GCM".into();
    assert!(matches!(
        unseal("production", &reseal(&other), &keys),
        Err(CredentialsError::Format(_))
    ));
    let mut other = envelope.clone();
    other["nonce"] = "AAAA".into();
    assert!(matches!(
        unseal("production", &reseal(&other), &keys),
        Err(CredentialsError::Format(_))
    ));
    let mut other = envelope.clone();
    other["extra"] = true.into();
    assert!(matches!(
        unseal("production", &reseal(&other), &keys),
        Err(CredentialsError::Format(_))
    ));
    let mut trailing = file.clone();
    trailing.extend_from_slice(b"{}");
    assert!(matches!(
        unseal("production", &trailing, &keys),
        Err(CredentialsError::Format(_))
    ));
    assert!(matches!(
        unseal("production", b"not json", &keys),
        Err(CredentialsError::Format(_))
    ));
    assert!(matches!(
        unseal("Production", &file, &keys),
        Err(CredentialsError::InvalidEnvironment)
    ));
}

#[test]
fn documents_are_validated_before_sealing() {
    let key = new_key(generate_key().unwrap()).unwrap();
    let duplicate = format!("{{\"a\": \"{SECRET}\", \"a\": \"x\"}}");
    let error = seal("production", duplicate.as_bytes(), &key).unwrap_err();
    assert!(
        error.to_string().contains("duplicate key at line 1"),
        "{error}"
    );
    assert!(!error.to_string().contains(SECRET));
    assert!(seal("production", b"[]", &key).is_err());
    for bad in ["", "Prod", "1prod", "a/b", "../x", &"a".repeat(33)] {
        assert!(
            matches!(
                seal(bad, b"{}", &key),
                Err(CredentialsError::InvalidEnvironment)
            ),
            "{bad}"
        );
    }
}

#[test]
fn key_lists_parse_like_forge_go_and_never_echo_input() {
    let first = encode_key(&generate_key().unwrap()).to_string();
    let second = encode_key(&generate_key().unwrap()).to_string();
    let keys = parse_keys(&format!("# comment\n\n {first} , {second}\n")).unwrap();
    assert_eq!(keys.len(), 2);
    assert_eq!(keys[0].id().len(), 16);
    assert_ne!(keys[0].id(), keys[1].id());
    let error = parse_keys(&format!("{first},{SECRET}"))
        .unwrap_err()
        .to_string();
    assert!(
        error.starts_with("credentials: key 2: encryption: invalid key"),
        "{error}"
    );
    assert!(!error.contains(SECRET));
    assert!(parse_keys("# only a comment\n").is_err());
    let nine = [first.as_str(); 9].join(",");
    assert!(
        parse_keys(&nine)
            .unwrap_err()
            .to_string()
            .contains("at most 8")
    );
}

#[cfg(unix)]
fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt as _;

    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn init_creates_a_private_key_and_a_skeleton_and_never_overwrites() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("config/credentials");
    let created = init(&dir, "production").unwrap();
    assert_eq!(
        created,
        [dir.join("production.key"), dir.join("production.json.enc")]
    );
    #[cfg(unix)]
    {
        assert_eq!(mode(&dir.join("production.key")), 0o600);
        assert_eq!(mode(&dir), 0o700);
    }
    let (keys, source) = load_keys(&dir, "production", &no_env).unwrap();
    assert_eq!(source, KeySource::File(dir.join("production.key")));
    let store = forge_credentials::open(
        "production",
        Options::default().with_dir(&dir).with_lookup(no_env),
    )
    .unwrap();
    assert_eq!(store.key_ring(&["encryption"]).unwrap().active_id(), "k1");
    let key_text = fs::read_to_string(dir.join("production.key")).unwrap();

    // A second init refuses: the file exists, and the key is untouched.
    let error = init(&dir, "production").unwrap_err();
    assert!(
        matches!(error.source, CredentialsError::Exists(_)),
        "{error}"
    );
    assert!(error.created.is_empty());
    assert_eq!(
        fs::read_to_string(dir.join("production.key")).unwrap(),
        key_text
    );

    // A restored key starts a new file without being replaced.
    fs::remove_file(dir.join("production.json.enc")).unwrap();
    let created = init(&dir, "production").unwrap();
    assert_eq!(created, [dir.join("production.json.enc")]);
    assert_eq!(
        fs::read_to_string(dir.join("production.key")).unwrap(),
        key_text
    );
    let file = read_file(&dir, "production").unwrap();
    assert!(unseal("production", &file, &keys).is_ok());
    // No temporary files are left behind.
    let names: Vec<String> = fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        names.iter().all(|name| !name.ends_with(".tmp")),
        "{names:?}"
    );
}

#[cfg(unix)]
#[test]
fn exposed_or_linked_key_files_are_refused() {
    use std::os::unix::fs::PermissionsExt as _;

    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("credentials");
    init(&dir, "production").unwrap();
    let key_path = dir.join("production.key");
    for exposed in [0o644, 0o640, 0o604, 0o660] {
        fs::set_permissions(&key_path, fs::Permissions::from_mode(exposed)).unwrap();
        let error = load_keys(&dir, "production", &no_env).unwrap_err();
        assert!(
            matches!(&error, CredentialsError::KeyPermissions { mode, .. } if *mode == exposed),
            "{error}"
        );
        assert!(error.to_string().contains("chmod 600"));
    }
    fs::set_permissions(&key_path, fs::Permissions::from_mode(0o400)).unwrap();
    assert!(load_keys(&dir, "production", &no_env).is_ok());

    // A symbolic link in place of the key file is refused, even to a 0600 file.
    let outside = root.path().join("elsewhere.key");
    fs::copy(&key_path, &outside).unwrap();
    fs::set_permissions(&outside, fs::Permissions::from_mode(0o600)).unwrap();
    fs::remove_file(&key_path).unwrap();
    std::os::unix::fs::symlink(&outside, &key_path).unwrap();
    assert!(load_keys(&dir, "production", &no_env).is_err());
    // A directory is not a key file either.
    fs::remove_file(&key_path).unwrap();
    fs::create_dir(&key_path).unwrap();
    assert!(load_keys(&dir, "production", &no_env).is_err());
}

#[test]
fn the_environment_variable_wins_over_the_key_file() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("credentials");
    let fresh = encode_key(&generate_key().unwrap()).to_string();
    let lookup = |name: &str| (name == "FORGE_MASTER_KEY").then(|| fresh.clone());
    let (keys, source) = load_keys(&dir, "production", &lookup).unwrap();
    assert_eq!(source, KeySource::Environment);
    assert_eq!(source.to_string(), "FORGE_MASTER_KEY");
    assert_eq!(keys.len(), 1);
    let blank = |_: &str| Some("  ".to_owned());
    assert!(matches!(
        load_keys(&dir, "production", &blank),
        Err(CredentialsError::KeyMissing)
    ));
    let broken = |_: &str| Some(SECRET.to_owned());
    let error = load_keys(&dir, "production", &broken)
        .unwrap_err()
        .to_string();
    assert!(
        error.starts_with("FORGE_MASTER_KEY: credentials: key 1"),
        "{error}"
    );
    assert!(!error.contains(SECRET));
}

#[test]
fn writes_are_atomic_and_exclusive_unless_replacing() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("credentials");
    let key = new_key(generate_key().unwrap()).unwrap();
    write_file(&dir, "test", b"one", false).unwrap();
    assert!(matches!(
        write_file(&dir, "test", b"two", false),
        Err(CredentialsError::Exists(_))
    ));
    assert_eq!(read_file(&dir, "test").unwrap(), b"one");
    write_file(&dir, "test", b"two", true).unwrap();
    assert_eq!(read_file(&dir, "test").unwrap(), b"two");
    write_key_file(&dir, "test", std::slice::from_ref(&key), false).unwrap();
    assert!(write_key_file(&dir, "test", std::slice::from_ref(&key), false).is_err());
    assert!(write_key_file(&dir, "test", &[], true).is_err());
    assert!(matches!(
        read_file(&dir, "missing"),
        Err(CredentialsError::NotFound(_))
    ));
    assert_eq!(file_name("test"), "test.json.enc");
    assert_eq!(key_file_name("test"), "test.key");
}

#[test]
fn missing_credentials_fail_closed_outside_development_and_test() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("credentials");
    let options = || Options::default().with_dir(&dir).with_lookup(no_env);
    // No file: empty in development and test, an error elsewhere.
    for lenient in ["development", "test"] {
        let store = forge_credentials::open(lenient, options()).unwrap();
        assert!(!store.exists());
        assert!(!store.has(&["anything"]));
    }
    assert!(matches!(
        forge_credentials::open("production", options()),
        Err(CredentialsError::NotFound(_))
    ));
    // A file without a key is an error everywhere.
    let key = new_key(generate_key().unwrap()).unwrap();
    for environment in ["development", "production"] {
        write_file(
            &dir,
            environment,
            &seal(environment, &document(), &key).unwrap(),
            true,
        )
        .unwrap();
        assert!(matches!(
            forge_credentials::open(environment, options()),
            Err(CredentialsError::KeyMissing)
        ));
    }
    write_key_file(&dir, "development", std::slice::from_ref(&key), true).unwrap();
    write_key_file(&dir, "production", std::slice::from_ref(&key), true).unwrap();
    // Required paths: every absent or empty one is reported, in production only.
    let required = || options().with_required(["stripe.api_key", "empty", "missing.path", "port"]);
    let error = forge_credentials::open("production", required()).unwrap_err();
    assert_eq!(
        error.to_string(),
        "credentials: missing credential in production credentials: empty, missing.path"
    );
    assert!(forge_credentials::open("development", required()).is_ok());
    // An undecryptable file is an error even in development.
    write_file(
        &dir,
        "development",
        &seal("production", &document(), &key).unwrap(),
        true,
    )
    .unwrap();
    assert!(matches!(
        forge_credentials::open("development", options()),
        Err(CredentialsError::Decrypt)
    ));
}

#[test]
fn stores_never_print_their_content() {
    let store = forge_credentials::parse("production", &document()).unwrap();
    for printed in [
        format!("{store:?}"),
        format!("{store}"),
        format!("{store:#?}"),
        serde_json::to_string(&store).unwrap(),
    ] {
        assert!(
            printed.contains("credentials.Store(production, [REDACTED])"),
            "{printed}"
        );
        assert!(!printed.contains(SECRET), "{printed}");
    }
    let secret = store.secret(&["stripe", "api_key"]).unwrap();
    assert_eq!(secret.expose(), SECRET);
    assert_eq!(format!("{secret:?}"), "[REDACTED]");
    // Errors name paths and types, never values.
    let errors = [
        store.string(&["port"]).unwrap_err(),
        store.string(&["stripe", "api_key", "deeper"]).unwrap_err(),
        store.decode::<u32>(&["stripe", "api_key"]).unwrap_err(),
        store.key_ring(&["stripe"]).unwrap_err(),
    ];
    for error in &errors {
        assert!(!error.to_string().contains(SECRET), "{error}");
    }
    assert_eq!(
        errors[0].to_string(),
        "credentials: unexpected credential type: \"port\" is a number, want a string"
    );
    #[derive(serde::Deserialize)]
    struct Stripe {
        api_key: String,
    }
    assert_eq!(store.decode::<Stripe>(&["stripe"]).unwrap().api_key, SECRET);
    assert_eq!(store.decode::<u16>(&["port"]).unwrap(), 5432);
    assert!(store.has(&["stripe", "api_key"]));
    assert_eq!(store.environment(), "production");
}
