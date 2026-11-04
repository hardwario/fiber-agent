#!/usr/bin/env bash
# Open an interactive RTT terminal to the fiber southbridge.
#
# Expects:
#   - J-Link cable connected to the STM32C031C6 SWD
#   - Firmware with CONFIG_USE_SEGGER_RTT=y already flashed
#   - rttt installed in ../.venv (or globally on PATH)
#
# Usage:
#   ./tools/rtt.sh           # interactive shell
#   ./tools/rtt.sh --reset   # reset target before opening
set -euo pipefail

cd "$(dirname "$0")/.."

# Prefer venv-local rttt if present.
if [[ -x .venv/bin/rttt ]]; then
    RTTT=".venv/bin/rttt"
else
    RTTT="rttt"
fi

exec "$RTTT" --device STM32C031C6 --speed 4000 "$@"
