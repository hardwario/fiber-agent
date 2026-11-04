#!/bin/sh
set -eu

CHECKS_DIR="${PROXIMOS_HEALTH_CHECKS_DIR:-/usr/share/proximos/health-checks.d}"
CMDLINE="${PROXIMOS_HEALTH_CMDLINE:-/proc/cmdline}"
RUNDIR="${PROXIMOS_HEALTH_RUNDIR:-/run/proximos-health}"

SKIP_STATUS=77

MARKED_STAMP="$RUNDIR/marked-good"

log() { echo "proximos-health: $*"; }
warn() { echo "proximos-health: $*" >&2; }

if [ ! -r "$CMDLINE" ]; then
    warn "cannot read $CMDLINE - refusing to decide anything about the running slot"
    exit 1
fi

if grep -qF 'proximos.recovery=1' "$CMDLINE"; then
    log "recovery boot (proximos.recovery=1): marking nothing, neither good nor bad"
    exit 0
fi

failed=""
skipped=""
ran=0

for check in "$CHECKS_DIR"/*; do
    [ -f "$check" ] || continue
    name=${check##*/}

    if [ ! -x "$check" ]; then
        warn "$name is not executable - ignoring it"
        continue
    fi

    if "$check"; then
        status=0
    else
        status=$?
    fi
    ran=$((ran + 1))

    case "$status" in
        0)
            log "$name: pass"
            ;;
        "$SKIP_STATUS")
            log "$name: skipped"
            skipped="$skipped $name"
            ;;
        *)
            warn "$name: FAILED (exit $status)"
            failed="$failed $name"
            ;;
    esac
done

if [ "$ran" -eq 0 ]; then
    warn "no checks found in $CHECKS_DIR - refusing to call an unverified system healthy"
    exit 1
fi

if [ -n "$failed" ]; then
    warn "FAILED:$failed"
    warn "not marking the running slot good - the bootloader's try counter takes it from here"
    exit 1
fi

log "all $ran check(s) passed (skipped:${skipped:- none})"

if [ -e "$MARKED_STAMP" ]; then
    log "the running slot was already marked good during this boot - nothing to do"
    exit 0
fi

if ! rauc_status=$(rauc status --output-format=shell 2>&1); then
    warn "rauc status failed, so the state of the update mechanism is unknown: $rauc_status"
    exit 1
fi

if ! printf '%s\n' "$rauc_status" | grep -qE "^RAUC_SLOT_STATE_[0-9]+='?booted'?$"; then
    log "no slot is in state 'booted' - this system was not booted through the A/B layout, so there is nothing to mark good"
    exit 0
fi

if ! mark_output=$(rauc status mark-good 2>&1); then
    warn "rauc status mark-good failed: $mark_output"
    exit 1
fi

log "$mark_output"

if ! (mkdir -p "$RUNDIR" && touch "$MARKED_STAMP"); then
    warn "could not record the mark in $MARKED_STAMP - later runs this boot will mark again"
fi
