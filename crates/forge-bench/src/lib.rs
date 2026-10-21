//! The harness of Forge-Rust's micro-benchmarks (docs/benchmarks.md).
//!
//! `criterion` is not in the dependency graph, so this is a small harness on
//! stable Rust. A [`Case`] is one `scenario/variant` of the catalog shared with
//! Forge-Go, with a fixed number of iterations. [`measure`] warms it up, times
//! three runs of that many iterations, takes the median run and reports
//! nanoseconds, heap bytes and heap allocations per operation (the last two
//! from the counting allocator of [`alloc`] when the bench binary installs
//! it). [`table`] prints the shared format
//! `| Scenario | Variant | ns/op | B/op | allocs/op |`.
//!
//! The bench binary runs the full measurement only when cargo passes
//! `--bench` (`cargo bench`); without it (`cargo test` builds and runs bench
//! targets too) or with `--quick`, every case runs exactly once, so the
//! benchmarks keep compiling and working without anyone reading their numbers.

use std::fmt::Write as _;
use std::hint::black_box;
use std::time::Instant;

pub mod alloc;
pub mod exec;

/// Runs the operation under test the given number of times.
pub type Runner = Box<dyn FnMut(u64)>;

/// One `scenario/variant` of the shared catalog.
pub struct Case {
    scenario: &'static str,
    variant: &'static str,
    iterations: u64,
    reference: bool,
    setup: Box<dyn FnOnce() -> Runner>,
}

impl Case {
    /// A case. `variant` is empty for scenarios without variants
    /// (`session_cookie_verify`, `password_verify`). `setup` builds the
    /// fixture, checks that the operation works, and returns the timed loop;
    /// it runs only when the case is selected, outside the timing.
    pub fn new(
        scenario: &'static str,
        variant: &'static str,
        iterations: u64,
        setup: impl FnOnce() -> Runner + 'static,
    ) -> Self {
        Self {
            scenario,
            variant,
            iterations,
            reference: false,
            setup: Box::new(setup),
        }
    }

    /// Marks the case as a reference measurement that Forge-Go's catalog does
    /// not have; it is printed apart from the shared table.
    #[must_use]
    pub fn reference(mut self) -> Self {
        self.reference = true;
        self
    }

    fn name(&self) -> String {
        if self.variant.is_empty() {
            self.scenario.to_owned()
        } else {
            format!("{}/{}", self.scenario, self.variant)
        }
    }
}

/// What to run and how.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Options {
    /// Measure (`--bench`) instead of running every case once.
    pub full: bool,
    /// Runs per case in a full measurement; the median is reported.
    pub runs: usize,
    /// Run only cases whose `scenario/variant` contains one of these.
    pub filters: Vec<String>,
}

impl Options {
    /// Reads the arguments cargo passes to a bench binary: `--bench` selects
    /// the full measurement, `--quick` overrides it, `--runs N` changes the
    /// number of runs and any argument that does not start with `-` filters
    /// the cases. Other flags are ignored.
    ///
    /// # Errors
    ///
    /// When `--runs` has no positive integer.
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut options = Self {
            full: false,
            runs: 3,
            filters: Vec::new(),
        };
        let mut quick = false;
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--bench" => options.full = true,
                "--quick" => quick = true,
                "--runs" => {
                    options.runs = args
                        .next()
                        .and_then(|value| value.parse().ok())
                        .filter(|runs| *runs > 0)
                        .ok_or("--runs takes a positive integer")?;
                }
                flag if flag.starts_with('-') => {}
                filter => options.filters.push(filter.to_owned()),
            }
        }
        options.full &= !quick;
        Ok(options)
    }

    fn selects(&self, name: &str) -> bool {
        self.filters.is_empty() || self.filters.iter().any(|filter| name.contains(filter))
    }
}

/// The figures of one case: those of its median run.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    /// The scenario, such as `router_match`.
    pub scenario: &'static str,
    /// The variant, such as `static/100`; empty when there is none.
    pub variant: &'static str,
    /// Wall-clock nanoseconds per operation.
    pub ns_per_op: f64,
    /// Heap bytes requested per operation, or `None` when the counting
    /// allocator is not installed.
    pub bytes_per_op: Option<u64>,
    /// Heap allocations per operation, or `None` as above.
    pub allocs_per_op: Option<u64>,
    /// Whether the case is outside Forge-Go's catalog.
    pub reference: bool,
}

