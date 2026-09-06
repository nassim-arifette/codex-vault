#!/usr/bin/env bash
set -euo pipefail
if [[ $# -gt 1 || (${1:-} != '' && ${1:-} != '--skip-build') ]]; then
    echo 'Usage: bash scripts/package-linux.sh [--skip-build]' >&2
    exit 2
fi
if [[ $(uname -sm) != 'Linux x86_64' ]]; then
    echo 'Build this package on Linux x86_64 (including WSL2).' >&2
    exit 1
fi
workspace=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$workspace"
target=x86_64-unknown-linux-musl
if [[ ${1:-} != '--skip-build' ]]; then
    cargo build --locked --release --target "$target"
fi
version=$(cargo metadata --offline --no-deps --format-version 1 | python3 -c 'import json,sys; print(next(p["version"] for p in json.load(sys.stdin)["packages"] if p["name"] == "codex-vault"))')
binary="${CARGO_TARGET_DIR:-target}/$target/release/codex-vault"
[[ $("$binary" --version) == "codex-vault $version" ]] || { echo 'Executable version mismatch' >&2; exit 1; }
# Reject a package that would silently depend on the build machine's dynamic runtime.
program_headers=$(readelf --program-headers "$binary")
dynamic_entries=$(readelf --dynamic "$binary")
if [[ $program_headers == *INTERP* || $dynamic_entries == *NEEDED* ]]; then
    echo 'The Linux release must be statically linked.' >&2
    exit 1
fi
release_root="$workspace/dist/release"
mkdir -p "$release_root"
stage=$(mktemp -d "$release_root/stage-linux.XXXXXX")
trap 'rm -rf -- "$stage"' EXIT
install -m 755 "$binary" "$stage/codex-vault"
install -m 755 scripts/install-linux.sh "$stage/install.sh"
cp LICENSE README.md "$stage/"
cp -R docs "$stage/docs"
(cd "$stage" && sha256sum --binary codex-vault > SHA256SUMS.txt)
archive="codex-vault-$version-linux-x86_64.tar.gz"
tar --owner=0 --group=0 --numeric-owner -czf "$release_root/$archive" -C "$stage" .
(cd "$release_root" && sha256sum --binary "$archive" > SHA256SUMS-linux.txt)
printf '%s\n' "$release_root/$archive"
