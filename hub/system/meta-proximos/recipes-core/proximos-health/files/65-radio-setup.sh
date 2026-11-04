#!/bin/sh
set -eu

# radio-setup.service must not have ended in error (plan/control/radio/lorawan.md
# section 9 and plan/system/design.md section 5.2, amendments 2026-10-09). It is
# the boot oneshot that runs `proximosctl control.radio create`, and a Hub where
# it failed cannot enrol Nodes. That is not healthy enough to commit an update on.
#
# - active: it finished and succeeded. That includes degraded-green (exit 0, no
#   radio card) and radio.mode off (exit 35, its SuccessExitStatus=). Pass.
# - failed: it ended in error. 10 NotBootstrapped, 11 Unauthenticated and 40
#   StorageError are the hard bootstrap/auth failures the unit file promises to
#   block on. A 20-29 code means systemd stopped retrying a transient failure,
#   and every other code is a fail-fast refusal. All of them fail.
# - activating, deactivating, inactive: it has not finished yet. It is between
#   two retries, waiting behind ChirpStack, or was never started. This fails too,
#   so a slot is never marked good before radio-setup has had its say. A failing
#   check rolls nothing back: the runner only withholds mark-good, and the timer
#   asks again ten minutes later. Only boots spend the slot's attempts.
# - not installed (an image without the radio stack) or masked: skipped. A unit
#   file that systemd cannot load is an image defect, and that fails.
# - inactive because its ConditionPathExists= did not hold: skipped. That is a
#   cluster Client, which runs no radio-setup (see the unit). Only a condition
#   systemd actually evaluated counts: a unit never started also reads
#   ConditionResult=no, but with no ConditionTimestamp, and that still fails.
#
# radio-setup is not in 60-services' list on purpose. A first boot is expected to
# spend some of its transient retries while ChirpStack migrates its database, and
# that restart counter would fail a run that converged in the end.
UNIT="${PROXIMOS_HEALTH_RADIO_SETUP_UNIT:-radio-setup.service}"

prop() { # name properties
    printf '%s\n' "$2" | sed -n "s/^$1=//p"
}

# What an exit code of `proximosctl control.radio` means, by the banded table in
# portal/crates/control-radio/src/error.rs (Kind::exit_code).
meaning() { # code
    case "$1" in
    0) echo "converged" ;;
    35) echo "radio.mode off, nothing to converge" ;;
    10) echo "NotBootstrapped, no ChirpStack API token" ;;
    11) echo "Unauthenticated, ChirpStack refused the credential" ;;
    40) echo "StorageError, the radio state under /data" ;;
    2) echo "usage error" ;;
    1[0-9]) echo "a configuration fact, fail-fast" ;;
    2[0-9]) echo "transient" ;;
    3[0-9]) echo "a mode or scope refusal, fail-fast" ;;
    *) echo "unknown code" ;;
    esac
}

props=$(systemctl show -p LoadState -p ActiveState -p SubState -p Result \
    -p ExecMainStatus -p ConditionResult -p ConditionTimestampMonotonic \
    "$UNIT" 2>/dev/null) || props=""
load=$(prop LoadState "$props")
state=$(prop ActiveState "$props")
sub=$(prop SubState "$props")
result=$(prop Result "$props")
code=$(prop ExecMainStatus "$props")
cond=$(prop ConditionResult "$props")
cond_at=$(prop ConditionTimestampMonotonic "$props")
case "$code" in
'' | *[!0-9]*) code=0 ;;
esac

case "$load" in
loaded) ;;
not-found)
    echo "65-radio-setup: $UNIT is not installed on this image"
    exit 77
    ;;
masked)
    echo "65-radio-setup: $UNIT is masked"
    exit 77
    ;;
'')
    echo "65-radio-setup: systemd gave no state for $UNIT" >&2
    exit 1
    ;;
*)
    echo "65-radio-setup: systemd cannot load $UNIT ($load)" >&2
    exit 1
    ;;
esac

# Its start condition was evaluated and said no: a cluster Client. Placed before
# the state cases because a skipped unit reads inactive, which they count as
# "not finished".
if [ "$cond" = no ]; then
    case "$cond_at" in
    '' | 0) ;;
    *)
        echo "65-radio-setup: $UNIT skipped by its start condition (a cluster Client runs none)"
        exit 77
        ;;
    esac
fi

case "$state" in
active)
    echo "65-radio-setup: $UNIT succeeded (exit $code, $(meaning "$code"))"
    exit 0
    ;;
failed)
    case "$result" in
    exit-code) why="exit $code, $(meaning "$code")" ;;
    start-limit-hit) why="systemd stopped retrying, last exit $code, $(meaning "$code")" ;;
    *) why="result ${result:-unknown}" ;;
    esac
    echo "65-radio-setup: $UNIT failed ($why) - a Hub that cannot enrol Nodes is not marked good" >&2
    exit 1
    ;;
*)
    if [ "$sub" = "auto-restart" ]; then
        why="waiting to retry, last exit $code, $(meaning "$code")"
    else
        why="${state:-unknown}${sub:+/$sub}"
    fi
    echo "65-radio-setup: $UNIT has not finished ($why) - not marked good until it has" >&2
    exit 1
    ;;
esac
