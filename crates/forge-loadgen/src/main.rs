//! Command loadgen is the HTTP load generator of `scripts/bench.sh`: the Rust
//! port of Forge-Go's `internal/loadgen`. A closed loop of a fixed number of
//! workers sends the requests of the benchmark scenarios
//! (docs/benchmarks.md) to a running generated application for a fixed time
//! and reports requests per second and latency percentiles, in the output
//! format both frameworks share.
//!
//! It uses only the standard library. It signs in once (the password comes
//! from the `BENCH_PASSWORD` environment variable, never an argument),
//! creates the rows the scenarios read, and measures nothing before the
//! warm-up has passed. Scenarios are measured in rounds, interleaved, so a
//! slow drift of the host affects every scenario alike.
//!
//! Forge-Go's flags are the same, plus `-tenant-id` (`forge setup` prints
//! the id): the tenant cookie of the session, when sign-in sets none.

mod http;
mod report;
mod target;

use std::io::Write as _;
use std::process::ExitCode;
use std::time::Duration;

use report::{Outcome, Settings, median_of, stats_of};
use target::{ALL, AVAILABLE, SHOW_JSON, Target};

/// Everything the flags and the environment configure.
#[derive(Clone, Debug)]
pub struct Config {
    pub base: String,
    pub collection: String,
    pub tenant: String,
    pub tenant_id: String,
    pub email: String,
    pub password: String,
    pub concurrency: usize,
    pub duration: Duration,
    pub warmup: Duration,
    pub runs: usize,
    pub rows: usize,
    pub scenarios: Vec<String>,
    pub format: String,
}

const USAGE: &str = "usage: loadgen -tenant-id UUID [flags]   (the owner's password is read from $BENCH_PASSWORD)

  -base URL          base URL of the application (default http://127.0.0.1:8080)
  -collection NAME   plural name of the scaffolded resource (HTML /NAME; fields title and body) (default posts)
  -tenant SLUG       tenant slug to sign in to (default bench)
  -tenant-id UUID    id of that tenant, as `setup` prints it (required)
  -email ADDRESS     email of the owner account (default bench@example.com)
  -concurrency N     concurrent workers (connections) (default 16)
  -duration D        measured time of each scenario in each run (default 10s)
  -warmup D          unmeasured warm-up of each scenario (default 2s)
  -runs N            measured runs of each scenario (default 3)
  -rows N            rows listed by index_html (the page size, at most 200) (default 50)
  -scenarios LIST    comma-separated scenarios to run (default health,welcome,index_html,create_form;
                     show_json needs a JSON API Forge-Rust does not generate yet)
  -format F          output: markdown or json (default markdown)
";

/// A Go-style duration: a sequence of number and unit (`ns`, `us`, `ms`, `s`,
/// `m`, `h`), such as `10s`, `1m30s` or `500ms`.
fn parse_duration(text: &str) -> Result<Duration, String> {
    let bad = || format!("invalid duration {text:?}");
    let mut total = 0.0_f64;
    let mut rest = text;
    if rest.is_empty() {
        return Err(bad());
    }
    while !rest.is_empty() {
        let digits = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .ok_or_else(bad)?;
        let value: f64 = rest[..digits].parse().map_err(|_| bad())?;
        rest = &rest[digits..];
        let unit_end = rest
            .find(|c: char| c.is_ascii_digit() || c == '.')
            .unwrap_or(rest.len());
        let scale = match &rest[..unit_end] {
            "ns" => 1e-9,
            "us" | "µs" => 1e-6,
            "ms" => 1e-3,
            "s" => 1.0,
            "m" => 60.0,
            "h" => 3600.0,
            _ => return Err(bad()),
        };
        total += value * scale;
        rest = &rest[unit_end..];
    }
    Duration::try_from_secs_f64(total).map_err(|_| format!("duration {text:?} is out of range"))
}

fn parse(args: &[String], password: String) -> Result<Config, String> {
    let mut cfg = Config {
        base: "http://127.0.0.1:8080".to_owned(),
        collection: "posts".to_owned(),
        tenant: "bench".to_owned(),
        tenant_id: String::new(),
        email: "bench@example.com".to_owned(),
        password,
        concurrency: 16,
        duration: Duration::from_secs(10),
        warmup: Duration::from_secs(2),
        runs: 3,
        rows: 50,
        scenarios: AVAILABLE.iter().map(|name| (*name).to_owned()).collect(),
        format: "markdown".to_owned(),
    };
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let flag = arg.trim_start_matches('-');
        if flag == arg.as_str() {
            return Err(format!("unexpected argument {arg:?}"));
        }
        let (name, inline) = match flag.split_once('=') {
            Some((name, value)) => (name, Some(value.to_owned())),
            None => (flag, None),
        };
        let mut value = || {
            inline
                .clone()
                .or_else(|| args.next().cloned())
                .ok_or_else(|| format!("flag -{name} needs a value"))
        };
        let number = |text: String| {
            text.parse::<usize>()
                .map_err(|_| format!("invalid value {text:?} for flag -{name}"))
        };
        match name {
            "base" => cfg.base = value()?,
            "collection" => cfg.collection = value()?,
            "tenant" => cfg.tenant = value()?,
            "tenant-id" => cfg.tenant_id = value()?,
            "email" => cfg.email = value()?,
            "concurrency" => cfg.concurrency = number(value()?)?,
            "duration" => cfg.duration = parse_duration(&value()?)?,
            "warmup" => cfg.warmup = parse_duration(&value()?)?,
            "runs" => cfg.runs = number(value()?)?,
            "rows" => cfg.rows = number(value()?)?,
            "scenarios" => {
                cfg.scenarios = value()?.split(',').map(str::to_owned).collect();
            }
            "format" => cfg.format = value()?,
            "h" | "help" => return Err(String::new()),
            _ => return Err(format!("flag provided but not defined: -{name}")),
        }
    }
    validate(&cfg)?;
    Ok(cfg)
}

