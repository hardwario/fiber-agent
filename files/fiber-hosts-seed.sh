#!/bin/sh
# Seed /run/proximos/net/hosts, the file /etc/hosts links to, at every boot.
#
# Upstream does this with a tmpfiles `C` line, but on Pi OS /run/proximos is
# owned by proximos (proximos-radio.conf) and /run/proximos/net by root, and
# systemd-tmpfiles refuses that "unsafe path transition": the copy never
# happens, /etc/hosts dangles and localhost does not resolve (the portal then
# reports the MQTT broker disconnected). A root shell has no such rule.
#
# Copies hosts.base when the file is missing, adds the base's lines when they
# are missing (a file that lost localhost), and leaves a complete file alone,
# so the agent's Server line on a cluster Client survives a re-run.
set -eu
DIR="${FIBER_HOSTS_DIR:-/run/proximos/net}"
BASE="${FIBER_HOSTS_BASE:-/usr/share/proximos/hosts.base}"
HOSTS="$DIR/hosts"

mkdir -p "$DIR"
chmod 0755 "$DIR"
if [ ! -s "$HOSTS" ]; then
    cp "$BASE" "$HOSTS"
else
    while IFS= read -r line || [ -n "$line" ]; do
        case "$line" in ''|'#'*) continue ;; esac
        grep -qxF -- "$line" "$HOSTS" || printf '%s\n' "$line" >> "$HOSTS"
    done < "$BASE"
fi
chmod 0644 "$HOSTS"
