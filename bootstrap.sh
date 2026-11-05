#!/usr/bin/env bash
# FIBER Hub installer for stock Raspberry Pi OS Lite 64-bit.
#
#   curl -fsSL https://raw.githubusercontent.com/hardwario/fiber-agent/main/bootstrap.sh | sudo bash
#
# Downloads the latest fiber-agent release (the whole on-device tree: FIBER
# portal and agent, ChirpStack, STM32 firmware, configuration), checks its
# sha256, unpacks it into /, runs `fiber setup` and reboots. Re-running it
# upgrades to the latest release.
#
# Environment (for testing and pinning):
#   FIBER_RELEASE_URL  where fiber-agent-arm64.tar.gz{,.sha256} live
#                      (default: the latest GitHub release)
#   FIBER_NO_REBOOT=1  do not reboot at the end
set -euo pipefail

RELEASE_URL="${FIBER_RELEASE_URL:-https://github.com/hardwario/fiber-agent/releases/latest/download}"
ROOT="${FIBER_INSTALL_ROOT:-/}"
SETUP_CMD="${FIBER_SETUP_CMD:-/usr/bin/fiber setup}"
ARCH="${FIBER_ARCH:-$(uname -m)}"
ASSET=fiber-agent-arm64.tar.gz

say() { printf '\033[1mfiber:\033[0m %s\n' "$*"; }
die() { printf 'fiber: %s\n' "$*" >&2; exit 1; }

[ "$(id -u)" -eq 0 ] || [ -n "${FIBER_ALLOW_NONROOT:-}" ] || die "run it as root: curl -fsSL <url>/bootstrap.sh | sudo bash"
[ "$ARCH" = aarch64 ] || die "this is a $ARCH machine; the FIBER Hub needs Raspberry Pi OS 64-bit (aarch64)"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

say "downloading ${RELEASE_URL}/${ASSET}"
curl -fsSL -o "${WORK}/${ASSET}" "${RELEASE_URL}/${ASSET}" || die "download failed: ${RELEASE_URL}/${ASSET}"
curl -fsSL -o "${WORK}/${ASSET}.sha256" "${RELEASE_URL}/${ASSET}.sha256" || die "download failed: ${ASSET}.sha256"
(cd "$WORK" && sha256sum --quiet -c "${ASSET}.sha256") || die "sha256 mismatch: the download is corrupt or not the published release; nothing was installed"

say "installing into ${ROOT}"
# --no-overwrite-dir keeps the owner and mode of directories that exist (/etc, /usr, ...).
tar -xzf "${WORK}/${ASSET}" -C "$ROOT" --no-overwrite-dir --no-same-owner
[ -f "${ROOT%/}/usr/share/fiber/release" ] && say "installed fiber-agent $(sed -n 's/^version: //p' "${ROOT%/}/usr/share/fiber/release")"

say "running fiber setup (installs packages, configures the Hub)"
$SETUP_CMD

if [ -n "${FIBER_NO_REBOOT:-}" ]; then
    say "done; reboot to start the FIBER Hub"
else
    say "done; rebooting in 5 s (the Hub comes back as fiber-<serial>.local)"
    sleep 5
    systemctl reboot
fi
