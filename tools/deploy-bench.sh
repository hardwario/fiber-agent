#!/usr/bin/env bash
# deploy-bench.sh HUB
#   Copy build/root/ onto a bench Hub (stock Raspberry Pi OS Lite 64-bit,
#   Bookworm, an Imager user with sudo) and run `fiber setup` there.
#
#   HUB is an ssh destination: user@host, a Host from ~/.ssh/config, …
#   The tree is sent with rsync as root on the far side, owned root:root.
set -euo pipefail

REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=../lib/fiber/common.sh
source "${REPO_ROOT}/lib/fiber/common.sh"

[ $# -eq 1 ] || { echo "usage: $0 HUB   (e.g. pi@fiber-1a2b3c4d.local)" >&2; exit 2; }
HUB="$1"
ROOT="${REPO_ROOT}/build/root"

[ -x "${ROOT}/usr/bin/fiber-portal" ] || fiber_die "deploy: ${ROOT}/usr/bin/fiber-portal is missing; run 'make stage' first"
[ -x "${ROOT}/usr/bin/fiber" ] || fiber_die "deploy: ${ROOT}/usr/bin/fiber is missing; run 'make stage' first"

# Two stages: rsync as the ssh user into a staging directory (no sudo over the
# rsync protocol, so sudo may ask for a password), then copy into / as root.
fiber_log "copying $(find "$ROOT" -type f | wc -l) files to ${HUB}:~/fiber-stage/"
rsync -rlpt --delete "${ROOT}/" "${HUB}:fiber-stage/"

fiber_log "installing into / and running 'fiber setup' on ${HUB} (sudo may ask for your password)"
ssh -t "$HUB" 'sudo rsync -rlpt --chown=root:root ~/fiber-stage/ / && sudo fiber setup'

fiber_log "done. Reboot the Hub (ssh ${HUB} sudo reboot), then: ssh ${HUB} sudo fiber doctor"
