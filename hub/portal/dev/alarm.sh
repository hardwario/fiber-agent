#!/usr/bin/env bash
# Force a wired-probe alarm transition on the mock Hub: publishes the same
# `{prefix}/{host}/alarms/events` payload dev/mock/mock_publisher.py emits on
# a real threshold crossing, so the alarm pipeline (table, notifications,
# activation toast) can be exercised on demand instead of waiting for the
# simulator's random walk to cross a threshold on its own.
#
# Usage:
#   dev/alarm.sh raise [LINE] [warning|critical] [VALUE]   defaults: 1 critical 15.7
#   dev/alarm.sh clear [LINE] [VALUE]                      defaults: 1 4.2
#
# The portal keeps ONE active alarm per (hostname, line): a raise on a line
# already active at that severity is deduped silently — clear it first, or
# raise the other severity, which escalates/de-escalates the row in place.
# A toast (and an e-mail/webhook) goes out for a raise that OPENS a row and
# for the row's FIRST escalation to critical (plan/decisions.md #26); a
# same-severity repeat, a de-escalation and a second climb to critical stay
# silent.
#
# Publishes through the compose broker container (no mosquitto-clients needed
# on the host). Hostname/prefix mirror the mock's fixed identity and
# dev/config.yaml; override with MOCK_HOSTNAME / TOPIC_PREFIX if you run
# either differently.

set -euo pipefail
cd "$(dirname "$0")/.."

host_id=${MOCK_HOSTNAME:-fiber-mock-001}
prefix=${TOPIC_PREFIX:-$(sed -n 's/^topic_prefix: "\(.*\)"/\1/p' dev/config.yaml)}
prefix=${prefix:-proximos}

usage() {
    echo "usage: dev/alarm.sh raise [LINE] [warning|critical] [VALUE]" >&2
    echo "       dev/alarm.sh clear [LINE] [VALUE]" >&2
    exit 2
}

cmd=${1:-}
line=${2:-1}
case "$cmd" in
    raise)
        sev=${3:-critical}
        case "$sev" in
            warning) to_state=WARNING ;;
            critical) to_state=CRITICAL ;;
            *) usage ;;
        esac
        from_state=NORMAL
        value=${4:-15.7}
        ;;
    clear)
        from_state=CRITICAL
        to_state=NORMAL
        value=${3:-4.2}
        ;;
    *) usage ;;
esac

payload=$(printf '{"timestamp":"%s","line":%d,"name":"Probe %d","from_state":"%s","to_state":"%s","temperature_celsius":%s,"event_type":"alarm_transition"}' \
    "$(date -u +%Y-%m-%dT%H:%M:%S+00:00)" "$line" "$((line + 1))" "$from_state" "$to_state" "$value")

# The dedup check the fire-and-forget publish can't do on its own: a raise on
# an already-active line opens no row (same severity is dropped outright, the
# other one only escalates the existing row in place) and therefore pops no
# activation toast — surface that instead of leaving "it ran but nothing
# happened". Best-effort: dev runs keep the DB unencrypted next to us.
db=dev-data/portal.db
active_sev_query="SELECT severity FROM alarm_events WHERE sensor_id='line-$line' AND cleared_at IS NULL LIMIT 1;"
watch_rows=false
if [ "$cmd" = raise ] && command -v sqlite3 >/dev/null && [ -f "$db" ]; then
    watch_rows=true
    before=$(sqlite3 "$db" \
        "SELECT COALESCE(MAX(rowid),0) FROM alarm_events WHERE sensor_id='line-$line';")
    before_sev=$(sqlite3 "$db" "$active_sev_query")
fi

docker compose -f dev/docker-compose.yml exec -T mosquitto \
    mosquitto_pub -t "$prefix/$host_id/alarms/events" -m "$payload"
echo "published $to_state (value $value) for line $line on $prefix/$host_id/alarms/events"

if $watch_rows; then
    sleep 1
    n=$(sqlite3 "$db" \
        "SELECT COUNT(*) FROM alarm_events WHERE sensor_id='line-$line' AND rowid > $before;")
    if [ "$n" = "0" ]; then
        after_sev=$(sqlite3 "$db" "$active_sev_query")
        if [ -n "$after_sev" ] && [ "$after_sev" != "$before_sev" ]; then
            if [ "$after_sev" = "critical" ]; then
                echo "note: escalated line $line's active alarm $before_sev → critical in" >&2
                echo "      place (no new row; the toast and e-mail go out only if this is" >&2
                echo "      the row's first time at critical, plan/decisions.md #26)." >&2
            else
                echo "note: de-escalated line $line's active alarm $before_sev → $after_sev" >&2
                echo "      in place (no new row, no toast — the situation improved)." >&2
            fi
        else
            echo "note: no new alarm row opened, so no toast will pop. Either line $line" >&2
            echo "      already has an active alarm at this severity (run" >&2
            echo "      'dev/alarm.sh clear $line' first), or the portal isn't ingesting" >&2
            echo "      right now — check the dashboard's MQTT banner / 'make logs'." >&2
        fi
    fi
fi