/// Runs the selected cases and returns one row for each.
///
/// A full measurement does `iterations / 10` unmeasured iterations first,
/// then `options.runs` timed runs of `iterations` and reports the run with
/// the median time. A quick run does each case once and reports that single
/// iteration (not a measurement).
pub fn measure(cases: Vec<Case>, options: &Options) -> Vec<Row> {
    let counting = alloc_is_installed();
    let mut rows = Vec::new();
    for case in cases {
        let name = case.name();
        if !options.selects(&name) {
            continue;
        }
        eprintln!("running {name}");
        let (scenario, variant, reference) = (case.scenario, case.variant, case.reference);
        let iterations = if options.full { case.iterations } else { 1 };
        let mut run = (case.setup)();
        if options.full {
            run((iterations / 10).max(1));
        }
        let runs = if options.full { options.runs } else { 1 };
        let mut samples: Vec<Sample> = (0..runs).map(|_| time(&mut run, iterations)).collect();
        samples.sort_by(|left, right| left.elapsed_ns.total_cmp(&right.elapsed_ns));
        let median = &samples[samples.len() / 2];
        let divisor = iterations.max(1);
        let per_op = |total: u64| (total + divisor / 2) / divisor;
        rows.push(Row {
            scenario,
            variant,
            ns_per_op: median.elapsed_ns / iterations as f64,
            bytes_per_op: counting.then(|| per_op(median.heap.bytes)),
            allocs_per_op: counting.then(|| per_op(median.heap.allocations)),
            reference,
        });
    }
    rows
}

struct Sample {
    elapsed_ns: f64,
    heap: alloc::Snapshot,
}

fn time(run: &mut Runner, iterations: u64) -> Sample {
    let before = alloc::snapshot();
    let started = Instant::now();
    run(black_box(iterations));
    let elapsed = started.elapsed();
    Sample {
        elapsed_ns: elapsed.as_nanos() as f64,
        heap: alloc::snapshot().since(before),
    }
}

/// Whether the counting allocator is the global allocator: a boxed value
/// must show up in the counters.
fn alloc_is_installed() -> bool {
    let before = alloc::snapshot();
    let boxed = black_box(Box::new(7_u64));
    let counted = alloc::snapshot().since(before).allocations > 0;
    drop(boxed);
    counted
}

/// The rows of the shared catalog as the markdown table both frameworks
/// print, and the reference rows (if any) as a second table after a note.
#[must_use]
pub fn table(rows: &[Row]) -> String {
    let mut out = String::new();
    write_table(&mut out, rows.iter().filter(|row| !row.reference));
    let reference: Vec<&Row> = rows.iter().filter(|row| row.reference).collect();
    if !reference.is_empty() {
        out.push_str("\nReference rows, not part of Forge-Go's catalog:\n\n");
        write_table(&mut out, reference.into_iter());
    }
    out
}

fn write_table<'a>(out: &mut String, rows: impl Iterator<Item = &'a Row>) {
    out.push_str("| Scenario | Variant | ns/op | B/op | allocs/op |\n");
    out.push_str("| --- | --- | ---: | ---: | ---: |\n");
    for row in rows {
        let variant = if row.variant.is_empty() {
            "-"
        } else {
            row.variant
        };
        let heap = |value: Option<u64>| value.map_or_else(|| "n/a".to_owned(), |v| v.to_string());
        // Writing to a String cannot fail.
        let _ = writeln!(
            out,
            "| `{}` | `{variant}` | {} | {} | {} |",
            row.scenario,
            nanoseconds(row.ns_per_op),
            heap(row.bytes_per_op),
            heap(row.allocs_per_op),
        );
    }
}

/// Whole nanoseconds from 100 up, one decimal below (an `uuid_v7` takes tens).
fn nanoseconds(value: f64) -> String {
    if value >= 100.0 {
        format!("{value:.0}")
    } else {
        format!("{value:.1}")
    }
}

