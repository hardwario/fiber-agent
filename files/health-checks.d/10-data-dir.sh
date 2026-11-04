#!/bin/sh
# /data is a directory on the root filesystem on Pi OS (fiber-agent design
# §4.6), so there is no data.mount to check: replaces the image's
# 10-data-mount.sh with the writability half of it.
set -eu
DATA_DIR="${PROXIMOS_HEALTH_DATA_DIR:-/data}"
probe="$DATA_DIR/.proximos-health-probe"
if ! touch "$probe" 2>/dev/null; then
    echo "10-data-dir: $DATA_DIR is not writable" >&2
    exit 1
fi
if ! rm -f "$probe"; then
    echo "10-data-dir: cannot remove $probe from an allegedly writable $DATA_DIR" >&2
    exit 1
fi
echo "10-data-dir: $DATA_DIR writable"
