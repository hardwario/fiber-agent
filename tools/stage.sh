#!/usr/bin/env bash
# stage.sh TAG DESTDIR
#   Lay out the on-device tree (the future .deb contents) from upstream/TAG,
#   upstream/chirpstack, build/fiber.dtbo and this repository's own files.
#   DESTDIR is wiped first. The only edits made to upstream files are the
#   two named at the end (hostname prefix, nginx http2 syntax).
#
# Environment: FIBER_UPSTREAM_DIR (default <repo>/upstream), FIBER_LOCK
# (default <repo>/upstream.lock).
set -euo pipefail

REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=../lib/fiber/common.sh
source "${REPO_ROOT}/lib/fiber/common.sh"

[ $# -eq 2 ] || { echo "usage: $0 TAG DESTDIR" >&2; exit 2; }
TAG="$1"; D="$2"
UP="${FIBER_UPSTREAM_DIR:-${REPO_ROOT}/upstream}"
LOCK="${FIBER_LOCK:-${REPO_ROOT}/upstream.lock}"
BIN="${UP}/${TAG}/bin"; U="${UP}/${TAG}/src"; CS="${UP}/chirpstack"
P="${U}/portal/packaging"; M="${U}/system/meta-proximos"; CC="${U}/portal/crates/control-core"
DTBO="${REPO_ROOT}/build/fiber.dtbo"

for need in "$BIN/portal" "$U/portal/packaging/fiber-portal.service" "$CS/chirpstack" "$DTBO" "$LOCK"; do
    [ -e "$need" ] || fiber_die "stage: missing $need (run fetch-upstream.sh, fetch-chirpstack.sh and make dtbo first)"
done

locktag=$(sed -n 's/^tag: //p' "$LOCK")
[ "$locktag" = "$TAG" ] || fiber_die "stage: $LOCK says tag ${locktag:-?}, not ${TAG}; run fetch-upstream.sh ${TAG} first"

case "$D" in /*) ;; *) D="${PWD}/${D}" ;; esac
rm -rf "$D"
mkdir -p "$D"

put() { install -D -m "$1" "$2" "${D}/$3"; }            # MODE SRC DEST
tree() { mkdir -p "${D}/$2"; cp -r "$1/." "${D}/$2/"; } # SRCDIR DESTDIR

UNIT=usr/lib/systemd/system

# --- binaries ---------------------------------------------------------------
put 0755 "$BIN/portal" usr/bin/fiber-portal
for b in fiber_app fiberctl fiber-factory-reset proximos-control proximosctl; do put 0755 "$BIN/$b" "usr/bin/$b"; done
for b in chirpstack chirpstack-concentratord-sx1302 chirpstack-mqtt-forwarder; do put 0755 "$CS/$b" "usr/bin/$b"; done
put 0755 "$M/recipes-connectivity/chirpstack/files/chirpstack-bootstrap.sh" usr/bin/chirpstack-bootstrap.sh
put 0755 "$M/recipes-core/hostname/files/set-hostname.sh" usr/bin/set-hostname.sh
put 0755 "$M/recipes-core/tls/files/proximos-tls-cert.sh" usr/bin/proximos-tls-cert.sh
put 0755 "$M/recipes-core/proximos-config/files/proximos-journal-prune.sh" usr/bin/proximos-journal-prune.sh
put 0755 "$M/recipes-core/proximos-migrate/files/proximos-data-migrate.sh" usr/libexec/proximos-data-migrate

# --- units ------------------------------------------------------------------
for u in fiber-portal.service fiber.service fiber-factory-reset.service proximos-control.service \
         radio-setup.service proximos-first-run.path proximos-first-run.service; do put 0644 "$P/$u" "$UNIT/$u"; done
put 0644 "$M/recipes-core/proximos-render/files/proximos-render.service" "$UNIT/proximos-render.service"
put 0644 "$M/recipes-core/proximos-migrate/files/proximos-data-migrate.service" "$UNIT/proximos-data-migrate.service"
for u in proximos-data-dirs.service proximos-journal-prune.service; do put 0644 "$M/recipes-core/proximos-config/files/$u" "$UNIT/$u"; done
put 0644 "$M/recipes-core/tls/files/proximos-tls-cert.service" "$UNIT/proximos-tls-cert.service"
put 0644 "$M/recipes-core/hostname/files/hostname-serial.service" "$UNIT/hostname-serial.service"
for u in proximos-firewall.service proximos-firewall-server.service; do put 0644 "$M/recipes-core/firewall/files/$u" "$UNIT/$u"; done
for u in rtc-sync.service rtc-save.service rtc-save.timer; do put 0644 "$M/recipes-core/rtc-sync/files/$u" "$UNIT/$u"; done
for u in chirpstack.service chirpstack-bootstrap.service; do put 0644 "$M/recipes-connectivity/chirpstack/files/$u" "$UNIT/$u"; done
put 0644 "$M/recipes-connectivity/chirpstack-concentratord/files/chirpstack-concentratord.service" "$UNIT/chirpstack-concentratord.service"
put 0644 "$M/recipes-connectivity/chirpstack-mqtt-forwarder/files/chirpstack-mqtt-forwarder.service" "$UNIT/chirpstack-mqtt-forwarder.service"
put 0644 "$REPO_ROOT/files/fiber-bt-unblock.service" "$UNIT/fiber-bt-unblock.service"
put 0644 "$REPO_ROOT/files/fiber-mqtt-lan.service" "$UNIT/fiber-mqtt-lan.service"
put 0644 "$M/recipes-connectivity/mosquitto/files/10-proximos.conf" "$UNIT/mosquitto.service.d/10-proximos.conf"
put 0644 "$M/recipes-connectivity/redis/files/10-proximos.conf" "$UNIT/redis-server.service.d/10-proximos.conf"
put 0644 "$REPO_ROOT/files/bluetooth-experimental.conf" "$UNIT/bluetooth.service.d/10-fiber.conf"
put 0644 "$REPO_ROOT/files/proximos-data-dirs-dropin.conf" "$UNIT/proximos-data-dirs.service.d/10-fiber.conf"
put 0755 "$P/proximos-board-off.shutdown" usr/lib/systemd/system-shutdown/proximos-board-off

# --- sysusers, tmpfiles, migrations, udev -------------------------------------
put 0644 "$P/fiber-portal.sysusers.conf" usr/lib/sysusers.d/fiber-portal.conf
put 0644 "$P/proximos-radio.sysusers.conf" usr/lib/sysusers.d/proximos-radio.conf
put 0644 "$M/recipes-connectivity/chirpstack/files/chirpstack.sysusers.conf" usr/lib/sysusers.d/chirpstack.conf
put 0644 "$P/fiber-portal.tmpfiles.conf" usr/lib/tmpfiles.d/fiber-portal.conf
put 0644 "$P/fiber.tmpfiles.conf" usr/lib/tmpfiles.d/fiber.conf
put 0644 "$P/proximos-radio.tmpfiles.conf" usr/lib/tmpfiles.d/proximos-radio.conf
put 0644 "$M/recipes-connectivity/mosquitto/files/proximos-mosquitto.tmpfiles.conf" usr/lib/tmpfiles.d/proximos-mosquitto.conf
# Not proximos-hosts.tmpfiles.conf: tmpfiles refuses its copy on Pi OS (see
# files/fiber-hosts-seed.sh); fiber-hosts.service seeds the file instead.
put 0644 "$REPO_ROOT/files/fiber-hosts.service" "$UNIT/fiber-hosts.service"
put 0755 "$REPO_ROOT/files/fiber-hosts-seed.sh" usr/lib/fiber/fiber-hosts-seed.sh
put 0755 "$M/recipes-connectivity/chirpstack/files/005-chirpstack-seed.sh" usr/lib/proximos/migrations/005-chirpstack-seed.sh
for m in 010-radio-init 020-ble-init 030-portal-owner 040-config-init; do put 0755 "$P/migrations/$m.sh" "usr/lib/proximos/migrations/$m.sh"; done
put 0644 "$M/recipes-core/rtc-sync/files/60-rtc-sync.rules" usr/lib/udev/rules.d/60-rtc-sync.rules
put 0644 "$M/recipes-connectivity/chirpstack-concentratord/files/99-chirpstack-concentratord.rules" usr/lib/udev/rules.d/99-chirpstack-concentratord.rules

# --- shared data ------------------------------------------------------------
tree "$U/portal/templates" usr/share/fiber-portal/templates
tree "$U/portal/static" usr/share/fiber-portal/static
for f in fiber.config.yaml fiber.sensors.config.yaml authorized_signers.yaml; do put 0644 "$CC/$f" "usr/share/fiber/config/$f"; done
put 0755 "$CC/scripts/init-signer.sh" usr/share/fiber/scripts/init-signer.sh
put 0755 "$P/fiber-config-seed.sh" usr/share/fiber/scripts/fiber-config-seed.sh
put 0644 "$LOCK" usr/share/fiber/upstream.lock
tree "$P/templates" usr/share/proximos/templates
put 0644 "$M/recipes-core/proximos-config/files/data-dirs.conf" usr/share/proximos/data-dirs.conf
put 0644 "$M/recipes-core/firewall/files/firewall.nft" usr/share/proximos/firewall.nft
put 0644 "$M/recipes-core/firewall/files/firewall-server.nft" usr/share/proximos/firewall-server.nft
put 0644 "$REPO_ROOT/files/hosts.base" usr/share/proximos/hosts.base
# The brand (proximos-v2 decision #50): set-hostname.sh and proximos-tls-cert.sh read it.
BRAND="${FIBER_BRAND:-fiber}"
if [ -f "$U/portal/branding/${BRAND}/brand.env" ]; then
    put 0644 "$U/portal/branding/${BRAND}/brand.env" usr/share/proximos/brand.env
fi
put 0755 "$REPO_ROOT/files/health-checks.d/10-data-dir.sh" usr/share/proximos/health-checks.d/10-data-dir.sh
# 70-landlock is left out: the Pi OS kernel has no CONFIG_SECURITY_LANDLOCK, so the
# portal runs degraded by design (spec §6); fiber doctor prints the LSM line instead.
for c in 20-clock 40-firewall 60-services 65-radio-setup; do
    put 0755 "$M/recipes-core/proximos-health/files/$c.sh" "usr/share/proximos/health-checks.d/$c.sh"
done

# --- /etc and /boot ---------------------------------------------------------
put 0644 "$P/fiber-portal-nginx.conf" usr/share/fiber/fiber-portal-nginx.conf   # installed by fiber setup, after apt
put 0644 "$M/recipes-connectivity/mosquitto/files/proximos.conf" usr/share/fiber/mosquitto-proximos.conf   # idem
put 0644 "$REPO_ROOT/files/mosquitto.conf" usr/share/fiber/mosquitto.conf   # fiber setup installs it over Debian's, with a backup
put 0644 "$REPO_ROOT/boot/config-fragment.txt" usr/share/fiber/config-fragment.txt
put 0644 "$M/recipes-connectivity/chirpstack-concentratord/files/chirpstack-concentratord.toml" etc/chirpstack-concentratord/chirpstack-concentratord.toml
put 0644 "$M/recipes-core/base-files/files/journald.conf" etc/systemd/journald.conf.d/proximos.conf
put 0644 "$M/recipes-core/systemd/files/timesyncd-proximos.conf" etc/systemd/timesyncd.conf.d/10-proximos.conf
put 0644 "$REPO_ROOT/boot/fiber.modules" etc/modules-load.d/fiber.conf
put 0644 "$DTBO" usr/share/fiber/overlays/fiber.dtbo   # fiber setup copies it onto the vfat boot partition

# --- the fiber CLI and its libraries (whatever exists) ----------------------
[ -f "$REPO_ROOT/bin/fiber" ] && put 0755 "$REPO_ROOT/bin/fiber" usr/bin/fiber
for l in "$REPO_ROOT"/lib/fiber/*.sh; do put 0644 "$l" "usr/lib/fiber/$(basename "$l")"; done

# --- STM32 firmware and the UART tools ---------------------------------------
# FIBER_FIRMWARE_DIR holds nb-package/ and sb-package/ (tools/build-firmware.sh).
# With them installed, fiber_app keeps the Southbridge and proximos-control the
# Northbridge on these images over UART; the J-Link hex is kept for a first flash.
if [ -n "${FIBER_FIRMWARE_DIR:-}" ]; then
    FW="$FIBER_FIRMWARE_DIR"
    for f in northbridge.bin northbridge.json; do put 0644 "$FW/nb-package/$f" "usr/share/proximos/northbridge/$f"; done
    for f in southbridge.bin southbridge.json; do put 0644 "$FW/sb-package/$f" "usr/share/proximos/southbridge/$f"; done
    put 0644 "$FW/nb-package/northbridge_jlink.hex" usr/share/fiber/stm32/northbridge_jlink.hex
fi
for t in nb_uart_flash.py nb_vtor_jump.py nb_erase_probe.py an3155_probe.py uart_cmd.py uart_listen.py sb_probe.sh; do
    put 0755 "$REPO_ROOT/stm32/$t" "usr/share/fiber/stm32/$t"
done

# --- the two edits ----------------------------------------------------------
sed -i 's/^PREFIX="proximos-"/PREFIX="fiber-"/' "${D}/usr/bin/set-hostname.sh"
grep -q '^PREFIX="fiber-"$' "${D}/usr/bin/set-hostname.sh" || fiber_die "stage: set-hostname.sh no longer has PREFIX=\"proximos-\"; check upstream"
sed -i -e '/^\s*http2 on;/d' -e 's/^\(\s*\)listen 443 ssl;/\1listen 443 ssl http2;/' "${D}/usr/share/fiber/fiber-portal-nginx.conf"
grep -qE '^\s*listen 443 ssl http2;' "${D}/usr/share/fiber/fiber-portal-nginx.conf" || fiber_die "stage: nginx snippet has no 'listen 443 ssl;' line; check upstream"

find "$D" -type d -exec chmod 0755 {} +
fiber_log "staged ${TAG} into ${D} ($(find "$D" -type f | wc -l) files)"
