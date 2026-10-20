#!/usr/bin/env bash
# Builds the forge CLI and forge-proxy the way a release does (locked,
# optimized) with cargo-auditable, which embeds the exact dependency list in
# each binary, and writes a CycloneDX and an SPDX SBOM of each binary, read by
# Syft from that embedded list. A third pair describes the workspace's
# Cargo.lock. Same tool, pinned image and output style as Forge-Go's
# scripts/sbom.sh, with SPDX added.
#
#   scripts/sbom.sh dist/sbom
#   SYFT=/usr/local/bin/syft scripts/sbom.sh dist/sbom   # a local Syft, no Docker
#   SBOM_LOCK_ONLY=1 scripts/sbom.sh dist/sbom           # only the Cargo.lock SBOM, no build
#
# Needs cargo-auditable (cargo install cargo-auditable --version 0.7.7
# --locked) unless SBOM_LOCK_ONLY is set. Syft runs from its image, pinned by
# digest, without network access. The output directory gets, for each of
# forge, forge-proxy and workspace, NAME.cdx.json and NAME.spdx.json.
set -euo pipefail

# ghcr.io/anchore/syft v1.54.1 (2026-10-06), the multi-architecture index.
syft_image="ghcr.io/anchore/syft:v1.54.1@sha256:3eb5379ba7b409c3f4069b686110527af0c47df993fa5c10d13e7cf34f49b1aa"

out="${1:?usage: scripts/sbom.sh OUTPUT_DIR}"
mkdir -p "$out"
out="$(cd "$out" && pwd)"
root="$(cd "$(dirname "$0")/.." && pwd)"
scratch="$(mktemp -d)"
trap 'rm -rf "$scratch"' EXIT

version="$(git -C "$root" describe --tags --always --dirty 2>/dev/null || echo dev)"

# syft_scan KIND PATH NAME: writes NAME.cdx.json and NAME.spdx.json for the
# file or directory PATH of $scratch (the Syft image sees $scratch as /in).
# KIND is Syft's source scheme, file or dir.
syft_scan() {
	local kind="$1" path="$2" name="$3"
	if [ -n "${SYFT:-}" ]; then
		SYFT_CHECK_FOR_APP_UPDATE=false "$SYFT" scan "$kind:$scratch/$path" \
			--source-name "$name" --source-version "$version" \
			-o "cyclonedx-json=$out/$name.cdx.json" -o "spdx-json=$out/$name.spdx.json"
	else
		docker run --rm --network none -e SYFT_CHECK_FOR_APP_UPDATE=false \
			-v "$scratch:/in:ro" -v "$out:/out" "$syft_image" \
			scan "$kind:/in/$path" --source-name "$name" --source-version "$version" \
			-o "cyclonedx-json=/out/$name.cdx.json" -o "spdx-json=/out/$name.spdx.json"
	fi
	echo "wrote $out/$name.cdx.json and $out/$name.spdx.json"
}

# The workspace: what Cargo.lock pins, for every package and feature.
mkdir -p "$scratch/workspace"
cp "$root/Cargo.lock" "$scratch/workspace/Cargo.lock"
syft_scan dir workspace workspace

if [ -z "${SBOM_LOCK_ONLY:-}" ]; then
	if ! cargo auditable --version >/dev/null 2>&1; then
		echo "sbom.sh: cargo-auditable is not installed: cargo install cargo-auditable --version 0.7.7 --locked" >&2
		exit 1
	fi
	target_dir="${CARGO_TARGET_DIR:-$root/target}"
	(cd "$root" && cargo auditable build --release --locked -p forge-cli --bin forge -p forge-proxy --bin forge-proxy)
	mkdir -p "$scratch/bin"
	for name in forge forge-proxy; do
		cp "$target_dir/release/$name" "$scratch/bin/$name"
		syft_scan file "bin/$name" "$name"
	done
fi
