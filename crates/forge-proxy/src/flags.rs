//! Command-line flags parsed the way Go's `flag` package parses them, so
//! `forge-proxy deploy -service shop -target … -tls` means the same to the
//! Rust and the Go proxy: `-name value`, `-name=value`, `--name` alike;
//! boolean flags take no separate value; parsing stops at the first
//! positional argument or `--`.

use std::collections::HashMap;

use crate::duration::GoDuration;

/// One flag definition: name, whether it is boolean, and its usage text.
pub(crate) type Spec = (&'static str, bool, &'static str);

/// Parsed flags and the remaining arguments.
#[derive(Debug, Default)]
pub(crate) struct Parsed {
    values: HashMap<&'static str, String>,
    pub(crate) rest: Vec<String>,
}

/// Why parsing failed; the message is Go's.
#[derive(Debug)]
pub(crate) struct FlagError(pub(crate) String);

pub(crate) fn parse(args: &[String], spec: &[Spec]) -> Result<Parsed, FlagError> {
    let mut parsed = Parsed::default();
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg.len() < 2 || !arg.starts_with('-') {
            break;
        }
        let mut name = &arg[1..];
        if let Some(stripped) = name.strip_prefix('-') {
            if stripped.is_empty() {
                // "--" ends the flags.
                index += 1;
                break;
            }
            name = stripped;
        }
        if name.is_empty() || name.starts_with('-') || name.starts_with('=') {
            return Err(FlagError(format!("bad flag syntax: {arg}")));
        }
        index += 1;
        let (name, inline) = match name.split_once('=') {
            Some((name, value)) => (name, Some(value.to_owned())),
            None => (name, None),
        };
        if name == "help" || name == "h" {
            return Err(FlagError("help requested".to_owned()));
        }
        let Some((canonical, boolean, _)) = spec.iter().find(|(n, _, _)| *n == name) else {
            return Err(FlagError(format!("flag provided but not defined: -{name}")));
        };
        let value = if *boolean {
            let value = inline.unwrap_or_else(|| "true".to_owned());
            if parse_bool(&value).is_none() {
                return Err(FlagError(format!(
                    "invalid boolean value {value:?} for -{name}: parse error"
                )));
            }
            value
        } else if let Some(value) = inline {
            value
        } else if index < args.len() {
            index += 1;
            args[index - 1].clone()
        } else {
            return Err(FlagError(format!("flag needs an argument: -{name}")));
        };
        parsed.values.insert(canonical, value);
    }
    parsed.rest = args[index..].to_vec();
    Ok(parsed)
}

/// Go's `strconv.ParseBool`.
fn parse_bool(value: &str) -> Option<bool> {
    match value {
        "1" | "t" | "T" | "true" | "TRUE" | "True" => Some(true),
        "0" | "f" | "F" | "false" | "FALSE" | "False" => Some(false),
        _ => None,
    }
}

/// Go's `strconv.ParseInt(value, 0, 64)`: decimal, `0x`, `0o`, `0b` and
/// leading-zero octal, optional sign and underscores between digits.
fn parse_int(value: &str) -> Option<i64> {
    let (negative, digits) = match value.as_bytes().first()? {
        b'-' => (true, &value[1..]),
        b'+' => (false, &value[1..]),
        _ => (false, value),
    };
    let lower = digits.to_ascii_lowercase();
    let (radix, body) = if let Some(rest) = lower.strip_prefix("0x") {
        (16, rest.to_owned())
    } else if let Some(rest) = lower.strip_prefix("0o") {
        (8, rest.to_owned())
    } else if let Some(rest) = lower.strip_prefix("0b") {
        (2, rest.to_owned())
    } else if lower.len() > 1 && lower.starts_with('0') {
        (8, lower[1..].to_owned())
    } else {
        (10, lower.clone())
    };
    if body.contains('_') {
        // Underscores only between digits (Go literal syntax).
        if body.starts_with('_') || body.ends_with('_') || body.contains("__") {
            return None;
        }
    }
    let cleaned: String = body.chars().filter(|c| *c != '_').collect();
    if cleaned.is_empty() {
        return None;
    }
    let magnitude = i128::from_str_radix(&cleaned, radix).ok()?;
    let signed = if negative { -magnitude } else { magnitude };
    i64::try_from(signed).ok()
}

