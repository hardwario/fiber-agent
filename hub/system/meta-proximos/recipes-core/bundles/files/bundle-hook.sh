#!/bin/sh
# RAUC bundle hook (proximos-bundle.bb). The image ships /etc/machine-id empty,
# so a freshly written rootfs slot would generate a new machine-id at its first
# boot. journald keys its directory on /var/log/journal (= /data/log/journal)
# by that ID and vacuums only the current one, so every update used to leave the
# previous directory behind until /data filled up (#127). Copying the running
# system's ID into the new slot keeps one ID, and one journal directory, across
# updates and rollbacks.
#
# The hook ships in the bundle, so it already runs when a Hub updates from an
# image that does not have it. It never fails the install: without a valid ID
# to carry the new slot generates its own, as before.
#
# MACHINE_ID_FILE is overridable for system/tests/test-machine-id.sh.
set -eu

MACHINE_ID_FILE="${MACHINE_ID_FILE:-/etc/machine-id}"

case "${1:-}" in
slot-post-install) ;;
*) exit 0 ;;
esac

[ "${RAUC_SLOT_CLASS:-}" = "rootfs" ] || exit 0

if [ -z "${RAUC_SLOT_MOUNT_POINT:-}" ] || [ ! -d "$RAUC_SLOT_MOUNT_POINT/etc" ]; then
    echo "bundle-hook: no mounted rootfs slot, machine-id not carried" >&2
    exit 0
fi

id=$(cat "$MACHINE_ID_FILE" 2>/dev/null || true)
if ! printf '%s' "$id" | grep -Eqx '[0-9a-f]{32}'; then
    echo "bundle-hook: $MACHINE_ID_FILE holds no valid machine-id, the new slot makes its own" >&2
    exit 0
fi

dst="$RAUC_SLOT_MOUNT_POINT/etc/machine-id"
printf '%s\n' "$id" >"$dst.new"
chmod 0444 "$dst.new"
mv -f "$dst.new" "$dst"
echo "bundle-hook: machine-id carried into slot ${RAUC_SLOT_BOOTNAME:-?}"
