#!/usr/bin/env bash
# fetch-upstream.sh TAG
#   Pull one proximos-v2 tag into upstream/TAG/: the six cross-built Hub
#   binaries from the GitLab generic package registry (verified against the
#   registry's SHA256SUMS) under bin/, and a sparse checkout of the packaging
#   inputs under src/. Writes upstream.lock last.
#
# Environment:
#   REGISTRY_BASE      default https://gitlab.hardwario.com/api/v4/projects/705/packages/generic
#   GITLAB_TOKEN_FILE  default .secrets/gitlab-token; must be mode 0600; sent as
#                      a curl header file, never on the command line
#   UPSTREAM_GIT       default git@gitlab.hardwario.com:proximos/proximos-v2.git
#   FIBER_UPSTREAM_DIR default <repo>/upstream;  FIBER_LOCK default <repo>/upstream.lock
#
# Exit: 0 ok, 2 usage, 3 download failed, 4 sha256 mismatch, 5 checkout failed.
# On 3, 4 or 5 nothing is left under upstream/TAG/.
set -euo pipefail

REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=../lib/fiber/common.sh
source "${REPO_ROOT}/lib/fiber/common.sh"

PACKAGE=proximos-portal-aarch64
BINARIES=(portal fiber_app fiberctl fiber-factory-reset proximos-control proximosctl)
SPARSE_PATHS=(
    portal/packaging portal/templates portal/static
    portal/crates/control-core/fiber.config.yaml
    portal/crates/control-core/fiber.sensors.config.yaml
    portal/crates/control-core/authorized_signers.yaml
    portal/crates/control-core/scripts
    system/meta-proximos
    firmware/fiber-southbridge/tools firmware/fiber-northbridge/tools
)

REGISTRY_BASE="${REGISTRY_BASE:-https://gitlab.hardwario.com/api/v4/projects/705/packages/generic}"
GITLAB_TOKEN_FILE="${GITLAB_TOKEN_FILE:-${REPO_ROOT}/.secrets/gitlab-token}"
UPSTREAM_GIT="${UPSTREAM_GIT:-git@gitlab.hardwario.com:proximos/proximos-v2.git}"
UPSTREAM_DIR="${FIBER_UPSTREAM_DIR:-${REPO_ROOT}/upstream}"
LOCK="${FIBER_LOCK:-${REPO_ROOT}/upstream.lock}"

usage() { echo "usage: $0 TAG   (e.g. v5.0.0)" >&2; exit 2; }
[ $# -eq 1 ] || usage
TAG="$1"
case "$TAG" in v[0-9]*) ;; *) usage ;; esac

[ -f "$GITLAB_TOKEN_FILE" ] || { fiber_warn "token file $GITLAB_TOKEN_FILE is missing (a GitLab read_api token, one line)"; exit 2; }
mode=$(stat -c '%a' "$GITLAB_TOKEN_FILE")
[ "$mode" = "600" ] || { fiber_warn "token file $GITLAB_TOKEN_FILE must be mode 0600, is $mode"; exit 2; }

DEST="${UPSTREAM_DIR}/${TAG}"
BIN="${DEST}/bin"
SRC="${DEST}/src"
WORK="$(mktemp -d)"
cleanup_fail() { rm -rf "$DEST" "$WORK"; }

printf 'PRIVATE-TOKEN: %s\n' "$(head -n 1 "$GITLAB_TOKEN_FILE")" > "${WORK}/auth"
chmod 0600 "${WORK}/auth"

rm -rf "$DEST"
mkdir -p "$BIN"

fetch() { # name
    local url="${REGISTRY_BASE}/${PACKAGE}/${TAG}/$1"
    if ! curl -fsSL --header "@${WORK}/auth" -o "${BIN}/$1" "$url"; then
        fiber_warn "download failed: $url"
        cleanup_fail; exit 3
    fi
}

fiber_log "fetching ${PACKAGE}/${TAG} from ${REGISTRY_BASE}"
fetch SHA256SUMS
for b in "${BINARIES[@]}"; do fetch "$b"; done
curl -fsSL --header "@${WORK}/auth" -o "${BIN}/build-info.txt" \
    "${REGISTRY_BASE}/${PACKAGE}/${TAG}/build-info.txt" || fiber_warn "no build-info.txt in the registry (not fatal)"

if ! (cd "$BIN" && sha256sum --quiet -c SHA256SUMS); then
    fiber_warn "sha256 mismatch against ${PACKAGE}/${TAG}/SHA256SUMS: the files above are not what the build hashed"
    cleanup_fail; exit 4
fi
chmod 0755 "${BIN:?}"/*
chmod 0644 "${BIN}/SHA256SUMS"

fiber_log "checking out proximos-v2 ${TAG} (sparse) from ${UPSTREAM_GIT}"
if ! git clone -q --filter=blob:none --no-checkout --depth 1 --branch "$TAG" "$UPSTREAM_GIT" "$SRC" 2>"${WORK}/git.log" \
   || ! git -C "$SRC" sparse-checkout set --no-cone "${SPARSE_PATHS[@]}" 2>>"${WORK}/git.log" \
   || ! git -C "$SRC" checkout -q "$TAG" 2>>"${WORK}/git.log"; then
    grep -v 'filtering not recognized' "${WORK}/git.log" >&2 || true
    fiber_warn "checkout of ${TAG} failed"
    cleanup_fail; exit 5
fi
[ -f "${SRC}/portal/packaging/fiber-portal.service" ] || { fiber_warn "checkout has no portal/packaging: wrong tag or wrong repository"; cleanup_fail; exit 5; }

{
    printf 'tag: %s\n' "$TAG"
    printf 'registry: %s/%s/%s\n' "$REGISTRY_BASE" "$PACKAGE" "$TAG"
    printf 'commit: %s\n' "$(git -C "$SRC" rev-parse HEAD)"
    for b in "${BINARIES[@]}"; do
        printf '%s  bin/%s\n' "$(sha256sum "${BIN}/$b" | cut -d' ' -f1)" "$b"
    done
} > "$LOCK"
rm -rf "$WORK"
fiber_log "upstream/${TAG} ready; lock written to ${LOCK}"
