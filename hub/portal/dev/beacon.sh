#!/usr/bin/env bash
# Drive one mock Beacon's liveness and signal on demand, so
# devices.backend.md §8 (B5) can be walked end to end in the dev loop
# instead of waiting for a fleet-wide coincidence. Publishes to the
# mock-only topic `{prefix}/{host}/mock/control`, which dev/mock/
# mock_publisher.py subscribes to and no real Hub has — same back door
# dev/node.sh uses for Nodes.
#
# Usage:
#   dev/beacon.sh list                      show each Beacon's MAC and flags
#   dev/beacon.sh silence [MAC]             drop it out of eye/sensors
#   dev/beacon.sh resume  [MAC]             put it back, advertising at once
#   dev/beacon.sh rssi DBM [MAC]            move it between signal bands
#   dev/beacon.sh vanish  [MAC]             out of the snapshot (B6 prune)
#   dev/beacon.sh appear  [MAC]             back into it
#   dev/beacon.sh stale   [MAC]             flip the WIRE stale flag
#   dev/beacon.sh empty                     publish `tags: []` once
#   dev/beacon.sh capped                    flip `discovered_capped`
#   dev/beacon.sh provisioning STATE [MAC]  pending|provisioning|provisioned|
#                                           failed|read_only
#
# MAC defaults to the first registered mock Beacon, EE:E0:00:00:00:01 —
# except for vanish/appear/stale, which default to the first DISCOVERED one
# (EE:E0:00:00:00:99, which the mock puts on the air 30 s after it starts):
# those three are about strangers more often than about the fleet.
#
# `vanish` and `stale` are DIFFERENT things, and B6 turns on the difference:
#   vanish  the tag is not in the array at all. A discovered-only tag is then
#           PRUNED (reconcile-by-absence, devices.backend.md §9/§22.2); an
#           enrolled one is kept and goes offline.
#   stale   the tag is still listed, with `stale: true` — the radio has
#           stopped hearing it. NOTHING is pruned; the row is marked
#           disconnected. Before B6 this flag was the prune instruction,
#           which is the rule §22.2 replaced.
#
# A Beacon is a BLE broadcast: it has no "disabled" flag on the wire, so
# going quiet IS vanishing from the eye/sensors snapshot — which is exactly
# what `silence` does. (Registered tags are never pruned from the portal's
# store on absence, so the row stays and goes offline; that asymmetry with
# discovered candidates is the contract's, see state/store.rs.)
#
# The B5 walk-through — the store-interval gate (§3.6) and the fault
# indicator (§3.7), both of which are per-Beacon settings:
#   1. make up                              the mock advertises every 10 s
#   2. open /beacons — the row links to /beacons/EE:E0:00:00:00:01
#   3. on that page, System configuration: set the history storage interval
#      to 1 (minute) and offline detection to 1, and Save. Both are
#      immediate and audited; nothing is sent to the Beacon.
#   4. watch eye_readings grow ONE row per minute despite six advertisements
#      per minute (the portal runs on the host, its database is dev-data/):
#        sqlite3 dev-data/portal.db \
#          "select datetime(ts,'unixepoch'), temperature_c from eye_readings
#             where mac='EE:E0:00:00:00:01' and event_type='live'
#             order by ts desc limit 5;"
#      (leave the override blank instead and it is one row per 15 min, the
#      beacons.store_interval default in dev/config.yaml.)
#      Expect the new cadence from the END of the window that was already
#      running, not from the Save: the gate re-reads device_meta when a
#      bucket boundary is crossed, so a 15 -> 1 edit at 13:42 still waits
#      for 13:45. That is the one point read per stored row the design
#      trades for (src/persist.rs, eye_store_gate).
#   5. dev/beacon.sh silence                after ~1 min the detail page
#                                           shows the Poruchy banner and the
#                                           /beacons row draws the triangle
#   6. dev/beacon.sh resume                 the fault clears on the next sweep
#   7. dev/beacon.sh rssi -105              the signal rung on the detail
#                                           page drops to "weak signal"
#
# The B6 walk-through — the discovered view, the reconcile rules, and
# registration by SN (devices.backend.md §9, spec §4/§3.5):
#   1. make up                              wait ~30 s for the stranger
#   2. open /beacons — "Add Beacon" leads to /beacons/add
#   3. /beacons/add lists EE:E0:00:00:00:99 under "Discovered beacons
#      (last 15 minutes)"; the registered Freezer is NOT there
#   4. dev/beacon.sh vanish                 the stranger disappears from the
#                                           table within one snapshot — that
#                                           is the prune, and it needed no
#                                           stale flag
#      dev/beacon.sh appear                 it comes back
#   5. dev/beacon.sh stale                  it STAYS on the table (still in
#                                           the array); only the row's
#                                           liveness changes
#   6. dev/beacon.sh empty                  every stranger goes, the Freezer
#                                           stays — `tags: []` is the empty
#                                           snapshot, not a signal to ignore
#   7. dev/beacon.sh capped                 the yellow "the Hub is hearing
#                                           more beacons than it can report"
#                                           notice appears, and while it is
#                                           set NOTHING is pruned. Run it
#                                           again to clear.
#   8. press Add on the stranger's row: the MAC fills in above and the
#      cursor lands in Serial number (the advertisement carries none — it is
#      on the device's label). Type one, a name, Register.
#      -> the /beacons row reads "registered — waiting for the first message"
#         until the next advertisement, then "waiting for configuration"
#         (the Hub has enrolled it and not yet written the profile)
#   9. open its detail page: Beacon parameters (BLE) -> "Send reconfiguration
#      request". #command-status reports the outcome, the pill cycles
#      "configuring…" -> gone (provisioned) three seconds later.
#      dev/beacon.sh provisioning failed    the pill turns yellow, the button
#                                           becomes "Try again", and the page
#                                           says the wire carries no reason
#      dev/beacon.sh provisioning read_only the button disappears entirely
#
# Removal is two steps since 2026-09-08 (§22.8): the row's Remove first asks
# the Hub to rewrite the Beacon's configuration and then deletes the
# registration. The Beacon keeps advertising, so it reappears on
# /beacons/add — expected, not a bug.
#
# Hostname/prefix mirror the mock's fixed identity and dev/config.yaml;
# override with MOCK_HOSTNAME / TOPIC_PREFIX if you run either differently.

