#!/bin/sh
# /data/proximos/ble/ — directories only.
#
# The division of labour is exact and each half must stand alone: this migration creates
# the directory tree, and `proximosctl control.ble create` writes the contents (the
# provisioning ledger on the central side, the per-role markers on both). Neither step
# depends on the other having produced file contents, which is what lets a factory-reset
# /data and a first boot be the same code path.
#
# Numbered 020 so it sorts after radio's 010. Idempotent, and run by
# proximos-data-migrate.service before anything else touches /data (system/design.md
# §6.2). A fresh /data starts at schema 0 and runs this along with everything else.
set -eu

root=/data/proximos/ble

# mkdir + chown + chmod rather than `install -d -o … -g … -m …`, for the reason
# 010-radio-init.sh's header sets out at length: this image's busybox is built without the
# install applet, and the one migration that reached a device with `install -d` in it
# failed the boot's whole /data chain. Do not reintroduce it here.
#
# /data/proximos is in the list even though 010 runs first and creates it: the parent is
# declared in no tmpfiles fragment and no data-dirs.conf entry, so it exists only because
# a migration made it, and this one costs three idempotent syscalls to stand on its own
# rather than on its neighbour's ordering. Parent first, so each directory exists before
# it is chowned.
for dir in /data/proximos "$root" "$root/central" "$root/central/archive" "$root/peripheral"; do
    mkdir -p "$dir"
    chown proximos:proximos "$dir"
    chmod 0750 "$dir"
done

# Nothing is written here: beacons.json is created 0600 by the component itself, on a
# temp file with the mode set before any content is written, so it is never briefly
# world-readable. There is deliberately no `pin` file — the FB01 credential is an
# ephemeral session token that lives in memory for the session only (peripheral.md §3),
# so there is nothing at rest here to protect or rotate.
exit 0
