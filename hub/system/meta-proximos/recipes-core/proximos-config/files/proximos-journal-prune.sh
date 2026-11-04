#!/bin/sh
# Delete journal directories left behind under an old machine-id (#127).
#
# journald writes /var/log/journal/<machine-id>/ and vacuums only that one.
# Since the bundle hook carries the machine-id into each new slot, an update no
# longer starts a new directory, but a WIC reflash, a manual dd of a rootfs, and
# every update before the hook existed still did. This runs at every boot and
# keeps the current directory plus the most recently written foreign one: after
# a rollback to a slot with an older ID, that one holds the logs of the boot
# that failed.
#
# Only names that are a machine-id (32 lowercase hex) are considered; anything
# else in the directory is left alone. Without a valid current ID nothing is
# deleted. JOURNAL_DIR, MACHINE_ID_FILE and KEEP_FOREIGN are overridable for
# system/tests/test-machine-id.sh.
set -eu

JOURNAL_DIR="${JOURNAL_DIR:-/var/log/journal}"
MACHINE_ID_FILE="${MACHINE_ID_FILE:-/etc/machine-id}"
KEEP_FOREIGN="${KEEP_FOREIGN:-1}"

is_id() { printf '%s' "$1" | grep -Eqx '[0-9a-f]{32}'; }

current=$(cat "$MACHINE_ID_FILE" 2>/dev/null || true)
if ! is_id "$current"; then
    echo "journal-prune: $MACHINE_ID_FILE holds no valid machine-id, nothing pruned" >&2
    exit 0
fi

[ -d "$JOURNAL_DIR" ] || exit 0

foreign=""
for d in "$JOURNAL_DIR"/*; do
    [ -d "$d" ] || continue
    name=${d##*/}
    is_id "$name" || continue
    [ "$name" = "$current" ] && continue
    foreign="$foreign $d"
done

[ -n "$foreign" ] || exit 0

# Newest first by mtime. The names are hex, so word splitting is safe.
# shellcheck disable=SC2086,SC2046
set -- $(ls -dt $foreign)
kept=0
for d in "$@"; do
    if [ "$kept" -lt "$KEEP_FOREIGN" ]; then
        kept=$((kept + 1))
        continue
    fi
    echo "journal-prune: removing ${d##*/} ($(du -sk "$d" | cut -f1) KiB)"
    rm -rf "$d"
done
