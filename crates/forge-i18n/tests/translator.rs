//! Forge-Go's `i18n` tests, ported: the same catalog, the same expectations.

use std::io;
use std::sync::{Arc, Mutex};

use forge_i18n::{
    Args, Category, DateTime, LoadError, Operands, Options, Translator, plural_rule_for,
};

const EN: &str = r#"{"en": {
    "greeting": "Hello, %{name}!",
    "percent": "100%% sure, %{name}",
    "users": {"welcome": "Welcome back", "intro_html": "<strong>%{name}</strong> joined"},
    "inbox": {"zero": "No messages", "one": "%{count} message", "other": "%{count} messages"},
    "only_en": "English only",
    "number": {"format": {"separator": ".", "delimiter": ",", "precision": 2}},
    "date": {"formats": {"long": "%B %-d, %Y"}}
}}"#;

const PT: &str = r#"{"pt": {
    "greeting": "Olá, %{name}!",
    "inbox": {"one": "%{count} mensagem", "other": "%{count} mensagens"},
    "only_pt": "Só em português",
    "number": {"format": {"separator": ",", "delimiter": ".", "precision": 2}},
    "date": {
        "formats": {"long": "%-d de %B de %Y"},
        "month_names": [null, "janeiro", "fevereiro", "março", "abril", "maio", "junho", "julho", "agosto", "setembro", "outubro", "novembro", "dezembro"],
        "day_names": ["domingo", "segunda-feira", "terça-feira", "quarta-feira", "quinta-feira", "sexta-feira", "sábado"]
    },
    "time": {"formats": {"short": "%d/%m %H:%M"}}
}}"#;

const PT_BR: &str = r#"{"pt-BR": {"greeting": "Oi, %{name}!"}}"#;

fn files() -> Vec<(&'static str, &'static str)> {
    vec![
        ("en.json", EN),
        ("pt.json", PT),
        ("pt-BR.json", PT_BR),
        ("README.md", "ignored"),
    ]
}

fn load(options: Options) -> Translator {
    match Translator::from_files(&files(), options) {
        Ok(translator) => translator,
        Err(error) => panic!("catalog: {error}"),
    }
}

