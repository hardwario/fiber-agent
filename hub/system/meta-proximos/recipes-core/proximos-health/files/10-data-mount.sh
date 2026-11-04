#!/bin/sh
set -eu

DATA_DIR="${PROXIMOS_HEALTH_DATA_DIR:-/data}"
DATA_UNIT="${PROXIMOS_HEALTH_DATA_UNIT:-data.mount}"

if ! systemctl -q is-active "$DATA_UNIT"; then
    echo "10-data-mount: $DATA_UNIT is not active" >&2
    exit 1
fi

probe="$DATA_DIR/.proximos-health-probe"

if ! touch "$probe" 2>/dev/null; then
    echo "10-data-mount: $DATA_DIR is mounted but not writable (read-only remount after an I/O error?)" >&2
    exit 1
fi

if ! rm -f "$probe"; then
    echo "10-data-mount: cannot remove $probe from an allegedly writable $DATA_DIR" >&2
    exit 1
fi

echo "10-data-mount: $DATA_UNIT active, $DATA_DIR writable"