set -euo pipefail
cd "$(dirname "$0")/.."

host_id=${MOCK_HOSTNAME:-fiber-mock-001}
prefix=${TOPIC_PREFIX:-$(sed -n 's/^topic_prefix: "\(.*\)"/\1/p' dev/config.yaml)}
prefix=${prefix:-proximos}
default_mac=EE:E0:00:00:00:01

usage() {
    echo "usage: dev/beacon.sh list" >&2
    echo "       dev/beacon.sh silence|resume [MAC]" >&2
    echo "       dev/beacon.sh rssi DBM [MAC]" >&2
    echo "       dev/beacon.sh vanish|appear|stale [MAC]" >&2
    echo "       dev/beacon.sh empty" >&2
    echo "       dev/beacon.sh capped" >&2
    echo "       dev/beacon.sh provisioning pending|provisioning|provisioned|failed|read_only [MAC]" >&2
    exit 2
}

cmd=${1:-}
case "$cmd" in
    list)
        payload='{"action":"list"}'
        note="the mock prints the list to its own log: docker compose -f dev/docker-compose.yml logs mock"
        ;;
    silence|resume)
        mac=${2:-$default_mac}
        payload=$(printf '{"action":"eye_%s","mac":"%s"}' "$cmd" "$mac")
        note="$cmd $mac"
        ;;
    vanish|appear|stale)
        # No MAC default here: the mock resolves an empty one to the first
        # DISCOVERED tag for these three (see handle_mock_eye_control), which
        # is what a walk-through of the prune rules wants.
        mac=${2:-}
        payload=$(printf '{"action":"eye_%s","mac":"%s"}' "$cmd" "$mac")
        note="$cmd ${mac:-<first discovered>}"
        ;;
    empty)
        payload='{"action":"eye_empty"}'
        note="one empty eye/sensors snapshot — every stranger pruned, the fleet kept"
        ;;
    capped)
        payload='{"action":"eye_capped"}'
        note="toggled discovered_capped (run again to clear)"
        ;;
    provisioning)
        state=${2:-}
        case "$state" in
            pending|provisioning|provisioned|failed|read_only) ;;
            *) usage ;;
        esac
        mac=${3:-$default_mac}
        payload=$(printf '{"action":"eye_provisioning","mac":"%s","state":"%s"}' "$mac" "$state")
        note="provisioning=$state for $mac"
        ;;
    rssi)
        dbm=${2:-}
        case "$dbm" in
            -[0-9]*|[0-9]*) ;;
            *) usage ;;
        esac
        mac=${3:-$default_mac}
        payload=$(printf '{"action":"eye_rssi","mac":"%s","rssi":%s}' "$mac" "$dbm")
        note="rssi=$dbm for $mac"
        ;;
    *) usage ;;
esac

docker compose -f dev/docker-compose.yml exec -T mosquitto \
    mosquitto_pub -t "$prefix/$host_id/mock/control" -m "$payload"
echo "published to $prefix/$host_id/mock/control: $note"
