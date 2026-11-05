#!/usr/bin/env bash
# make-release.sh ROOT DIST VERSION
#   Pack a staged on-device tree (build/root, from `make stage`) into
#   DIST/fiber-agent-arm64.tar.gz (root:root, the layout bootstrap.sh unpacks
#   into /) and its .sha256. VERSION (vX.Y.Z, the GitHub release tag) is
#   recorded in usr/share/fiber/release, which `fiber version` prints. Upload
#   both files to the GitHub release of that tag.
set -euo pipefail

REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=../lib/fiber/common.sh
source "${REPO_ROOT}/lib/fiber/common.sh"

[ $# -eq 3 ] || { echo "usage: $0 ROOT DIST VERSION   (e.g. build/root dist v0.1.3)" >&2; exit 2; }
ROOT="$1"; DIST="$2"; VERSION="$3"
ASSET=fiber-agent-arm64.tar.gz
case "$VERSION" in
    v[0-9]*.[0-9]*.[0-9]*) ;;
    *) fiber_die "version '$VERSION' is not vX.Y.Z (the GitHub release tag)" ;;
esac

[ -x "${ROOT}/usr/bin/fiber-portal" ] || [ -f "${ROOT}/usr/bin/fiber-portal" ] || fiber_die "${ROOT} has no usr/bin/fiber-portal; run 'make stage' first"
mkdir -p "$DIST" "${ROOT}/usr/share/fiber"
lock="${ROOT}/usr/share/fiber/upstream.lock"
hub_tag=$(sed -n 's/^tag: //p' "$lock" 2>/dev/null || true)
hub_commit=$(sed -n 's/^commit: //p' "$lock" 2>/dev/null | cut -c1-8 || true)
{
    printf 'version: %s\n' "$VERSION"
    printf 'agent: %s\n' "$(git -C "$REPO_ROOT" rev-parse --short=7 HEAD 2>/dev/null || echo unknown)"
    printf 'hub: %s %s\n' "${hub_tag:-unknown}" "${hub_commit:-unknown}"
    printf 'built: %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
} > "${ROOT}/usr/share/fiber/release"
chmod 0644 "${ROOT}/usr/share/fiber/release"
tar -C "$ROOT" --owner=0 --group=0 --numeric-owner --sort=name -czf "${DIST}/${ASSET}" .
(cd "$DIST" && sha256sum "$ASSET" > "${ASSET}.sha256")
fiber_log "$(du -h "${DIST}/${ASSET}" | cut -f1) ${DIST}/${ASSET}"
fiber_log "release ${VERSION}: $(tr '\n' ' ' < "${ROOT}/usr/share/fiber/release")"
cat "${DIST}/${ASSET}.sha256"
