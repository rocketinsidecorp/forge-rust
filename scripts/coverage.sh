#!/usr/bin/env bash
# Line coverage of the framework workspace, measured with cargo-llvm-cov and
# gated by the committed floor in .coverage-floor (docs/test-strategy.md,
# "Coverage"). Same name, floor file and output as Forge-Go's
# scripts/coverage.sh.
#
#   scripts/coverage.sh                         # measure, report, gate
#   COVERAGE_JSON=summary.json scripts/coverage.sh   # report and gate an existing summary
#
# Needs llvm-tools (rustup component add llvm-tools-preview) and
# cargo-llvm-cov (cargo install cargo-llvm-cov --version 0.9.1 --locked).
# The #[ignore]d database tests run too, so FORGE_TEST_DATABASE_URL,
# FORGE_TEST_MYSQL_URL and FORGE_TEST_MARIADB_URL must name servers, as in CI.
# cargo-llvm-cov builds into its own target/llvm-cov-target directory, a
# second full build of the workspace: mind the disk.
#
# Measured: the test set of scripts/test-databases.sh (every workspace
# crate's library, binaries, examples and tests, the #[ignore]d database
# tests included; PostgreSQL, MySQL and MariaDB through FORGE_TEST_*_URL, as
# in CI). Not measured, each for the reason in docs/test-strategy.md:
#   - forge-bench and forge-loadgen, the benchmark and load-generation
#     harnesses (fuzz/ is outside the workspace);
#   - crates/forge-cli/templates/, generated-app templates rendered as text,
#     and the generated-application suite (forge-cli --test scaffold), left
#     to CI's scaffold job;
#   - test code: tests/ directories (cargo-llvm-cov's default) and the
#     #[cfg(test)] modules kept in their own files (tests.rs, *_tests.rs,
#     test_support.rs).
# Writes target/coverage/summary.json, prints the total and a per-crate
# table (also to $GITHUB_STEP_SUMMARY when set), and exits 1 below the floor.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
out="$root/target/coverage"
mkdir -p "$out"
floor="$(tr -d '[:space:]' < .coverage-floor)"

summary="${COVERAGE_JSON:-}"
if [ -z "$summary" ]; then
	summary="$out/summary.json"
	cargo llvm-cov clean --workspace
	scripts/test-databases.sh llvm-cov --no-report
	cargo llvm-cov report \
		--ignore-filename-regex '(^|/)crates/(forge-bench|forge-loadgen|forge-cli/templates)/|(^|/)(tests|[a-z0-9_]+_tests|test_support)\.rs$' \
		--json --summary-only --output-path "$summary"
fi

# One row per crate: lines covered, lines, percent; then the total.
report="$(jq -r --arg root "$root/" '
	.data[0] as $data
	| [$data.files[]
		| {crate: ((.filename | ltrimstr($root) | capture("^crates/(?<name>[^/]+)/").name) // "other"),
		   count: .summary.lines.count, covered: .summary.lines.covered}]
	| group_by(.crate)[]
	| {crate: .[0].crate, count: (map(.count) | add), covered: (map(.covered) | add)}
	| "| `\(.crate)` | \(.covered) | \(.count) | \(if .count == 0 then 100 else (.covered * 1000 / .count | floor) / 10 end)% |"
' "$summary")"
total="$(jq -r '.data[0].totals.lines.percent * 10 | floor / 10' "$summary")"

table="$(printf '### Line coverage: %s%% (floor %s%%)\n\n| Crate | Covered lines | Lines | Coverage |\n| --- | ---: | ---: | ---: |\n%s\n' "$total" "$floor" "$report")"
printf '%s\n' "$table"
if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
	printf '%s\n' "$table" >> "$GITHUB_STEP_SUMMARY"
fi

if awk -v total="$total" -v floor="$floor" 'BEGIN { exit !(total + 0 < floor + 0) }'; then
	echo "coverage ${total}% is below the floor ${floor}% (.coverage-floor)" >&2
	exit 1
fi
echo "coverage ${total}% >= floor ${floor}%"
