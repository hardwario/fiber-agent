#!/bin/sh
set -eu

# The services a software update can break on its own (plan/system/design.md
# section 5.2, amendment 2026-09-23). A unit its own Condition*= skipped passes -
# chirpstack on a Hub in `off` or `p2p` radio mode. Every other unit must be
# active and must not be in a restart loop: a unit that crashes a few seconds
# after each start can read `active` at the moment it is looked at, and its
# restart counter is what gives it away. concentratord and the forwarder that
# Requires= it are left out on purpose: they fail with the radio card (#39),
# which a reboot does not cure, and a gate that fails on hardware burns both
# slots of a device whose software is fine.
UNITS="${PROXIMOS_HEALTH_SERVICES:-proximos-control.service mosquitto.service chirpstack.service fiber-portal.service}"
MAX_RESTARTS="${PROXIMOS_HEALTH_MAX_RESTARTS:-5}"

prop() { # name properties
    printf '%s\n' "$2" | sed -n "s/^$1=//p"
}

# A unit the runtime radio switch stopped (plan/control/radio/node_registry.md
# section 11: moving a Hub to `p2p` stops chirpstack and removes its rendered
# config) keeps the ConditionResult of its last start. Its path condition,
# evaluated now, says whether the running mode wants it at all.
not_wanted_now() { # unit
    systemctl cat "$1" 2>/dev/null | sed -n 's/^ConditionPathExists=//p' |
        while IFS= read -r path; do
            path=${path#|}
            case "$path" in
            !*) [ ! -e "${path#!}" ] || echo no ;;
            *) [ -e "$path" ] || echo no ;;
            esac
        done | grep -q no
}

healthy=""
broken=""
for unit in $UNITS; do
    props=$(systemctl show -p LoadState -p ActiveState -p ConditionResult \
        -p ConditionTimestampMonotonic -p NRestarts "$unit" 2>/dev/null) || props=""
    state=$(prop ActiveState "$props")
    restarts=$(prop NRestarts "$props")
    case "$restarts" in
    '' | *[!0-9]*) restarts=0 ;;
    esac

    # A condition that was evaluated and said no. A unit whose condition was never
    # evaluated (timestamp 0) was never started at all, and that is a failure.
    if [ "$(prop ConditionResult "$props")" = "no" ]; then
        case "$(prop ConditionTimestampMonotonic "$props")" in
        '' | 0) ;;
        *)
            healthy="$healthy $unit(condition-skipped)"
            continue
            ;;
        esac
    fi

    if [ "$(prop LoadState "$props")" != "loaded" ]; then
        broken="$broken $unit(not loaded)"
    elif [ "$state" != "active" ]; then
        if not_wanted_now "$unit"; then
            healthy="$healthy $unit(not-wanted)"
        else
            broken="$broken $unit(${state:-unknown})"
        fi
    elif [ "$restarts" -ge "$MAX_RESTARTS" ]; then
        broken="$broken $unit(restarted $restarts times)"
    else
        healthy="$healthy $unit"
    fi
done

if [ -n "$broken" ]; then
    echo "60-services: not healthy:$broken" >&2
    exit 1
fi

echo "60-services: healthy:$healthy"
