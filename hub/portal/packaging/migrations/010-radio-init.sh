#!/bin/sh
# /data/proximos/radio/ — directories only.
#
# The division of labour is exact and each half must stand alone: this migration creates
# the directory tree, and `proximosctl control.radio create` writes the contents
# (ChirpStack converge + config.json on the LoRaWAN arm, session-DB schema + net_id on
# the P2P arm). Neither step depends on the other having produced file contents, which
# is what lets a factory-reset /data and a first boot be the same code path.
#
# Idempotent, and run by proximos-data-migrate.service before anything else touches
# /data (system/design.md §6.2). A fresh /data starts at schema 0 and runs this along
# with everything else.
set -eu

root=/data/proximos/radio

# mkdir + chown + chmod rather than `install -d -o … -g … -m …`: busybox's install applet
# is a build-time configuration option and this image's busybox is built without it. That
# is not a hypothetical — `install: command not found` on line 17 of this file failed this
# migration on the first device that ran it, and through
# proximos-data-migrate.service's Requires= chain it took proximos-render.service and
# fiber-portal.service down with it: a dead web UI whose only symptom was "Dependency
# failed". 005-chirpstack-seed.sh's header records the same finding about the same image
# (`od`, `base64`, `xxd`, `head -c` are absent or crippled here too). The three plain
# applets below are in every busybox build.
#
# /data/proximos is created here, explicitly, rather than left implicit in `mkdir -p`: no
# tmpfiles fragment and no data-dirs.conf entry declares it, so the numbered migrations
# are its only author, and `install -d` applied its mode and ownership to every directory
# it created — the parent included. Dropping it would silently leave the parent of the
# whole tree 0755 root:root. Parent first, so each directory exists before it is chowned.
for dir in /data/proximos "$root" "$root/lorawan" "$root/p2p"; do
    mkdir -p "$dir"
    chown proximos:proximos "$dir"
    chmod 0750 "$dir"
done

# Nothing is written here. Secrets that will land in this tree — the three ChirpStack API
# tokens, nodes.json, the broker password — are created 0600 by the component itself, on
# a temp file before any content is written, so they are never briefly world-readable.
exit 0