fn validate(cfg: &Config) -> Result<(), String> {
    if cfg.concurrency < 1 || cfg.runs < 1 || cfg.duration.is_zero() {
        return Err(
            "concurrency, runs and duration must be positive and warmup not negative".to_owned(),
        );
    }
    if !(1..=200).contains(&cfg.rows) {
        return Err("rows must be between 1 and 200".to_owned());
    }
    if cfg.password.is_empty() {
        return Err("BENCH_PASSWORD is required".to_owned());
    }
    if cfg.tenant_id.is_empty() {
        return Err("-tenant-id is required (the id `setup` prints)".to_owned());
    }
    if cfg.tenant_id.len() != 36
        || !cfg
            .tenant_id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
    {
        return Err(format!(
            "-tenant-id must be a UUID, not {:?}",
            cfg.tenant_id
        ));
    }
    if cfg.format != "markdown" && cfg.format != "json" {
        return Err("format must be markdown or json".to_owned());
    }
    for name in &cfg.scenarios {
        if name == SHOW_JSON {
            return Err("show_json is not available in Forge-Rust: a generated application has no JSON scaffold or bearer-token login yet (docs/benchmarks.md)".to_owned());
        }
        if !ALL.contains(&name.as_str()) {
            return Err(format!(
                "unknown scenario {name:?} (have {})",
                ALL.join(", ")
            ));
        }
    }
    Ok(())
}

/// Warms every scenario up, then measures `cfg.runs` rounds of all of them,
/// one window each, in a fixed order.
fn measure(cfg: &Config, target: &mut Target) -> Result<Vec<Outcome>, String> {
    let mut progress = std::io::stderr();
    if !cfg.warmup.is_zero() {
        for name in &cfg.scenarios {
            let _ = writeln!(
                progress,
                "warm-up {name} ({})",
                report::go_duration(cfg.warmup)
            );
            target.drive(name, cfg.concurrency, cfg.warmup);
        }
    }
    let mut outcomes: Vec<Outcome> = cfg
        .scenarios
        .iter()
        .map(|name| Outcome {
            scenario: name.clone(),
            runs: Vec::new(),
            median: report::Stats::default(),
            errors: 0,
        })
        .collect();
    for round in 1..=cfg.runs {
        for outcome in &mut outcomes {
            let _ = writeln!(
                progress,
                "run {round}/{} {} ({})",
                cfg.runs,
                outcome.scenario,
                report::go_duration(cfg.duration)
            );
            let sample = target.drive(&outcome.scenario, cfg.concurrency, cfg.duration);
            if sample.ok == 0 {
                return Err(format!(
                    "{}: no request succeeded ({} errors)",
                    outcome.scenario, sample.errors
                ));
            }
            outcome.runs.push(stats_of(&sample));
            outcome.errors += sample.errors;
        }
    }
    for outcome in &mut outcomes {
        outcome.median = median_of(&outcome.runs);
    }
    Ok(outcomes)
}

