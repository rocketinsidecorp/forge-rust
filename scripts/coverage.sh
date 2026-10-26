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
# Two totals are printed. The gross total counts everything measured above.
# The gated total, the one the floor is checked against, also leaves out the
# contract test kits, the testing.rs files listed below: they are test
# harnesses, and the only code of theirs no test reaches is the assertion
# failure branches that run when a backend breaks the contract. The kits stay
# verified by running against every real backend in CI (docs/test-strategy.md,
# "Coverage"). Excluded, exactly these files and no others:
#   crates/forge-cache/src/testing.rs      crates/forge-ratelimit/src/testing.rs
#   crates/forge-db/src/testing.rs         crates/forge-storage/src/testing.rs
#   crates/forge-jobs/src/testing.rs       crates/forge-telemetry/src/testing.rs
#   crates/forge-mail/src/testing.rs
# The list is checked first, as Forge-Go's script checks its kits, and the
# script fails when a listed kit does not exist, is not behind a `testing`
# feature gate (the `pub mod testing;` line in the crate's lib.rs must follow a
# `#[cfg(...)]` naming `feature = "testing"`), or is reachable from non-test
# code: another crate's Cargo.toml may enable a kit's `testing` feature only
# under [dev-dependencies] or from a feature that is itself named `testing`.
# A kit shipped code depends on is production code and has to be covered.
# Writes target/coverage/summary.json (the gross measurement) and, when it
# measures, the line-level target/coverage/lcov.info (one DA:<line>,<hits>
# record per line of each file), prints both totals and a per-crate table
# (also to $GITHUB_STEP_SUMMARY when set), and exits 1 when the gated total is
# below the floor.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
out="$root/target/coverage"
mkdir -p "$out"
floor="$(tr -d '[:space:]' < .coverage-floor)"

# The kit crates, the same list as `kits` below (docs/test-strategy.md,
# "Contract test kits").
kit_crates=(forge-cache forge-db forge-jobs forge-mail forge-ratelimit forge-storage forge-telemetry)

for crate in "${kit_crates[@]}"; do
	kit="crates/$crate/src/testing.rs"
	if [ ! -f "$kit" ]; then
		echo "coverage kit $kit does not exist: update kit_crates and kits in scripts/coverage.sh and docs/test-strategy.md" >&2
		exit 1
	fi
	if ! grep -B1 -E '^[[:space:]]*pub mod testing;' "crates/$crate/src/lib.rs" | head -n 1 |
		grep -Eq '^[[:space:]]*#\[cfg\(.*feature = "testing"'; then
		echo "coverage kit $kit is not behind a testing feature gate in crates/$crate/src/lib.rs: it is production code and cannot be excluded" >&2
		exit 1
	fi
