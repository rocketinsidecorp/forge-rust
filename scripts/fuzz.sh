#!/usr/bin/env bash
# Runs every cargo-fuzz target of fuzz/, one after another, for a fixed time
# each. Targets are discovered from the [[bin]] entries of fuzz/Cargo.toml, so
# a new target is fuzzed in CI without editing a list (listing needs neither
# cargo-fuzz nor the nightly toolchain).
# Same interface as Forge-Go's scripts/fuzz.sh.
#
#   scripts/fuzz.sh                        # every target, 10s each
#   scripts/fuzz.sh -t 2m config           # the targets whose name matches config, 2 minutes each
#   scripts/fuzz.sh -t 5m -r '^proxy_duration$'
#   scripts/fuzz.sh -l                     # list the targets
#   scripts/fuzz.sh -j                     # the same, as JSON for a CI matrix
#   scripts/fuzz.sh -c DIR                 # copy new failing inputs into DIR
#   scripts/fuzz.sh -T                     # print the nightly toolchain it uses
#
# cargo-fuzz needs a nightly compiler and a C++ toolchain (libFuzzer is built
# from source). The nightly is pinned below so a fuzzing run is reproducible;
# change the date by hand (docs/supply-chain.md). Install the tools once:
#
#   rustup toolchain install "$(scripts/fuzz.sh -T)" --profile minimal
#   cargo install cargo-fuzz --version 0.13.2 --locked
#
# Each target reads its committed regression seeds (fuzz/seeds/<target>/) and
# writes what it learns to fuzz/corpus/<target>/ (git-ignored; CI caches it
# between nightly runs). A failing input is written by libFuzzer to
# fuzz/artifacts/<target>/. It is a real bug: fix it and move the input to
# fuzz/seeds/<target>/ with the fix, where it is replayed by every run. The
# script fuzzes every target even after a failure and exits 1 if any failed.
set -euo pipefail

# The nightly the fuzz targets are built with (see scripts/fuzz.sh -T).
toolchain="${FUZZ_TOOLCHAIN:-nightly-2026-10-19}"

usage() {
	echo "usage: scripts/fuzz.sh [-t duration] [-r target-regexp] [-l | -j | -c dir | -T] [target-name-fragment ...]" >&2
	exit 2
}

root="$(cd "$(dirname "$0")/.." && pwd)"
fuzztime="${FUZZTIME:-10s}"
filter='.'
mode=run
while getopts 't:r:ljc:Th' option; do
	case "$option" in
	t) fuzztime="$OPTARG" ;;
	r) filter="$OPTARG" ;;
	l) mode=list ;;
	j) mode=json ;;
	c)
		mode=collect
		collect_dir="$OPTARG"
		;;
	T)
		echo "$toolchain"
		exit 0
		;;
	*) usage ;;
	esac
done
shift $((OPTIND - 1))

# collect copies the failing inputs libFuzzer wrote (crash-*, oom-*,
# timeout-*) into a directory, keeping their paths, for a CI artifact.
if [ "$mode" = collect ]; then
	mkdir -p "$collect_dir"
	count=0
	while IFS= read -r -d '' file; do
		relative="${file#"$root"/}"
		mkdir -p "$collect_dir/$(dirname "$relative")"
		cp "$file" "$collect_dir/$relative"
		echo "failing input: $relative"
		count=$((count + 1))
	done < <(find "$root/fuzz/artifacts" -type f -print0 2>/dev/null)
	echo "collected $count failing inputs into $collect_dir"
	exit 0
fi

# A duration (10s, 5m, 1h, as Forge-Go's takes it) or an iteration count
# (1000x), turned into the libFuzzer limit.
if ! [[ "$fuzztime" =~ ^([0-9]+)(s|m|h|x)$ ]]; then
	echo "fuzz.sh: invalid duration $fuzztime (10s, 5m, 1h or 1000x)" >&2
	exit 2
fi
amount="${BASH_REMATCH[1]}"
case "${BASH_REMATCH[2]}" in
s) limit="-max_total_time=$amount" ;;
m) limit="-max_total_time=$((amount * 60))" ;;
h) limit="-max_total_time=$((amount * 3600))" ;;
x) limit="-runs=$amount" ;;
esac

# bins prints the name of every [[bin]] table of fuzz/Cargo.toml.
bins() {
	awk '
		/^\[\[bin\]\]/ { in_bin = 1; next }
		/^\[/ { in_bin = 0 }
		in_bin && /^name[ \t]*=/ { gsub(/^name[ \t]*=[ \t]*"|"[ \t]*$/, ""); print }
	' "$root/fuzz/Cargo.toml"
}

# discover prints the target names that match the filter and the arguments.
discover() {
	local name fragment keep
	while IFS= read -r name; do
		[[ "$name" =~ $filter ]] || continue
		keep=1
		if [ "$#" -gt 0 ]; then
			keep=0
			for fragment in "$@"; do
				[[ "$name" == *"$fragment"* ]] && keep=1
			done
		fi
		[ "$keep" -eq 1 ] && echo "$name"
	done < <(bins)
	return 0
}

targets="$(discover "$@")"
case "$mode" in
list)
	printf '%s\n' "$targets"
	exit 0
	;;
json)
	# id names a target uniquely in artifact and cache names.
	printf '%s\n' "$targets" | awk '
		BEGIN { printf "[" }
		NF == 1 { printf "%s{\"target\":\"%s\",\"id\":\"%s\"}", (n++ ? "," : ""), $1, $1 }
		END { print "]" }
	'
	exit 0
	;;
esac

if [ -z "$targets" ]; then
	echo "fuzz.sh: no fuzz targets found" >&2
	exit 1
fi

if ! command -v cargo-fuzz >/dev/null 2>&1; then
	echo "fuzz.sh: cargo-fuzz is not installed: cargo install cargo-fuzz --version 0.13.2 --locked" >&2
	exit 1
fi
# --fuzz-dir must precede the `--` that hands the rest to libFuzzer.
cargo_fuzz() {
	local subcommand="$1"
	shift
	cargo "+$toolchain" fuzz "$subcommand" --fuzz-dir "$root/fuzz" "$@"
}

failed=()
total=0
while read -r target; do
	total=$((total + 1))
	echo "=== fuzz $target ($fuzztime)"
	mkdir -p "$root/fuzz/corpus/$target" "$root/fuzz/seeds/$target"
	# The first corpus directory receives new inputs; the seeds are only read.
	# -rss_limit_mb and -timeout turn a runaway input into a failing one.
	if ! cargo_fuzz run "$target" "$root/fuzz/corpus/$target" "$root/fuzz/seeds/$target" -- \
		"$limit" -rss_limit_mb=2048 -timeout=10; then
		failed+=("$target")
		if [ -n "${GITHUB_ACTIONS:-}" ]; then
			echo "::error title=Fuzzing failed::$target found a failing input; see fuzz/artifacts/$target"
		fi
	fi
done <<<"$targets"

echo "fuzzed $total targets for $fuzztime each; ${#failed[@]} failed"
if [ "${#failed[@]}" -gt 0 ]; then
	printf 'FAIL %s\n' "${failed[@]}"
	exit 1
fi
