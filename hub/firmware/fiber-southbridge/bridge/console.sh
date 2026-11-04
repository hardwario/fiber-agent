#!/usr/bin/env bash
# Linux serial console to the ProXimos Hub via the southbridge UART<->RTT bridge.
#
# Prereqs: bridge firmware (bridge/) flashed on the southbridge, J-Link on its
# SWD header, and the Hub image set up with console/getty on ttyAMA4.
#
# Usage:  bash console.sh          (Ctrl-] to exit; press Enter for login prompt)
#         LOG=/path RTT_PORT=NNNN JLINK_BIN=/path/to/bin bash console.sh
set -euo pipefail

if ! [ -t 0 ]; then
  echo "This is an interactive console — run it in a real terminal (not via ! / a pipe)." >&2
  exit 1
fi

# SEGGER tools: PATH first, then where the vendor installer puts them.
if [ -z "${JLINK_BIN:-}" ]; then
  if command -v JLinkExe >/dev/null 2>&1; then
    JLINK_BIN=$(dirname "$(command -v JLinkExe)")
  else
    for d in /opt/SEGGER/JLink /usr/bin /usr/local/bin; do
      [ -x "$d/JLinkExe" ] && JLINK_BIN=$d && break
    done
  fi
fi
[ -x "${JLINK_BIN:-}/JLinkExe" ] || {
  echo "JLinkExe not found. Install the SEGGER J-Link tools, or set" >&2
  echo "JLINK_BIN=/path/to/dir/containing/JLinkExe." >&2
  echo "On nix, the package is unfree and pulls an insecure qt4, so both" >&2
  echo "flags are needed:" >&2
  echo "  NIXPKGS_ALLOW_UNFREE=1 nix build --impure --print-out-paths --expr \\" >&2
  echo "    'with import <nixpkgs> { config = { allowUnfree = true;" >&2
  echo "       segger-jlink.acceptLicense = true;" >&2
  echo "       permittedInsecurePackages = [ \"segger-jlink-qt4-952\" ]; }; };" >&2
  echo "     segger-jlink'" >&2
  exit 1
}

# socat is the terminal half. Without it the script would come up, hold the
# J-Link and then fail with nothing attached, which reads like a dead target.
command -v socat >/dev/null 2>&1 || {
  echo "socat not found - it is what attaches your terminal to the RTT port." >&2
  echo "e.g.  nix-shell -p socat --run 'bash $0'" >&2
  exit 1
}
PORT=${RTT_PORT:-19021}
LOG=${LOG:-/tmp/jlink-rtt-console.log}

# Clean up any leftover instance from a previous run (single J-Link, single client).
pkill -f "JLinkExe .*-RTTTelnetPort $PORT" 2>/dev/null && sleep 1 || true

# Kickstart: this board has NBOOT_SEL=0 and BOOT0 on a CM4 GPIO, so a power-cycle
# can land the MCU in the ST system bootloader instead of the app (symptom: J-Link
# reports "RTT control block not found" and the console stays silent). Reset, then
# jump to the app entry unconditionally — harmless if it was already running.
# Initial MSP and reset vector are the first two words of the image, read from the
# built binary so a rebuild can't leave stale addresses here.
here=$(dirname "$(readlink -f "$0")")
if [ -z "${BIN:-}" ]; then
  for b in "$here/../build-bridge" "$here/../build"; do
    [ -f "$b/zephyr/zephyr.bin" ] && BIN="$b/zephyr/zephyr.bin" && break
  done
fi
if [ -n "${BIN:-}" ] && [ -f "$BIN" ]; then
  read -r -a vec < <(od -An -tx4 -N8 "$BIN")
  KICK=$(mktemp)
  printf 'r\nwreg MSP, 0x%s\nsetPC 0x%s\ng\nexit\n' "${vec[0]}" "${vec[1]}" > "$KICK"
  "$JLINK_BIN/JLinkExe" -device STM32C031C6 -if SWD -speed 4000 -autoconnect 1 \
    -CommandFile "$KICK" >/dev/null 2>&1 || true
  rm -f "$KICK"
else
  echo "note: no build-bridge/zephyr/zephyr.bin - skipping the app kickstart," >&2
  echo "      so a southbridge sitting in the ST bootloader will stay silent." >&2
fi

# J-Link connection + RTT telnet server (stays up for the whole session).
# stdin must stay OPEN but silent: JLinkExe quits on EOF at its J-Link> prompt.
sleep infinity | "$JLINK_BIN/JLinkExe" -device STM32C031C6 -if SWD -speed 4000 \
  -autoconnect 1 -RTTTelnetPort "$PORT" >"$LOG" 2>&1 &
JPID=$!   # pid of JLinkExe (last element of the pipeline)
cleanup() { pkill -P $$ 2>/dev/null || true; }
trap cleanup EXIT INT TERM

# Wait for the RTT server socket to LISTEN. Do NOT probe by connecting:
# the server accepts a single client and a probe connection poisons the slot
# ("There already is an active connection" for the real client).
up=0
for _ in $(seq 1 100); do
  kill -0 "$JPID" 2>/dev/null || { echo "JLinkExe exited — see $LOG"; tail -5 "$LOG"; exit 1; }
  if ss -ltn 2>/dev/null | grep -q ":$PORT "; then up=1; break; fi
  sleep 0.2
done
[ "$up" = 1 ] || { echo "RTT server never came up — see $LOG"; exit 1; }
sleep 0.5  # let the server finish initializing before the one allowed client attaches

echo ">>> Console connected (log: $LOG). Press Enter for a prompt, Ctrl-] to exit."
# Raw terminal <-> RTT channel 0: keystrokes go straight through (passwords,
# Ctrl-C etc. reach the Hub); 0x1d = Ctrl-] detaches locally.
socat -,raw,echo=0,escape=0x1d "TCP:127.0.0.1:$PORT"
