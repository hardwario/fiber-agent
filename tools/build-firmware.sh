#!/usr/bin/env bash
# build-firmware.sh PROXIMOS_V2_CHECKOUT [OUT_DIR]
#   Build the Northbridge (with nbboot) and Southbridge firmware exactly as
#   proximos-v2's CI jobs firmware:north-build and firmware:south-build do, in
#   CI's container, from a throwaway clone of the checkout's HEAD. Writes
#   OUT_DIR/nb-package/ and OUT_DIR/sb-package/ (default build/firmware).
#   Needs docker; the image is ~20 GB on first pull, the west workspace ~5 GB.
set -euo pipefail

REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=../lib/fiber/common.sh
source "${REPO_ROOT}/lib/fiber/common.sh"

[ $# -ge 1 ] || { echo "usage: $0 PROXIMOS_V2_CHECKOUT [OUT_DIR]" >&2; exit 2; }
SRC="$(cd "$1" && pwd)"
OUT="${2:-${REPO_ROOT}/build/firmware}"
IMAGE="${ZEPHYR_CI_IMAGE:-zephyrprojectrtos/ci:v0.29.0}"
WORK="${REPO_ROOT}/build/fw-work"

rm -rf "$WORK"
git clone -q "$SRC" "$WORK"
cat > "$WORK/build-fw.sh" <<'INNER'
#!/bin/bash
set -euo pipefail
git config --global --add safe.directory '*'
cd /w/firmware
west init -l fiber-northbridge
west update --narrow -o=--depth=1
west build -b stm32wl5moc -d build-north -p always fiber-northbridge/app
../system/scripts/nb-package.sh build-north nb-package
west config manifest.path fiber-southbridge
west update --narrow -o=--depth=1
west build -b fiber_southbridge -d build-south -p always fiber-southbridge/app
../system/scripts/sb-package.sh build-south sb-package
INNER
chmod +x "$WORK/build-fw.sh"

fiber_log "building firmware of $(git -C "$SRC" rev-parse --short HEAD) in ${IMAGE}"
docker run --rm -u "$(id -u):$(id -g)" -e HOME=/tmp \
    -e ZEPHYR_TOOLCHAIN_VARIANT=zephyr -e ZEPHYR_SDK_INSTALL_DIR=/opt/toolchains/zephyr-sdk-1.0.0 \
    -v "$WORK:/w" -w /w "$IMAGE" ./build-fw.sh

rm -rf "$OUT"; mkdir -p "$OUT"
cp -r "$WORK/firmware/nb-package" "$WORK/firmware/sb-package" "$OUT/"
rm -rf "$WORK"
fiber_log "firmware ready in ${OUT}"
cat "$OUT/nb-package/northbridge.json"; echo
cat "$OUT/sb-package/southbridge.json"; echo
