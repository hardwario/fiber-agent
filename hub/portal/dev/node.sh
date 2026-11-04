#!/usr/bin/env bash
# Drive one mock Node's liveness and history behaviour on demand, so
# devices.backend.md §6 (B3) can be walked end to end in the dev loop
# instead of waiting for a fleet-wide coincidence. Publishes to the
# mock-only topic `{prefix}/{host}/mock/control`, which dev/mock/
# mock_publisher.py subscribes to and no real Hub has — same back-door
# spirit as dev/alarm.sh forcing a probe alarm transition.
#
# Usage:
#   dev/node.sh list                        show each Node's dev_eui and flags
#   dev/node.sh silence [DEV_EUI]           stop its uplinks -> portal marks it offline
#   dev/node.sh resume  [DEV_EUI]           resume, and uplink at once -> recovery edge
#   dev/node.sh history MODE [DEV_EUI]      MODE = ok | silent | history_unavailable
#   dev/node.sh join    [DEV_EUI]           first uplink of a Node just registered
#
# DEV_EUI defaults to the first mock Sticker, 70b3d50000000000.
#
# The B4 lifecycle walk-through:
#   1. Register a Node from /nodes (any 16-hex DevEUI, any 32-hex AppKey).
#      The mock puts it in config/state and leaves it out of every telemetry
#      frame, so the portal shows "registered - waiting for join".
#   2. dev/node.sh join <DEV_EUI>          its first uplink; the caption goes
#                                          and the row starts reporting.
#   3. Remove it from its detail page (typed confirmation) - the row leaves
#      every open list, and its device_meta and unsent draft go with it.
#
# The B3 walk-through:
#   1. dev/node.sh history silent          this Node will not answer history
#   2. dev/node.sh silence                 wait for the portal to call it offline
#                                          (K x interval_report; nodes.offline_detector
#                                          in dev/config.yaml, default 4)
#   3. dev/node.sh resume                  the recovery arms the auto backfill
#   4. its detail page shows "missing N records (...) for from -> to" and a
#      pending auto-on-reconnect job; the reconciler re-sends twice more,
#      one interval_report x 3 apart, then fails the job and raises
#      "Node History Unavailable" on the fault axis.
#   5. dev/node.sh history ok              a further resume actually replays,
#                                          the job completes and the gap row
#                                          disappears.
#
# Hostname/prefix mirror the mock's fixed identity and dev/config.yaml;
# override with MOCK_HOSTNAME / TOPIC_PREFIX if you run either differently.

set -euo pipefail
cd "$(dirname "$0")/.."

host_id=${MOCK_HOSTNAME:-fiber-mock-001}
prefix=${TOPIC_PREFIX:-$(sed -n 's/^topic_prefix: "\(.*\)"/\1/p' dev/config.yaml)}
prefix=${prefix:-proximos}
default_eui=70b3d50000000000

usage() {
    echo "usage: dev/node.sh list" >&2
    echo "       dev/node.sh silence|resume [DEV_EUI]" >&2
    echo "       dev/node.sh history ok|silent|history_unavailable [DEV_EUI]" >&2
    echo "       dev/node.sh join [DEV_EUI]" >&2
    exit 2
}

cmd=${1:-}
case "$cmd" in
    list)
        payload='{"action":"list"}'
        note="the mock prints the list to its own log: docker compose -f dev/docker-compose.yml logs mock"
        ;;
    silence|resume|join)
        eui=${2:-$default_eui}
        payload=$(printf '{"action":"%s","dev_eui":"%s"}' "$cmd" "$eui")
        note="$cmd $eui"
        ;;
    history)
        mode=${2:-}
        case "$mode" in
            ok|silent|history_unavailable) ;;
            *) usage ;;
        esac
        eui=${3:-$default_eui}
        payload=$(printf '{"action":"history","dev_eui":"%s","mode":"%s"}' "$eui" "$mode")
        note="history_mode=$mode for $eui"
        ;;
    *) usage ;;
esac

docker compose -f dev/docker-compose.yml exec -T mosquitto \
    mosquitto_pub -t "$prefix/$host_id/mock/control" -m "$payload"
echo "published to $prefix/$host_id/mock/control: $note"
