#!/bin/sh
# /data/portal/ — re-owned from the retired `fiber-portal` account to `proximos`.
#
# fiber-portal.service runs as `proximos` from this image on (plan/portal/monolith.md §15,
# issue #88). /data survives a RAUC update and /etc/passwd does not, so on a unit that ran
# an older image the portal's whole tree — portal.db with its -wal/-shm, db.key, exports,
# calibrations — is still owned by the old slot's `fiber-portal` uid, a number this slot has
# no name for. The tmpfiles fragment re-owns the four directories and none of the files, so
# without this the portal could not open its own database.
#
# Run by proximos-data-migrate.service (system/design.md §6.2). proximos-render.service
# Requires= that unit and is ordered after it, and fiber-portal.service Requires= the
# renderer, so on an upgraded unit this has finished before the renderer asserts
# /data/portal's owner and before the portal opens the database. A migration and not a
# tmpfiles `Z` line for the reason 005-chirpstack-seed.sh's header gives: the tmpfiles unit
# is not ordered after the `nofail` /data mount, the migration runner is.
#
# Idempotent — a second run is a second chown to the same owner — and that is load-bearing:
# after a rollback to an older slot and a roll forward again, /data/.schema already says 30
# and the runner will not repeat this, so the documented roll-forward step is running it by
# hand (monolith.md §15.5, system/README.md):
#
#   sh /usr/lib/proximos/migrations/030-portal-owner.sh && systemctl restart fiber-portal
#
# A virgin or factory-reset /data has no /data/portal yet. That is nothing to migrate: the
# tmpfiles fragment and the renderer create it with the new owner.
#
# busybox only, like every migration here (010-radio-init.sh's header has the story): `id`,
# `chown` and the shell's own tests, all of them in every busybox build.
set -eu

dir=/data/portal
owner=proximos

log() { echo "030-portal-owner: $*"; }

# A packaging defect, not a state of /data: the package that ships this script declares the
# account. Exit non-zero like 010/020 do on a failed chown, so the runner leaves the schema
# where it is and retries on the next boot, rather than recording a migration that did
# nothing over a tree the portal then cannot open.
if ! id -u "$owner" >/dev/null 2>&1; then
    log "no '$owner' account on this image - refusing to re-own $dir to nobody" >&2
    exit 1
fi

# Root, walking a tree that a network-facing process could write until this boot: nothing
# here may follow a symlink. A symlinked /data/portal is not something any of our tooling
# makes, so it is refused outright rather than guessed at.
if [ -L "$dir" ]; then
    log "$dir is a symlink - refusing to re-own whatever it points at" >&2
    exit 1
fi

if [ ! -d "$dir" ]; then
    log "$dir does not exist yet - nothing to migrate"
    exit 0
fi

# -h: change a symlink met inside the tree, never its referent. busybox's `chown -R`
# dereferences the links it meets unless busybox was built with CONFIG_DESKTOP, and one
# planted in /data/portal (`db.key -> /etc/shadow`) would otherwise hand its target to
# `proximos`. Without -L/-H neither busybox nor coreutils descends into a linked directory.
#
# Ownership only. The directory modes are the tmpfiles fragment's, the file modes are
# whatever SQLite and the portal created them with, and neither changes with the owner.
chown -R -h "$owner:$owner" "$dir"

log "$dir and everything in it is owned by $owner"
exit 0
