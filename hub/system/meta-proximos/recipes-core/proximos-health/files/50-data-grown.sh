#!/bin/sh
set -eu

DATA_DIR="${PROXIMOS_HEALTH_DATA_DIR:-/data}"
MOUNTS="${PROXIMOS_HEALTH_MOUNTS:-/proc/mounts}"
SYSFS_BLOCK="${PROXIMOS_HEALTH_SYSFS_BLOCK:-/sys/class/block}"

FLASHED_MIB=256
GROWN_MIN_PCT=125
COVERAGE_MIN_PCT=90

if [ ! -r "$MOUNTS" ]; then
    echo "50-data-grown: cannot read $MOUNTS"
    exit 77
fi

device=$(awk -v d="$DATA_DIR" '$2 == d { print $1 }' "$MOUNTS" | tail -n 1)
if [ -z "$device" ]; then
    echo "50-data-grown: $DATA_DIR is not mounted (10-data-mount.sh is the check that cares)"
    exit 77
fi

size_file="$SYSFS_BLOCK/${device##*/}/size"
if [ ! -r "$size_file" ]; then
    echo "50-data-grown: no block size for $device at $size_file"
    exit 77
fi

sectors=$(cat "$size_file")
part_mib=$((sectors / 2048))

if ! fs=$(stat -f -c '%b %S' "$DATA_DIR" 2>/dev/null); then
    echo "50-data-grown: cannot stat the filesystem at $DATA_DIR"
    exit 77
fi

blocks=${fs% *}
block_size=${fs#* }

if [ "$block_size" -le 0 ] || [ $((1048576 % block_size)) -ne 0 ]; then
    echo "50-data-grown: unexpected filesystem block size $block_size"
    exit 77
fi

fs_mib=$((blocks / (1048576 / block_size)))

if [ $((part_mib * 100)) -lt $((FLASHED_MIB * GROWN_MIN_PCT)) ]; then
    echo "50-data-grown: $device is ${part_mib} MiB, still the ${FLASHED_MIB} MiB that was flashed - systemd-repart did not grow it" >&2
    exit 1
fi

if [ $((fs_mib * 100)) -lt $((part_mib * COVERAGE_MIN_PCT)) ]; then
    echo "50-data-grown: the filesystem on $device is ${fs_mib} MiB inside a ${part_mib} MiB partition - the partition grew but the filesystem did not" >&2
    exit 1
fi

echo "50-data-grown: ${fs_mib} MiB filesystem in a ${part_mib} MiB partition on $device"