/// The lines that identify a measurement: system, CPU, toolchain, profile.
#[must_use]
pub fn environment() -> Vec<String> {
    let cpu = std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|info| {
            info.lines()
                .find(|line| line.starts_with("model name"))
                .and_then(|line| line.split_once(':'))
                .map(|(_, model)| model.trim().to_owned())
        })
        .unwrap_or_else(|| "unknown".to_owned());
    let cpus = std::thread::available_parallelism().map_or(0, std::num::NonZero::get);
    let rustc = std::process::Command::new("rustc")
        .arg("--version")
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map_or_else(|| "unknown".to_owned(), |text| text.trim().to_owned());
    vec![
        format!("os: {}", std::env::consts::OS),
        format!("arch: {}", std::env::consts::ARCH),
        format!("cpu: {cpu} ({cpus} logical CPUs)"),
        format!("toolchain: {rustc}"),
        "profile: bench (release, thin LTO, 1 codegen unit)".to_owned(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|item| (*item).to_owned()).collect()
    }

    #[test]
    fn parses_the_arguments_cargo_passes() {
        let options = Options::parse(args(&["--bench", "router", "--runs", "5", "--nocapture"]));
        assert_eq!(
            options,
            Ok(Options {
                full: true,
                runs: 5,
                filters: vec!["router".to_owned()],
            })
        );
        let quick = Options::parse(args(&["--bench", "--quick"]));
        assert_eq!(quick.map(|options| options.full), Ok(false));
        assert!(Options::parse(args(&["--runs", "0"])).is_err());
        assert!(Options::parse(args(&["--runs"])).is_err());
    }

    #[test]
    fn a_quick_run_does_each_case_once() {
        let counter = std::rc::Rc::new(std::cell::Cell::new(0_u64));
        let seen = std::rc::Rc::clone(&counter);
        let case = Case::new("demo", "", 1_000, move || {
            Box::new(move |n| seen.set(seen.get() + n))
        });
        let options = Options {
            full: false,
            runs: 3,
            filters: Vec::new(),
        };
        let rows = measure(vec![case], &options);
        assert_eq!(counter.get(), 1);
        assert_eq!(rows.len(), 1);
        // The test binary has no counting allocator.
        assert_eq!(rows[0].bytes_per_op, None);
    }

    #[test]
    fn a_full_run_warms_up_and_times_every_run() {
        let counter = std::rc::Rc::new(std::cell::Cell::new(0_u64));
        let seen = std::rc::Rc::clone(&counter);
        let case = Case::new("demo", "v", 100, move || {
            Box::new(move |n| seen.set(seen.get() + n))
        });
        let options = Options {
            full: true,
            runs: 3,
            filters: Vec::new(),
        };
        measure(vec![case], &options);
        // 10 warm-up iterations and three runs of 100.
        assert_eq!(counter.get(), 310);
    }

    #[test]
    fn filters_select_by_scenario_and_variant() {
        let options = Options {
            full: false,
            runs: 1,
            filters: vec!["router_match/param".to_owned()],
        };
        let cases = vec![
            Case::new("router_match", "param/1", 1, || Box::new(|_| {})),
            Case::new("router_match", "static/1", 1, || Box::new(|_| {})),
        ];
        let rows = measure(cases, &options);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].variant, "param/1");
    }

    #[test]
    fn the_table_has_the_shared_columns() {
        let rows = vec![
            Row {
                scenario: "router_match",
                variant: "static/1",
                ns_per_op: 455.4,
                bytes_per_op: Some(64),
                allocs_per_op: Some(1),
                reference: false,
            },
            Row {
                scenario: "password_verify",
                variant: "",
                ns_per_op: 42.25,
                bytes_per_op: None,
                allocs_per_op: None,
                reference: false,
            },
        ];
        assert_eq!(
            table(&rows),
            "| Scenario | Variant | ns/op | B/op | allocs/op |\n\
             | --- | --- | ---: | ---: | ---: |\n\
             | `router_match` | `static/1` | 455 | 64 | 1 |\n\
             | `password_verify` | `-` | 42.2 | n/a | n/a |\n"
        );
    }
}
