#!/bin/sh
set -eu

OS_RELEASE="${PROXIMOS_HEALTH_OS_RELEASE:-/etc/os-release}"

if synced=$(timedatectl show -p NTPSynchronized --value 2>/dev/null); then
    if [ "$synced" = "yes" ]; then
        echo "20-clock: NTP synchronised"
        exit 0
    fi
fi

if ! floor=$(stat -c %Y "$OS_RELEASE" 2>/dev/null); then
    echo "20-clock: cannot read the mtime of $OS_RELEASE, so there is nothing to compare the clock against" >&2
    exit 1
fi

now=$(date +%s)

if [ "$now" -lt "$floor" ]; then
    echo "20-clock: the clock reads $now, older than the image it is running ($floor) - the RTC is dead, unset or was never written" >&2
    exit 1
fi

echo "20-clock: not NTP-synchronised, but the clock ($now) is at least as new as the image ($floor)"