impl Parsed {
    pub(crate) fn string(&self, name: &str, default: &str) -> String {
        self.values
            .get(name)
            .cloned()
            .unwrap_or_else(|| default.to_owned())
    }

    pub(crate) fn bool(&self, name: &str) -> bool {
        self.values
            .get(name)
            .and_then(|value| parse_bool(value))
            .unwrap_or(false)
    }

    pub(crate) fn int(&self, name: &str, default: i64) -> Result<i64, FlagError> {
        match self.values.get(name) {
            None => Ok(default),
            Some(value) => parse_int(value).ok_or_else(|| {
                FlagError(format!(
                    "invalid value {value:?} for flag -{name}: parse error"
                ))
            }),
        }
    }

    pub(crate) fn duration(
        &self,
        name: &str,
        default: GoDuration,
    ) -> Result<GoDuration, FlagError> {
        match self.values.get(name) {
            None => Ok(default),
            Some(value) => GoDuration::parse(value).map_err(|_| {
                FlagError(format!(
                    "invalid value {value:?} for flag -{name}: parse error"
                ))
            }),
        }
    }
}

/// The usage text Go prints: `  -name type\n    \tusage`.
pub(crate) fn usage(command: &str, spec: &[Spec]) -> String {
    let mut text = format!("Usage of {command}:\n");
    let mut sorted: Vec<&Spec> = spec.iter().collect();
    sorted.sort_by_key(|(name, _, _)| *name);
    for (name, boolean, help) in sorted {
        if *boolean {
            text.push_str(&format!("  -{name}\n    \t{help}\n"));
        } else {
            text.push_str(&format!("  -{name} value\n    \t{help}\n"));
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPEC: &[Spec] = &[
        ("service", false, "service name"),
        ("tls", true, "tls"),
        ("count", false, "count"),
        ("wait", false, "wait"),
    ];

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn parses_like_go_flag() {
        let parsed = parse(
            &args(&[
                "-service",
                "shop",
                "--tls",
                "-count=0x10",
                "-wait",
                "1m0s",
                "on",
                "-x",
            ]),
            SPEC,
        )
        .expect("valid");
        assert_eq!(parsed.string("service", ""), "shop");
        assert!(parsed.bool("tls"));
        assert_eq!(parsed.int("count", 0).expect("int"), 16);
        assert_eq!(
            parsed.duration("wait", GoDuration::ZERO).expect("duration"),
            GoDuration::from_secs(60)
        );
        assert_eq!(parsed.rest, ["on", "-x"]);
        let parsed = parse(&args(&["--", "-service"]), SPEC).expect("valid");
        assert_eq!(parsed.rest, ["-service"]);
        let parsed = parse(&args(&["-tls=false", "x"]), SPEC).expect("valid");
        assert!(!parsed.bool("tls"));
    }

    #[test]
    fn errors_like_go_flag() {
        for (list, message) in [
            (&["-bogus"][..], "flag provided but not defined: -bogus"),
            (&["-service"][..], "flag needs an argument: -service"),
            (&["---x"][..], "bad flag syntax: ---x"),
            (
                &["-tls=maybe"][..],
                "invalid boolean value \"maybe\" for -tls: parse error",
            ),
        ] {
            let error = parse(&args(list), SPEC).expect_err("invalid");
            assert_eq!(error.0, message);
        }
        let parsed = parse(&args(&["-count", "ten"]), SPEC).expect("parsed");
        assert!(parsed.int("count", 0).is_err());
        assert_eq!(parse_int("017"), Some(15));
        assert_eq!(parse_int("-0b101"), Some(-5));
        assert_eq!(parse_int("1_000"), Some(1000));
        assert_eq!(parse_int("1__0"), None);
        assert_eq!(parse_int("0x1_0"), Some(16));
    }
}
