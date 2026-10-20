//! The micro-benchmarks of docs/benchmarks.md: the scenario catalog shared
//! with Forge-Go, one module per area.
//!
//! ```sh
//! cargo bench -p forge-bench                       # every scenario, median of 3 runs
//! cargo bench -p forge-bench -- router_match       # scenarios whose name contains the text
//! cargo bench -p forge-bench -- --quick            # each scenario once (what CI and `cargo test` do)
//! ```

use forge_bench::alloc::Counting;
use forge_bench::{Options, environment, measure, table};

mod http;
mod identity;
mod misc;
mod render;
mod support;

/// Heap figures (`B/op`, `allocs/op`) need the counting allocator
/// (`forge_bench::alloc`).
#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn main() -> std::process::ExitCode {
    let options = match Options::parse(std::env::args().skip(1)) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("forge-bench: {message}");
            return std::process::ExitCode::from(2);
        }
    };
    let mut cases = Vec::new();
    http::cases(&mut cases);
    render::cases(&mut cases);
    identity::cases(&mut cases);
    misc::cases(&mut cases);
    let rows = measure(cases, &options);
    if options.full {
        for line in environment() {
            println!("{line}");
        }
        println!(
            "runs: {} per scenario, median reported; iterations are fixed per scenario\n",
            options.runs
        );
        print!("{}", table(&rows));
    } else {
        println!(
            "forge-bench: {} scenarios ran once (not a measurement; use `cargo bench`)",
            rows.len()
        );
    }
    std::process::ExitCode::SUCCESS
}