/// Collects the `tracing` output of a closure.
fn logs_of(run: impl FnOnce()) -> String {
    #[derive(Clone, Default)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);
    impl io::Write for Buffer {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .map_err(|_| io::Error::other("poisoned"))?
                .extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let buffer = Buffer::default();
    let writer = buffer.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .without_time()
        .finish();
    tracing::subscriber::with_default(subscriber, run);
    let bytes = buffer
        .0
        .lock()
        .map(|bytes| bytes.clone())
        .unwrap_or_default();
    String::from_utf8_lossy(&bytes).into_owned()
}

#[test]
fn load_merges_scoped_files_and_lists_keys() -> Result<(), LoadError> {
    let mut all = files();
    all.push((
        "posts.en.json",
        r#"{"en": {"posts": {"index": {"title": "Posts"}}}}"#,
    ));
    let translator = Translator::from_files(&all, Options::new())?;
    assert_eq!(translator.locales(), ["en", "pt", "pt-BR"]);
    assert_eq!(translator.default_locale(), "en");
    let keys = translator.keys("en");
    for want in [
        "posts.index.title",
        "inbox",
        "users.intro_html",
        "date.formats.long",
    ] {
        assert!(keys.iter().any(|key| key == want), "{want}: {keys:?}");
    }
    assert!(!keys.iter().any(|key| key == "inbox.one"));
    assert!(translator.has("en", "posts.index.title"));
    assert!(!translator.has("pt", "only_en"));
    assert!(!translator.has("en", "users"));
    assert_eq!(
        translator.plural_forms("pt", "inbox"),
        Some(vec![Category::One, Category::Other])
    );
    assert_eq!(translator.plural_forms("pt", "greeting"), None);
    Ok(())
}

#[test]
fn invalid_options_are_refused() {
    assert!(Translator::from_files(&files(), Options::new().default_locale("../../etc")).is_err());
    let missing = Translator::from_files(&[("pt.json", r#"{"pt":{}}"#)], Options::new());
    assert!(missing.err().is_some_and(|error| {
        error
            .to_string()
            .contains("default locale en has no translation file")
    }),);
    let rule: forge_i18n::PluralRule = |_| Category::Other;
    assert!(Translator::from_files(&files(), Options::new().plural_rule("c/y", rule)).is_err());
}

#[test]
fn translate_interpolates_and_falls_back_through_parents() {
    let translator = load(Options::new().show_missing(true));
    let name = || Args::new().with("name", "Ana");
    let cases: Vec<(&str, &str, Args<'_>, &str)> = vec![
        ("en", "greeting", name(), "Hello, Ana!"),
        ("pt-BR", "greeting", name(), "Oi, Ana!"),
        // pt-BR -> pt
        ("pt-BR", "only_pt", Args::new(), "Só em português"),
        ("pt-PT", "greeting", name(), "Olá, Ana!"),
        ("PT-br", "greeting", name(), "Oi, Ana!"),
        // Unknown locale -> default.
        ("fr", "greeting", name(), "Hello, Ana!"),
        ("../x", "greeting", name(), "Hello, Ana!"),
        ("en", "percent", name(), "100% sure, Ana"),
        // A missing value stays visible.
        ("en", "greeting", Args::new(), "Hello, %{name}!"),
        ("en", "inbox", Args::new().count(0), "No messages"),
        ("en", "inbox", Args::new().count(1), "1 message"),
        ("en", "inbox", Args::new().count(2), "2 messages"),
        ("en", "inbox", Args::new().count(-1), "-1 message"),
        ("en", "inbox", Args::new().count(1.5), "1.5 messages"),
        ("en", "inbox", Args::new().with("count", 2), "2 messages"),
        ("en", "inbox", Args::new(), "%{count} messages"),
        // CLDR pt: 0 and 1 are singular.
        ("pt", "inbox", Args::new().count(0), "0 mensagem"),
        (
            "pt",
            "inbox",
            Args::new().count(1_000_000_i64),
            "1000000 mensagens",
        ),
        (
            "pt-BR",
            "only_en",
            Args::new(),
            "translation missing: pt-BR.only_en",
        ),
        ("en", "users", Args::new(), "translation missing: en.users"),
        (
            "en",
            "users.welcome.deeper",
            Args::new(),
            "translation missing: en.users.welcome.deeper",
        ),
        (
            "en",
            "bad key!",
            Args::new(),
            "translation missing: en.bad key!",
        ),
    ];
    for (locale, key, args, want) in cases {
        assert_eq!(
            translator.translate(locale, key, &args),
            want,
            "{locale} {key} {args:?}"
        );
    }
}

#[test]
fn missing_translations_in_production_fall_back_and_log_once() {
    let translator = load(Options::new());
    let logs = logs_of(|| {
        assert_eq!(
            translator.translate("pt-BR", "only_en", &Args::new()),
            "English only"
        );
        assert_eq!(
            translator.translate("pt-BR", "users.welcome_back_html", &Args::new()),
            "Welcome back"
        );
        assert_eq!(
            translator.translate("pt", "inbox", &Args::new().count(5)),
            "5 mensagens"
        );
        for _ in 0..3 {
            let _ = translator.translate("pt-BR", "nowhere.at_all", &Args::new());
        }
    });
    assert_eq!(logs.matches("nowhere.at_all").count(), 1, "{logs}");
    assert!(logs.contains("translation missing"), "{logs}");
    assert!(logs.contains("locale=\"pt-BR\""), "{logs}");
}

/// Distinct missing keys from request input cannot grow the log or the
/// memory forever; the first one past the bound says so once, and the rest
/// are counted.
#[test]
fn missing_translation_warnings_are_bounded_and_the_cap_is_announced() {
    let translator = load(Options::new());
    let below = logs_of(|| {
        for index in 0..1024 {
            let _ = translator.translate("en", &format!("flood.k{index}"), &Args::new());
        }
    });
    assert_eq!(below.lines().count(), 1024);
    assert_eq!(translator.suppressed_warnings(), 0);
    let above = logs_of(|| {
        for index in 1024..3000 {
            let _ = translator.translate("en", &format!("flood.k{index}"), &Args::new());
        }
        // A key that was logged stays quiet and is not counted again.
        let _ = translator.translate("en", "flood.k0", &Args::new());
    });
    assert_eq!(above.lines().count(), 1, "{above}");
    assert_eq!(
        above
            .matches("missing-translation warnings suppressed")
            .count(),
        1
    );
    assert_eq!(translator.suppressed_warnings(), 3000 - 1024);
}

#[test]
fn html_escapes_interpolation_outside_trusted_keys() {
    let translator = load(Options::new().show_missing(true));
    let attack = r#"<script>alert(1)</script>"'&"#;
    let plain = translator.translate_html("en", "greeting", &Args::new().with("name", attack));
    assert!(!plain.contains("<script>"), "{plain}");
    assert!(plain.contains("&lt;script&gt;"), "{plain}");
    assert_eq!(
        translator.translate_html("en", "users.intro_html", &Args::new().with("name", attack)),
        "<strong>&lt;script&gt;alert(1)&lt;/script&gt;&#34;&#39;&amp;</strong> joined"
    );
    assert_eq!(
        translator.translate_html(
            "en",
            "users.intro_html",
            &Args::new().with_markup("name", "<em>Ana</em>")
        ),
        "<strong><em>Ana</em></strong> joined"
    );
    // A missing _html key is never trusted markup.
    let missing = translator.translate_html(
        "en",
        "users.<img src=x onerror=alert(1)>_html",
        &Args::new(),
    );
    assert!(!missing.contains("<img"), "{missing}");
    // Plain-text translate leaves non-HTML text unescaped for the caller.
    assert_eq!(
        translator.translate("en", "greeting", &Args::new().with("name", "<b>")),
        "Hello, <b>!"
    );
}

#[test]
fn malformed_arguments_are_reported_not_fatal() {
    let translator = load(Options::new());
    let logs = logs_of(|| {
        assert_eq!(
            translator.translate("en", "inbox", &Args::new().with("count", "many")),
            "many messages"
        );
        assert_eq!(
            translator.translate(
                "en",
                "greeting",
                &Args::new().with("3x", "x").with("name", "Ana")
            ),
            "Hello, Ana!"
        );
    });
    assert!(logs.contains("not a number"), "{logs}");
    assert!(logs.contains("is not a name"), "{logs}");
}

#[test]
fn format_number() {
    let translator = load(Options::new());
    let cases: Vec<(&str, forge_i18n::Number, Option<u32>, &str)> = vec![
        ("en", 0.into_n(), None, "0"),
        ("en", 999.into_n(), None, "999"),
        ("en", 1000.into_n(), None, "1,000"),
        ("en", (-1_234_567).into_n(), None, "-1,234,567"),
        ("en", u64::MAX.into_n(), None, "18,446,744,073,709,551,615"),
        ("en", 1234.5.into_n(), None, "1,234.50"),
        // Exact binary ties round half to even.
        ("en", 1234.5.into_n(), Some(0), "1,234"),
        ("en", 1235.5.into_n(), Some(0), "1,236"),
        ("en", 12.into_n(), Some(2), "12.00"),
        ("pt-BR", 1234.5.into_n(), Some(1), "1.234,5"),
        ("pt-BR", (-0.5).into_n(), None, "-0,50"),
        ("pt-BR", 1_234_567.891.into_n(), None, "1.234.567,89"),
        // The default locale's format.
        ("ja", 1234.into_n(), None, "1,234"),
        ("en", f64::NAN.into_n(), None, "NaN"),
    ];
    for (locale, value, precision, want) in cases {
        assert_eq!(
            translator.format_number(locale, value, precision),
            want,
            "{locale} {value:?} {precision:?}"
        );
    }
    let reference = &&42_u8;
    assert_eq!(translator.format_number("en", reference, None), "42");
}

trait IntoN {
    fn into_n(self) -> forge_i18n::Number;
}

impl<T: forge_i18n::IntoNumber> IntoN for T {
    fn into_n(self) -> forge_i18n::Number {
        self.into_number()
    }
}

#[test]
fn format_time() -> Result<(), LoadError> {
    let translator = load(Options::new());
    // 2026-03-05 14:07:09 at UTC-3 ("BRT").
    let when = DateTime::from_utc(2026, 3, 5, 17, 7, 9)
        .and_then(|utc| utc.with_offset(-3 * 3600, Some("BRT")))
        .expect("a valid date");
    for (locale, format, want) in [
        ("en", "", "Thu, 05 Mar 2026 14:07:09 -0300"),
        ("en", "short", "05 Mar 14:07"),
        ("en", "date.formats.long", "March 5, 2026"),
        ("pt-BR", "date.formats.long", "5 de março de 2026"),
        ("pt-BR", "short", "05/03 14:07"),
        // Falls back to time.formats.default.
        ("pt-BR", "unknown", "Thu, 05 Mar 2026 14:07:09 -0300"),
    ] {
        assert_eq!(
            translator.format_time(locale, &when, format),
            want,
            "{locale} {format}"
        );
    }
    let custom = Translator::from_files(
        &[(
            "en.json",
            r#"{"en":{"time":{"formats":{"x":"%I:%M %p|%-I%P|%e|%j|%y|%A|%Z|%%|%q|%"}}}}"#,
        )],
        Options::new(),
    )?;
    assert_eq!(
        custom.format_time("en", &when, "x"),
        "02:07 PM|2pm| 5|064|26|Thursday|BRT|%|%q|%"
    );
    assert_eq!(
        custom.format_time("en", &DateTime::from_unix(0), "%Z"),
        "Thu, 01 Jan 1970 00:00:00 +0000"
    );
    Ok(())
}

#[test]
fn locale_matching() {
    let translator = load(Options::new());
    for (requested, want) in [
        ("pt-BR", "pt-BR"),
        ("pt-PT", "pt"),
        ("PT", "pt"),
        ("en-GB", "en"),
        ("fr", "en"),
        ("../../etc", "en"),
    ] {
        assert_eq!(translator.match_locale(requested), want, "{requested}");
    }
    assert_eq!(translator.exact_locale("pt-br"), Some("pt-BR"));
    assert_eq!(translator.exact_locale("pt-PT"), None);
    for (header, want) in [
        ("pt-BR,en;q=0.5", Some("pt-BR")),
        ("pt-PT", Some("pt")),
        ("en;q=0.4, pt;q=0.9", Some("pt")),
        ("fr, de;q=0.9, pt-BR;q=0.1", Some("pt-BR")),
        ("fr, *;q=0.5", Some("en")),
        ("pt;q=0, fr", None),
        (";;;,,,=", None),
        ("en-US-x-private", Some("en")),
    ] {
        assert_eq!(translator.match_accept_language(header), want, "{header}");
    }
    // A language variant: pt-BR only, asked for pt.
    let variants = Translator::from_files(
        &[
            ("en.json", r#"{"en":{}}"#),
            ("pt-BR.json", r#"{"pt-BR":{}}"#),
        ],
        Options::new(),
    );
    assert_eq!(
        variants
            .ok()
            .map(|t| t.match_locale("pt").to_owned())
            .as_deref(),
        Some("pt-BR")
    );
}

#[test]
fn custom_plural_rules() -> Result<(), LoadError> {
    let files = [
        ("en.json", r#"{"en":{}}"#),
        (
            "cy.json",
            r#"{"cy":{"cats":{"two":"dwy gath","other":"%{count} cath"}}}"#,
        ),
    ];
    let rule: forge_i18n::PluralRule = |o: Operands| {
        if o.v == 0 && o.i == 2 {
            Category::Two
        } else {
            Category::Other
        }
    };
    let translator = Translator::from_files(&files, Options::new().plural_rule("CY", rule))?;
    assert_eq!(
        translator.translate("cy", "cats", &Args::new().count(2)),
        "dwy gath"
    );
    assert_eq!(
        translator.plural_categories("cy"),
        [Category::Two, Category::Other]
    );
    assert_eq!(
        plural_rule_for("cy")(Operands::from_integer(2)),
        Category::Other
    );
    Ok(())
}

#[test]
fn directories_load_like_embedded_files() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    for (name, content) in files() {
        std::fs::write(dir.path().join(name), content)?;
    }
    let translator = Translator::load_dir(dir.path(), Options::new())?;
    assert_eq!(translator.locales(), ["en", "pt", "pt-BR"]);
    Ok(())
}
