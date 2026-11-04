#!/usr/bin/env bash
# Drive one mock Hub of the demo cluster: take it off the air, park it in
# standby, or age its gateway row in the cluster-wide radio/gateways report.
# Publishes to the mock-only topic `{prefix}/{host}/mock/control`, which
# dev/mock/mock_publisher.py subscribes to and no real Hub has — the same
# back door dev/node.sh and dev/beacon.sh use.
#
# Usage:
#   dev/hub.sh list                          the Hubs of the cluster
#   dev/hub.sh offline [HOST]                retained `offline`, publishing stops
#   dev/hub.sh online  [HOST]                back on the air, state republished
#   dev/hub.sh standby [HOST]                toggle power/standby
#   dev/hub.sh gateway-stale [HOST]          toggle: age this Hub's gateway row
#                                            past the portal's 300 s window
#
# HOST defaults to fiber-mock-001, the server. Names work too: `server`,
# `cold-store`, `truck`.
#
# gateway-stale is always sent to the SERVER, whichever Hub it names: the
# report is cluster-wide and retained on one topic, and the server is its
# only publisher.
#
# The cluster walk-through:
#   1. make multi-up                         three Hubs on one broker
#   2. /hubs lists all three; the truck is alarming, off DC and near-full,
#      and its radio card says "no radio" rather than showing a dead gateway.
#   3. dev/hub.sh offline truck              the row goes offline everywhere
#   4. dev/hub.sh online truck               and recovers
#   5. open the Beacon EE:E0:00:00:00:01 — enrolled on the server, overheard
#      by both clients, so "heard by" ranks three Hubs by RSSI.
#   6. dev/hub.sh gateway-stale cold-store   its gateway row goes offline
#      while the Hub itself stays up.
#
# Prefix mirrors dev/config.yaml; override with TOPIC_PREFIX.

set -euo pipefail
cd "$(dirname "$0")/.."

prefix=${TOPIC_PREFIX:-$(sed -n 's/^topic_prefix: "\(.*\)"/\1/p' dev/config.yaml)}
prefix=${prefix:-proximos}

server=fiber-mock-001

# Profile name or hostname, both spellings of the same Hub.
resolve() {
    case "${1:-}" in
        ""|server|fiber-mock-001|FIBER-MOCK-001)  echo fiber-mock-001 ;;
        cold-store|proximos-c01d0002|PROXIMOS-C01D0002) echo proximos-c01d0002 ;;
        truck|proximos-7a0c0003|PROXIMOS-7A0C0003)      echo proximos-7a0c0003 ;;
        *) echo "unknown hub: $1" >&2; exit 2 ;;
    esac
}

usage() {
    echo "usage: dev/hub.sh list" >&2
    echo "       dev/hub.sh offline|online|standby [server|cold-store|truck]" >&2
    echo "       dev/hub.sh gateway-stale [server|cold-store|truck]" >&2
    exit 2
}

cmd=${1:-}
case "$cmd" in
    list)
        printf '%-12s %-16s %s\n' server fiber-mock-001 "the unit the portal runs on"
        printf '%-12s %-16s %s\n' cold-store proximos-c01d0002 "client, WiFi, P2P Hub with 2 Nodes"
        printf '%-12s %-16s %s\n' truck proximos-7a0c0003 "client, no concentrator, battery + storage alerts"
        exit 0
        ;;
    offline|online|standby)
        host=$(resolve "${2:-}")
        target=$host
        payload=$(printf '{"action":"hub_%s"}' "$cmd")
        note="hub_$cmd on $host"
        ;;
    gateway-stale)
        host=$(resolve "${2:-}")
        # The server owns the retained cluster-wide report, so the control
        # message goes there and names the Hub whose row to age.
        target=$server
        payload=$(printf '{"action":"gateway_stale","hostname":"%s"}' "$host")
        note="toggled the stale flag for $host's gateway row"
        ;;
    *) usage ;;
esac

docker compose -f dev/docker-compose.yml exec -T mosquitto \
    mosquitto_pub -t "$prefix/$target/mock/control" -m "$payload"
echo "published to $prefix/$target/mock/control: $note"
