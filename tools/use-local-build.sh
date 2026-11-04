#!/usr/bin/env bash
# use-local-build.sh PROXIMOS_V2_CHECKOUT TAG
#   Use a local `make cross` of proximos-v2 instead of the package registry:
#   upstream/TAG/bin gets the six aarch64 binaries (checked against their
#   SHA256SUMS), upstream/TAG/src links the checkout, upstream.lock names TAG.
#   Build first with:  (cd CHECKOUT/portal && make cross PROXIMOS_BRAND=fiber)
set -euo pipefail

REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=../lib/fiber/common.sh
source "${REPO_ROOT}/lib/fiber/common.sh"

[ $# -eq 2 ] || { echo "usage: $0 PROXIMOS_V2_CHECKOUT TAG   (e.g. ../proximos-v2 vlocal)" >&2; exit 2; }
SRC="$(cd "$1" && pwd)"; TAG="$2"
REL="${SRC}/portal/target/aarch64-unknown-linux-gnu/release"
DEST="${FIBER_UPSTREAM_DIR:-${REPO_ROOT}/upstream}/${TAG}"
LOCK="${FIBER_LOCK:-${REPO_ROOT}/upstream.lock}"

[ -f "${REL}/SHA256SUMS" ] || fiber_die "no ${REL}/SHA256SUMS: run 'make cross PROXIMOS_BRAND=fiber' in ${SRC}/portal first"
rm -rf "$DEST"; mkdir -p "${DEST}/bin"
for f in portal fiber_app fiberctl fiber-factory-reset proximos-control proximosctl SHA256SUMS; do
    cp "${REL}/${f}" "${DEST}/bin/"
done
(cd "${DEST}/bin" && sha256sum --quiet -c SHA256SUMS) || fiber_die "binaries do not match their SHA256SUMS"
ln -s "$SRC" "${DEST}/src"
{
    printf 'tag: %s\n' "$TAG"
    printf 'commit: %s\n' "$(git -C "$SRC" rev-parse HEAD)"
    printf 'source: local make cross of %s\n' "$SRC"
} > "$LOCK"
fiber_log "upstream/${TAG} ready from ${SRC}"
