#!/usr/bin/env bash
# Runs the workspace tests including the #[ignore]d ones, which need database
# servers: PostgreSQL (FORGE_TEST_DATABASE_URL), MySQL (FORGE_TEST_MYSQL_URL)
# and MariaDB (FORGE_TEST_MARIADB_URL). CI's `database` and `coverage` jobs run
# exactly this set (docs/test-strategy.md, "Database tests in CI").
#
#   scripts/test-databases.sh                      # cargo test ...
#   scripts/test-databases.sh llvm-cov --no-report # the same set, measured
#
# The slow generated-application suites (forge-cli --test scaffold and
# --test readme_quickstart) are left to CI's `scaffold` job, and the benchmark harnesses (forge-bench,
# forge-loadgen) to the `test` and `benchmarks` jobs. Every other test file of
# forge-cli is picked up automatically.
set -euo pipefail

cd "$(dirname "$0")/.."
runner=("$@")
if [ "${#runner[@]}" -eq 0 ]; then
	runner=(test)
fi

cargo "${runner[@]}" --workspace --all-targets --all-features --locked \
	--exclude forge-cli --exclude forge-bench --exclude forge-loadgen \
	-- --include-ignored

cli_tests=()
for file in crates/forge-cli/tests/*.rs; do
	name="$(basename "$file" .rs)"
	if [ "$name" != scaffold ] && [ "$name" != readme_quickstart ]; then
		cli_tests+=(--test "$name")
	fi
done
cargo "${runner[@]}" -p forge-cli --all-features --locked \
	--lib --bins --examples "${cli_tests[@]}" \
	-- --include-ignored
