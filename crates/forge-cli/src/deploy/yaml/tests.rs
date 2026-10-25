use super::*;

fn scalar(text: &str, plain: bool) -> Value {
    Value::Scalar {
        text: text.to_owned(),
        plain,
    }
}

fn map(node: &Node) -> Vec<(String, Value)> {
    match &node.value {
        Value::Map(entries) => entries
            .iter()
            .map(|(k, v)| (scalar_text(k).cloned().unwrap_or_default(), v.value.clone()))
            .collect(),
        other => panic!("not a map: {other:?}"),
    }
}

fn parse_one(text: &str) -> Node {
    parse(text).expect("valid").expect("document")
}

#[test]
fn block_and_flow_collections() {
    let node = parse_one(
        "# comment\nservice: shop # trailing\nservers:\n  web:\n    hosts: [192.0.2.10, \"192.0.2.11\"]\n  jobs:\n    hosts:\n    - a\n    -  b\n    cmd: ['it''s', \"a\\tb\"]\nenv: {clear: {A: \"1\", B: 2}, secret: []}\nempty:\nlist:\n  - x: 1\n    y: 2\n  - - nested\n",
    );
    let top = map(&node);
    assert_eq!(top[0], ("service".into(), scalar("shop", true)));
    let Value::Map(servers) = &top[1].1 else {
        panic!("servers")
    };
    let web = map(&servers[0].1);
    let Value::Seq(hosts) = &web[0].1 else {
        panic!("hosts")
    };
    assert_eq!(hosts[1].value, scalar("192.0.2.11", false));
    let jobs = map(&servers[1].1);
    let Value::Seq(hosts) = &jobs[0].1 else {
        panic!("compact hosts")
    };
    assert_eq!(hosts.len(), 2);
    let Value::Seq(cmd) = &jobs[1].1 else {
        panic!("cmd")
    };
    assert_eq!(cmd[0].value, scalar("it's", false));
    assert_eq!(cmd[1].value, scalar("a\tb", false));
    let env = &top[2].1;
    let Value::Map(env) = env else { panic!("env") };
    assert_eq!(map(&env[0].1)[1], ("B".into(), scalar("2", true)));
    assert_eq!(top[3].1, Value::Null);
    let Value::Seq(list) = &top[4].1 else {
        panic!("list")
    };
    assert_eq!(map(&list[0]).len(), 2);
    assert!(matches!(&list[1].value, Value::Seq(inner) if inner.len() == 1));
}

#[test]
fn plain_scalars_keep_their_text() {
    let top = map(&parse_one(
        "a: a#b\nb: :x\nc: -x\nd: 1.5\ne: ~\nf: http://x:80/y\ng: [a:b]\nh: {a:b}\n",
    ));
    assert_eq!(top[0].1, scalar("a#b", true));
    assert_eq!(top[1].1, scalar(":x", true));
    assert_eq!(top[2].1, scalar("-x", true));
    assert_eq!(top[3].1, scalar("1.5", true));
    assert_eq!(top[4].1, Value::Null);
    assert_eq!(top[5].1, scalar("http://x:80/y", true));
    assert!(matches!(&top[6].1, Value::Seq(items) if items[0].value == scalar("a:b", true)));
    assert!(matches!(&top[7].1, Value::Map(entries) if entries[0].1.value == Value::Null));
}

#[test]
fn multi_line_flow_collections() {
    let top = map(&parse_one("l: [\n  a,\n  b # c\n]\nm: {x: 1,\n  y: 2}\n"));
    assert!(matches!(&top[0].1, Value::Seq(items) if items.len() == 2));
    assert!(matches!(&top[1].1, Value::Map(entries) if entries.len() == 2));
}

#[test]
fn documents() {
    assert_eq!(parse("").expect("empty"), None);
    assert_eq!(parse("# only\n\n").expect("comments"), None);
    assert!(parse("---\na: 1\n...\n").expect("markers").is_some());
    for text in ["a: 1\n---\nb: 2\n", "---\na: 1\n---\n", "a: 1\n...\nb: 2\n"] {
        let error = parse(text).expect_err("two documents");
        assert!(
            error.message.contains("exactly one YAML document"),
            "{error}"
        );
    }
}

#[test]
fn refuses_deep_nesting_instead_of_exhausting_the_stack() {
    for text in [
        format!("hosts:\n{}x\n", "- ".repeat(200_000)),
        format!("a: {}{}\n", "[".repeat(100_000), "]".repeat(100_000)),
        format!("a: {}1{}\n", "{b: ".repeat(1_000), "}".repeat(1_000)),
    ] {
        let error = parse(&text).expect_err("deep nesting");
        assert!(
            error.to_string().contains("exceeded max depth of 100"),
            "{error}"
        );
    }
    let nested = format!("a: {}1{}\n", "[".repeat(50), "]".repeat(50));
    assert!(parse(&nested).is_ok(), "moderate nesting stays valid");
}

#[test]
fn refuses_what_it_does_not_support() {
    for (text, message) in [
        ("a: &x 1\n", "anchors"),
        ("a: *x\n", "aliases"),
        ("a: !!str 5\n", "tags"),
        ("a: |\n  x\n", "block scalars"),
        ("a: >\n  x\n", "block scalars"),
        ("? a\n: b\n", "complex keys"),
        ("%YAML 1.2\n---\na: 1\n", "directives"),
        ("a: x\n  y\n", "multi-line plain"),
        ("a: \"x\n  y\"\n", "multi-line quoted"),
        ("a: x\n\tb: 1\n", "tab character"),
        ("a: b: c\n", "mapping values are not allowed"),
        ("a: - x\n", "block sequence entries"),
        ("a: @x\n", "cannot start any token"),
        (
            "a: 1\na: 2\n",
            "mapping key \"a\" already defined at line 1",
        ),
        ("m: {a: 1, a: 2}\n", "already defined"),
        ("a: [1, 2\n", "unclosed"),
        ("a: \"\\q\"\n", "unknown escape"),
        ("a:\n  b: 1\n c: 2\n", "bad indentation"),
    ] {
        let error = parse(text).expect_err(text);
        assert!(error.to_string().contains(message), "{text:?}: {error}");
    }
}

#[test]
fn resolves_tags_like_yaml_v3() {
    for (text, tag) in [
        ("", Tag::Null),
        ("~", Tag::Null),
        ("NULL", Tag::Null),
        ("true", Tag::Bool),
        ("False", Tag::Bool),
        ("yes", Tag::Str),
        ("5432", Tag::Int),
        ("0x10", Tag::Int),
        ("1_000", Tag::Int),
        ("017", Tag::Int),
        ("0o17", Tag::Int),
        ("0b101", Tag::Int),
        ("+5", Tag::Int),
        ("-1", Tag::Int),
        ("1.5", Tag::Float),
        ("1e3", Tag::Float),
        (".5", Tag::Float),
        (".inf", Tag::Float),
        ("30s", Tag::Str),
        ("1m0s", Tag::Str),
        ("5432:5432", Tag::Str),
        ("-x", Tag::Str),
        ("192.0.2.10", Tag::Str),
        ("08", Tag::Float),
    ] {
        assert_eq!(resolve(text), tag, "{text:?}");
    }
}
