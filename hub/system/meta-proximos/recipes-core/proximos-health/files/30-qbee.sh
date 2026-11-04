#!/bin/sh
set -eu

QBEE_CONFIG="${PROXIMOS_HEALTH_QBEE_CONFIG:-/data/qbee/etc/qbee-agent.json}"
QBEE_UNIT="${PROXIMOS_HEALTH_QBEE_UNIT:-qbee-agent.service}"

if [ ! -e "$QBEE_CONFIG" ]; then
    echo "30-qbee: not enrolled ($QBEE_CONFIG does not exist)"
    exit 77
fi

if ! systemctl -q is-active "$QBEE_UNIT"; then
    echo "30-qbee: enrolled, but $QBEE_UNIT is not active - this device is no longer reachable by the fleet manager" >&2
    exit 1
fi

echo "30-qbee: enrolled and $QBEE_UNIT is active"
