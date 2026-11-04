# shellcheck shell=bash
# Shared helpers for the fiber CLI and its libraries.
#
# FIBER_ROOT      prefix for every path a step reads or writes ("" on a device,
#                 a temporary directory in the tests).
# FIBER_SYSTEMCTL the systemctl to call (a logging stub in the tests).

: "${FIBER_ROOT:=}"
: "${FIBER_SYSTEMCTL:=systemctl}"
export FIBER_ROOT FIBER_SYSTEMCTL

fiber_log() { printf 'fiber: %s\n' "$*"; }
fiber_warn() { printf 'fiber: %s\n' "$*" >&2; }
fiber_die() { fiber_warn "$@"; exit 1; }
