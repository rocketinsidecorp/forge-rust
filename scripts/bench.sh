#!/usr/bin/env bash
# End-to-end HTTP benchmark of Forge-Rust (docs/benchmarks.md).
#
# Builds the forge CLI and the load generator, generates an application
# against this checkout (`forge new --forge-path`), scaffolds a tenant-scoped
# "post" resource, wires it, runs it (a release build) against PostgreSQL as
# non-superuser roles (so row-level security applies, as in production) and
# measures it with the load generator in crates/forge-loadgen, the Rust port
# of Forge-Go's internal/loadgen: requests per second and p50/p95/p99 latency
# of the readiness probe, the welcome page, the scaffold index (HTML) and the
# scaffold create (form POST with CSRF). It prints a markdown table with the
# columns of Forge-Go's `scripts/bench.sh`.
#
#   export FORGE_TEST_DATABASE_URL='postgres://postgres:postgres@127.0.0.1:5432/postgres'
#   scripts/bench.sh                       # 16 workers, 10s per scenario, 3 runs
#   BENCH_DURATION=3s BENCH_RUNS=1 scripts/bench.sh
#   scripts/bench.sh -format json          # extra arguments go to the load generator
#
# The URL names a server whose user may create roles and databases (a
# superuser, as in the repository's tests); FORGE_TEST_POSTGRES_URL, Forge-Go's
# name for it, is accepted too. The script creates two roles and a database of
# its own (Forge-Rust splits the migration and the runtime credentials,
# ADR 0011: the runtime role cannot create schema objects) and drops all three,
# and deletes everything it built, when it ends. The URL is psql's argument,
# which ps shows: on a shared machine leave the password out of it and use
# PGPASSWORD or ~/.pgpass. The generated roles' passwords never appear in an
# argument: statements go to psql's standard input and the application reads
# its URLs from the environment.
#
# Settings (environment): BENCH_CONCURRENCY (16), BENCH_DURATION (10s),
# BENCH_WARMUP (2s), BENCH_RUNS (3), BENCH_ROWS (50), BENCH_PORT (18080),
# and BENCH_TARGET_DIR (a Cargo target directory to reuse and keep; the default
# is a fresh one that is deleted at the end).
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
admin="${FORGE_TEST_DATABASE_URL:-${FORGE_TEST_POSTGRES_URL:-}}"
if [ -z "$admin" ]; then
	echo "bench.sh: set FORGE_TEST_DATABASE_URL to a PostgreSQL server whose user may create roles and databases" >&2
	exit 2
fi
# A generated application refuses unknown FORGE_* variables at start-up, so the
# only ones it sees are the ones this script sets.
for name in $(compgen -e | grep '^FORGE_' || true); do unset "$name"; done
for tool in cargo psql openssl; do
	command -v "$tool" >/dev/null || { echo "bench.sh: $tool is required" >&2; exit 2; }
done

concurrency="${BENCH_CONCURRENCY:-16}"
duration="${BENCH_DURATION:-10s}"
warmup="${BENCH_WARMUP:-2s}"
runs="${BENCH_RUNS:-3}"
rows="${BENCH_ROWS:-50}"
port="${BENCH_PORT:-18080}"
case "$rows" in '' | *[!0-9]*) echo "bench.sh: BENCH_ROWS must be a number" >&2; exit 2 ;; esac

work="$(mktemp -d "${TMPDIR:-/tmp}/forge-bench.XXXXXX")"
target="${BENCH_TARGET_DIR:-$work/target}"
suffix="$(openssl rand -hex 4)"
migrator="forge_bench_${suffix}_migrator"
runtime="forge_bench_${suffix}_runtime"
database="forge_bench_${suffix}"
migrator_password="$(openssl rand -hex 16)"
runtime_password="$(openssl rand -hex 16)"
owner_password="$(openssl rand -hex 16)"
server_pid=""
created_database=0
created_roles=0

cleanup() {
	status=$?
	trap - EXIT INT TERM
	if [ -n "$server_pid" ]; then
		kill "$server_pid" 2>/dev/null || true
		wait "$server_pid" 2>/dev/null || true
	fi
	if [ "$created_database" = 1 ]; then
		psql "$admin" -qAtc "DROP DATABASE IF EXISTS ${database} WITH (FORCE)" >/dev/null 2>&1 || echo "bench.sh: could not drop database ${database}" >&2
	fi
	if [ "$created_roles" = 1 ]; then
		psql "$admin" -qAtc "DROP ROLE IF EXISTS ${runtime}" >/dev/null 2>&1 || echo "bench.sh: could not drop role ${runtime}" >&2
		psql "$admin" -qAtc "DROP ROLE IF EXISTS ${migrator}" >/dev/null 2>&1 || echo "bench.sh: could not drop role ${migrator}" >&2
	fi
	if [ "$status" != 0 ] && [ -f "$work/server.log" ]; then
		echo "--- application log (last lines) ---" >&2
		tail -n 20 "$work/server.log" >&2 || true
	fi
	rm -rf "$work"
	exit "$status"
}
trap cleanup EXIT INT TERM

