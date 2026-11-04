#!/usr/bin/env bash
# make-release.sh [ROOT] [DIST]
#   Pack a staged on-device tree (default build/root, from `make stage`) into
#   DIST/fiber-agent-arm64.tar.gz (root:root, the layout bootstrap.sh unpacks
#   into /) and its .sha256. Upload both to a GitHub release.
set -euo pipefail

REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=../lib/fiber/common.sh
source "${REPO_ROOT}/lib/fiber/common.sh"

ROOT="${1:-${REPO_ROOT}/build/root}"
DIST="${2:-${REPO_ROOT}/dist}"
ASSET=fiber-agent-arm64.tar.gz

[ -x "${ROOT}/usr/bin/fiber-portal" ] || [ -f "${ROOT}/usr/bin/fiber-portal" ] || fiber_die "${ROOT} has no usr/bin/fiber-portal; run 'make stage' first"
mkdir -p "$DIST"
tar -C "$ROOT" --owner=0 --group=0 --numeric-owner --sort=name -czf "${DIST}/${ASSET}" .
(cd "$DIST" && sha256sum "$ASSET" > "${ASSET}.sha256")
fiber_log "$(du -h "${DIST}/${ASSET}" | cut -f1) ${DIST}/${ASSET}"
[ -f "${ROOT}/usr/share/fiber/upstream.lock" ] && fiber_log "version: $(sed -n 's/^tag: //p' "${ROOT}/usr/share/fiber/upstream.lock")"
cat "${DIST}/${ASSET}.sha256"
