#!/bin/bash
# Read-only southbridge bootloader probe: BOOT0 (GPIO26) high, pulse NRST (GPIO7),
# AN3155 sync + GET_ID, then BOOT0 low and pulse NRST so the app runs again.
# libgpiod v2: gpioset holds the lines until it is killed.
set -u
hold() { gpioset -c gpiochip0 "$@" & H=$!; }
rel() { kill "$H" 2>/dev/null; wait "$H" 2>/dev/null; }
hold 26=1 7=0; sleep 0.1; rel
hold 26=1 7=1; sleep 0.3
python3 "$(dirname "$(readlink -f "$0")")/an3155_probe.py" /dev/ttyAMA4
rel
hold 26=0 7=0; sleep 0.1; rel
hold 26=0 7=1; sleep 0.2; rel
echo "SB returned to its app"