echo "building the forge CLI and the load generator (release)" >&2
export CARGO_TARGET_DIR="$target"
(cd "$root" && cargo build --release --locked --quiet -p forge-cli -p forge-loadgen)
forge="$target/release/forge"
loadgen="$target/release/loadgen"

echo "generating the application" >&2
app="$work/benchapp"
# The workspace's lockfile pins the versions the application resolves; cargo
# adds what the application needs beyond it.
(cd "$work" && "$forge" new benchapp --skip-docker --skip-ci --skip-lockfile --forge-path "$root/crates/forge" >/dev/null)
cp "$root/Cargo.lock" "$app/Cargo.lock"
(cd "$app" && "$forge" generate scaffold post title:string body:text --tenant-scoped >"$work/scaffold.out")

# The generators never edit your code; apply what the scaffold prints, once,
# as the tests of forge-cli do. 1. The module declarations: `FILE pub mod x;`.
awk '/^  1\. Declare/ { on = 1; next } /^  2\. In src\/bootstrap/ { on = 0 } on && / pub mod / { print $1, $4 }' "$work/scaffold.out" >"$work/declarations.txt"
[ -s "$work/declarations.txt" ] || { echo "bench.sh: the scaffold printed no module declarations" >&2; exit 1; }
while read -r file module; do
	# the sorted mod block is cosmetic: a declaration at the end of the file compiles the same
	printf 'pub mod %s\n' "$module" >>"$app/$file"