fn run(args: &[String], password: String) -> Result<String, String> {
    let cfg = parse(args, password)?;
    let mut target = Target::new(&cfg)?;
    let outcomes = measure(&cfg, &mut target)?;
    let settings = Settings {
        concurrency: cfg.concurrency,
        duration: cfg.duration,
        warmup: cfg.warmup,
        runs: cfg.runs,
        rows: cfg.rows,
    };
    Ok(if cfg.format == "json" {
        report::json(&settings, &outcomes)
    } else {
        report::markdown(&settings, &outcomes)
    })
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let password = std::env::var("BENCH_PASSWORD").unwrap_or_default();
    match run(&args, password) {
        Ok(output) => {
            print!("{output}");
            ExitCode::SUCCESS
        }
        Err(message) if message.is_empty() => {
            eprint!("{USAGE}");
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("loadgen: {message}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TENANT: &str = "01941f29-7c00-7000-8000-000000000001";

    fn flags(list: &[&str]) -> Vec<String> {
        list.iter().map(|flag| (*flag).to_owned()).collect()
    }

    #[test]
    fn parses_go_style_durations() {
        assert_eq!(parse_duration("10s"), Ok(Duration::from_secs(10)));
        assert_eq!(parse_duration("1m30s"), Ok(Duration::from_secs(90)));
        assert_eq!(parse_duration("500ms"), Ok(Duration::from_millis(500)));
        assert_eq!(parse_duration("1.5s"), Ok(Duration::from_millis(1500)));
        assert!(parse_duration("").is_err());
        assert!(parse_duration("10").is_err());
        assert!(parse_duration("s").is_err());
        assert!(parse_duration("10parsecs").is_err());
    }

    #[test]
    fn the_defaults_are_forge_gos() {
        let cfg = parse(&flags(&["-tenant-id", TENANT]), "secret".to_owned());
        let Ok(cfg) = cfg else {
            panic!("{cfg:?}");
        };
        assert_eq!(
            (
                cfg.concurrency,
                cfg.duration,
                cfg.warmup,
                cfg.runs,
                cfg.rows
            ),
            (16, Duration::from_secs(10), Duration::from_secs(2), 3, 50)
        );
        assert_eq!(
            cfg.scenarios,
            ["health", "welcome", "index_html", "create_form"]
        );
        assert_eq!(cfg.format, "markdown");
    }

    #[test]
    fn flags_take_one_or_two_dashes_and_inline_values() {
        let cfg = parse(
            &flags(&[
                "--tenant-id=01941f29-7c00-7000-8000-000000000001",
                "-runs",
                "5",
                "--format",
                "json",
                "-scenarios=health,welcome",
            ]),
            "secret".to_owned(),
        );
        let Ok(cfg) = cfg else {
            panic!("{cfg:?}");
        };
        assert_eq!((cfg.runs, cfg.format.as_str()), (5, "json"));
        assert_eq!(cfg.scenarios, ["health", "welcome"]);
    }

    #[test]
    fn refuses_what_it_cannot_measure() {
        let refuse = |list: &[&str], password: &str| {
            parse(&flags(list), password.to_owned())
                .err()
                .unwrap_or_default()
        };
        assert!(refuse(&["-tenant-id", TENANT], "").contains("BENCH_PASSWORD"));
        assert!(refuse(&[], "x").contains("-tenant-id"));
        assert!(refuse(&["-tenant-id", "bench"], "x").contains("must be a UUID"));
        assert!(refuse(&["-tenant-id", "bench) and its owner"], "x").contains("must be a UUID"));
        assert!(refuse(&["-tenant-id", TENANT, "-rows", "201"], "x").contains("rows"));
        assert!(refuse(&["-tenant-id", TENANT, "-format", "xml"], "x").contains("format"));
        assert!(
            refuse(&["-tenant-id", TENANT, "-scenarios", "nope"], "x").contains("unknown scenario")
        );
        assert!(
            refuse(&["-tenant-id", TENANT, "-scenarios", "show_json"], "x")
                .contains("not available")
        );
        assert!(refuse(&["-tenant-id", TENANT, "-concurrency", "0"], "x").contains("positive"));
        assert!(refuse(&["-bogus"], "x").contains("not defined"));
        assert!(refuse(&["stray"], "x").contains("unexpected argument"));
    }
}
