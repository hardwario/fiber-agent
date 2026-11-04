#!/usr/bin/env bash
# fetch-chirpstack.sh
#   Download the three ChirpStack arm64 tarballs pinned in chirpstack.lock,
#   verify each sha256, and extract the binaries into upstream/chirpstack/.
#
# Environment:
#   CHIRPSTACK_BASE    default https://artifacts.chirpstack.io/downloads
#   CHIRPSTACK_LOCK    default <repo>/chirpstack.lock
#   FIBER_UPSTREAM_DIR default <repo>/upstream
#
# Exit: 0 ok, 3 download failed (prints the URL), 4 sha256 mismatch (prints
# expected and actual). On 3 or 4 upstream/chirpstack/ is removed.
set -euo pipefail

REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=../lib/fiber/common.sh
source "${REPO_ROOT}/lib/fiber/common.sh"

BASE="${CHIRPSTACK_BASE:-https://artifacts.chirpstack.io/downloads}"
LOCK="${CHIRPSTACK_LOCK:-${REPO_ROOT}/chirpstack.lock}"
DEST="${FIBER_UPSTREAM_DIR:-${REPO_ROOT}/upstream}/chirpstack"
WORK="$(mktemp -d)"
cleanup_fail() { rm -rf "$DEST" "$WORK"; }

rm -rf "$DEST"
mkdir -p "$DEST"

while read -r name version rel sha256 binary; do
    case "$name" in ''|'#'*) continue ;; esac
    url="${BASE}/${rel}"
    tarball="${WORK}/${rel##*/}"
    fiber_log "fetching ${name} ${version}"
    if ! curl -fsSL -o "$tarball" "$url"; then
        fiber_warn "download failed: $url"
        cleanup_fail; exit 3
    fi
    actual=$(sha256sum "$tarball" | cut -d' ' -f1)
    if [ "$actual" != "$sha256" ]; then
        fiber_warn "sha256 mismatch for ${name} (${rel}): expected ${sha256}, got ${actual}"
        cleanup_fail; exit 4
    fi
    mkdir -p "${WORK}/x"
    tar -xzf "$tarball" -C "${WORK}/x"
    found=$(find "${WORK}/x" -type f -name "$binary" | head -n 1)
    if [ -z "$found" ]; then
        fiber_warn "${rel} holds no file named ${binary}"
        cleanup_fail; exit 4
    fi
    install -m 0755 "$found" "${DEST}/${binary}"
    rm -rf "${WORK}/x" "$tarball"
done < "$LOCK"

rm -rf "$WORK"
fiber_log "ChirpStack binaries ready under ${DEST}"