done <"$work/declarations.txt"
# 2. The statements for the end of `fn compose` (the lines indented four
# spaces after the instruction, up to the first line that is not).
awk '/^  2\. In src\/bootstrap\/mod\.rs/ { on = 1; next } on && /^    / { print; started = 1; next } on && started { exit }' "$work/scaffold.out" >"$work/wiring.rs.txt"
[ -s "$work/wiring.rs.txt" ] || { echo "bench.sh: the scaffold printed no wiring" >&2; exit 1; }
bootstrap="$app/src/bootstrap/mod.rs"
# The function the scaffold names (`fn compose`), and its closing `Ok(())`.
function="$(sed -n 's/^  2\. In src\/bootstrap\/mod\.rs, at the end of `fn \([a-z_]*\)`.*$/\1/p' "$work/scaffold.out" | head -n 1)"
[ -n "$function" ] || { echo "bench.sh: the scaffold does not name the function to wire" >&2; exit 1; }
end="$(awk -v name="$function" '$0 ~ "^(pub )?fn " name "\\(" { on = 1 } on && /^    Ok\(\(\)\)$/ { print NR; exit }' "$bootstrap")"
[ -n "$end" ] || { echo "bench.sh: bootstrap/mod.rs changed: no 'Ok(())' closes fn ${function}" >&2; exit 1; }
{ head -n $((end - 1)) "$bootstrap"; cat "$work/wiring.rs.txt"; tail -n +"$end" "$bootstrap"; } >"$bootstrap.new"
mv "$bootstrap.new" "$bootstrap"
# The scaffold's index shows PAGE_SIZE rows (25) and takes no limit: make the
# page the BENCH_ROWS rows the report claims.
sed -i -E "s/^pub const PAGE_SIZE: u16 = [0-9]+;/pub const PAGE_SIZE: u16 = ${rows};/" "$app/src/application/use_cases/posts.rs"
grep -q "PAGE_SIZE: u16 = ${rows};" "$app/src/application/use_cases/posts.rs" || { echo "bench.sh: posts.rs changed: PAGE_SIZE not found" >&2; exit 1; }

echo "building the application (release)" >&2
(cd "$app" && cargo build --release --quiet)
server="$target/release/benchapp"

echo "creating the database ${database} with the non-superuser roles ${migrator} and ${runtime}" >&2
# The statements go on stdin (printf is a builtin), so the passwords never
# appear in psql's arguments, which ps shows.
# Set first: the cleanup drops the roles IF EXISTS, so a failure between the
# two CREATE ROLE statements leaks neither.
created_roles=1
printf "CREATE ROLE %s LOGIN PASSWORD '%s' NOSUPERUSER NOBYPASSRLS CREATEDB;\nCREATE ROLE %s LOGIN PASSWORD '%s' NOSUPERUSER NOCREATEDB NOBYPASSRLS;\n" \
	"$migrator" "$migrator_password" "$runtime" "$runtime_password" |
	psql "$admin" -qAt -v ON_ERROR_STOP=1 >/dev/null
psql "$admin" -qAt -v ON_ERROR_STOP=1 -c "CREATE DATABASE ${database} OWNER ${migrator}" >/dev/null
created_database=1
# Same host and port as the admin URL, the new roles and database.
authority="$(printf '%s' "$admin" | sed -E 's#^[a-z]+://([^@/]*@)?([^/?]+).*$#\2#')"
admin_on_database="$(printf '%s' "$admin" | sed -E "s#^([a-z]+://[^/?]+)(/[^?]*)?#\\1/${database}#")"
# Modelled on the generated compose file (docker/postgres/init.sql), but
# broader (docs/benchmarks.md): the runtime role may use the schema and, by
# default privileges, read and write the tables the migration role creates; it owns nothing and cannot
# create anything. Row-level security applies to both (it is forced).
psql "$admin_on_database" -qAt -v ON_ERROR_STOP=1 >/dev/null <<SQL
REVOKE CONNECT, TEMPORARY ON DATABASE ${database} FROM PUBLIC;
GRANT CONNECT ON DATABASE ${database} TO ${migrator}, ${runtime};
REVOKE ALL ON SCHEMA public FROM PUBLIC;
GRANT ALL ON SCHEMA public TO ${migrator};
GRANT USAGE ON SCHEMA public TO ${runtime};
ALTER DEFAULT PRIVILEGES FOR ROLE ${migrator} IN SCHEMA public GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO ${runtime};
SQL
# The URLs reach the application through its environment, never its arguments.
# Migrations and `setup` use the migration credential; the server only the runtime one.
migration_url="postgres://${migrator}:${migrator_password}@${authority}/${database}"
runtime_url="postgres://${runtime}:${runtime_password}@${authority}/${database}"

FORGE_MIGRATION_DATABASE_URL="$migration_url" "$server" migrate >"$work/migrate.log" 2>&1 || { cat "$work/migrate.log" >&2; exit 1; }
# `setup` reads the owner's password from standard input, never an argument.
printf '%s\n' "$owner_password" |
	FORGE_MIGRATION_DATABASE_URL="$migration_url" "$server" setup --tenant bench --email bench@example.com --organization "Bench" --password-stdin >"$work/setup.log" 2>&1 || { cat "$work/setup.log" >&2; exit 1; }
tenant_id="$(sed -n 's/^Created tenant bench (\([^)]*\)).*$/\1/p' "$work/setup.log" | head -n 1)"
[ -n "$tenant_id" ] || { echo "bench.sh: setup printed no tenant id" >&2; cat "$work/setup.log" >&2; exit 1; }

echo "starting the application on 127.0.0.1:${port}" >&2
FORGE_DATABASE_URL="$runtime_url" FORGE_BIND="127.0.0.1:${port}" "$server" >"$work/server.log" 2>&1 &
server_pid=$!

meta_cpu="unknown"
if [ -r /proc/cpuinfo ]; then
	meta_cpu="$(grep -m1 'model name' /proc/cpuinfo | sed 's/^[^:]*: *//')"
elif command -v sysctl >/dev/null; then
	meta_cpu="$(sysctl -n machdep.cpu.brand_string 2>/dev/null || echo unknown)"
fi
cpus="$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo '?')"
commit="$(git -C "$root" rev-parse --short HEAD 2>/dev/null || echo unknown)"
dirty=""
if [ -n "$(git -C "$root" status --porcelain 2>/dev/null)" ]; then dirty="+dirty"; fi
echo "Forge-Rust ${commit}${dirty}, $(rustc --version | cut -d' ' -f1-2), $(uname -sr), ${cpus} x ${meta_cpu}"
echo "$(psql "$admin" -qAtc 'select version()' | cut -d, -f1), date $(date -u +%Y-%m-%dT%H:%MZ)"
echo

BENCH_PASSWORD="$owner_password" "$loadgen" \
	-base "http://127.0.0.1:${port}" -tenant bench -tenant-id "$tenant_id" -email bench@example.com \
	-concurrency "$concurrency" -duration "$duration" -warmup "$warmup" -runs "$runs" -rows "$rows" "$@"