done
# Another crate may use a kit from tests only: its `testing` feature may be
# enabled under [dev-dependencies], or by a feature named `testing`, nowhere else.
kit_pattern="$(IFS='|'; echo "${kit_crates[*]}")"
for manifest in crates/*/Cargo.toml; do
	if ! awk -v kits="^($kit_pattern)[?]?/testing\$" -v pattern="^($kit_pattern)[[:space:]]*=.*\"testing\"" '
		/^\[/ { section = $0; feature = ""; next }
		section == "[features]" && /^[A-Za-z0-9_-]+[[:space:]]*=/ { feature = $1 }
		section == "[features]" && feature != "testing" {
			line = $0
			while (match(line, /"[^"]+"/)) {
				item = substr(line, RSTART + 1, RLENGTH - 2)
				if (item ~ kits) { print FILENAME ": feature " feature " enables " item; bad = 1 }
				line = substr(line, RSTART + RLENGTH)
			}
		}
		section ~ /^\[(build-)?dependencies\]$|^\[target\..*dependencies\]$/ && $0 ~ pattern {
			print FILENAME ": " $0; bad = 1
		}
		END { exit bad }
	' "$manifest"; then
		echo "a coverage kit's testing feature is enabled outside tests (see above): the kit is production code and cannot be excluded" >&2
		exit 1
	fi
done

# What no measurement counts, and what the gated total also leaves out.
ignored='(^|/)crates/(forge-bench|forge-loadgen|forge-cli/templates)/|(^|/)(tests|[a-z0-9_]+_tests|test_support)\.rs$'
kits="(^|/)crates/($kit_pattern)/src/testing\\.rs\$"
gated_ignored="$ignored|$kits"

summary="${COVERAGE_JSON:-}"
if [ -z "$summary" ]; then
	summary="$out/summary.json"
	cargo llvm-cov clean --workspace
	scripts/test-databases.sh llvm-cov --no-report
	cargo llvm-cov report \
		--ignore-filename-regex "$ignored" \
		--json --summary-only --output-path "$summary"
	# The same measurement line by line, for finding what no test reaches.
	cargo llvm-cov report \
		--ignore-filename-regex "$ignored" \
		--lcov --output-path "$out/lcov.info"
fi

# One row per crate with its gross and gated lines (covered, lines, percent),
# then both totals. The gated figures are the gross files minus the kits.
report="$(jq -r --arg root "$root/" --arg gated "$gated_ignored" '
	def tenths($c; $n): if $n == 0 then 1000 else ($c * 1000 / $n | floor) end;
	def pct: tenths(.covered; .count) | "\(. / 10 | floor).\(. % 10)";
	.data[0].files
	| map({crate: ((.filename | ltrimstr($root) | capture("^crates/(?<name>[^/]+)/").name) // "other"),
	       gross: .summary.lines.count, gross_covered: .summary.lines.covered,
	       kit: (.filename | test($gated))})
	| group_by(.crate)[]
	| {crate: .[0].crate,
	   gross: {count: (map(.gross) | add), covered: (map(.gross_covered) | add)},
	   gated: {count: (map(select(.kit | not) | .gross) | add // 0),
	           covered: (map(select(.kit | not) | .gross_covered) | add // 0)}}
	| "| `\(.crate)` | \(.gross.covered) | \(.gross.count) | \(.gross | pct)% | \(.gated.covered) | \(.gated.count) | \(.gated | pct)% |"
' "$summary")"
# Totals from the same per-file counts, rounded down to one decimal.
read -r gross gated < <(jq -r --arg gated "$gated_ignored" '
	def pct($c; $n): (if $n == 0 then 1000 else ($c * 1000 / $n | floor) end) | "\(. / 10 | floor).\(. % 10)";
	.data[0].files as $files
	| ($files | map(.summary.lines.covered) | add) as $gc | ($files | map(.summary.lines.count) | add) as $gn
	| ($files | map(select(.filename | test($gated) | not))) as $kept
	| ($kept | map(.summary.lines.covered) | add) as $kc | ($kept | map(.summary.lines.count) | add) as $kn
	| "\(pct($gc; $gn)) \(pct($kc; $kn))"
' "$summary")

table="$(printf '### Line coverage: %s%% gated, %s%% gross (floor %s%%, checked against the gated total)\n\n| Crate | Gross covered | Gross lines | Gross | Gated covered | Gated lines | Gated |\n| --- | ---: | ---: | ---: | ---: | ---: | ---: |\n%s\n' "$gated" "$gross" "$floor" "$report")"
printf '%s\n' "$table"
if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
	printf '%s\n' "$table" >> "$GITHUB_STEP_SUMMARY"
fi

if awk -v total="$gated" -v floor="$floor" 'BEGIN { exit !(total + 0 < floor + 0) }'; then
	echo "gated coverage ${gated}% (gross ${gross}%) is below the floor ${floor}% (.coverage-floor)" >&2
	exit 1
fi
echo "gated coverage ${gated}% (gross ${gross}%) >= floor ${floor}%"
