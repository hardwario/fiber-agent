#!/bin/sh
# Seed /data/fiber/config with the shipped default config files on first boot
# (or after a factory reset / fresh /data), without ever touching a file that
# is already there.
#
# /data persists across RAUC slot updates; the rootfs — and therefore
# /usr/share/fiber/config, where control-core's fiber.config.yaml and
# fiber.sensors.config.yaml are baked in read-only by fiber-portal.bb — does
# not. "Copy only if absent" is the whole point of this script: an operator's
# on-device edits (BLE PIN, sensor names, alarm thresholds) and the
# provisioning tool's writes (MQTT/ChirpStack credentials) must survive every
# future update, so this must never clobber an existing file.
#
# Runs from fiber.service's ExecStartPre, before the CA/key bootstrap line —
# same shape as that existing check, and ordered first so the `mkdir -p`
# below also covers the config/ directory init-signer.sh writes
# authorized_signers.yaml into.
#
# ExecStartPre lines are "+"-prefixed, which (per systemd.service(5)) makes
# them run with full privileges and none of the unit's sandboxing applied —
# ProtectSystem=strict/ReadWritePaths=/data/fiber included — so this script
# sees the whole filesystem read-write, same as init-signer.sh already does.

set -e

SOURCE_DIR="/usr/share/fiber/config"
DEST_DIR="/data/fiber/config"

mkdir -p "$DEST_DIR"

for f in fiber.config.yaml fiber.sensors.config.yaml; do
    if [ ! -f "$DEST_DIR/$f" ]; then
        cp "$SOURCE_DIR/$f" "$DEST_DIR/$f"
    fi
done
