#!/bin/sh
# Migration 005 - ChirpStack's tree on /data and the two secrets that must exist
# before anything reads them. Installed as
# /usr/lib/proximos/migrations/005-chirpstack-seed.sh and run by
# proximos-data-migrate.service (system/design.md §6.2).
#
# WHY A MIGRATION AND NOT A tmpfiles.d FRAGMENT, which §6.1's table still says:
# systemd-tmpfiles-setup.service is ordered after local-fs.target, and /data is a
# `nofail` mount, so local-fs.target is NOT ordered after data.mount. A fragment
# would race the mount and, when it lost, create this tree inside the empty mount
# point in the rootfs where the real /data then hides it - silently. v1 hit
# exactly this with its ChirpStack directories; proximos-config's data-dirs.conf
# header carries the long version. The migration runner is ordered after
# data.mount by construction, which is the whole point of it.
#
# GENERATE ONLY, NEVER APPLY. This script writes secret values to files and
# nothing else: chirpstack-bootstrap.service is what feeds admin-password to
# `chirpstack set-password`, and the renderer is what injects api-secret into
# /run/proximos/chirpstack/chirpstack.toml. That split is the correction
# lorawan.md §4 flags against system/design.md §7, which says the seed step
# "actually set[s] it via the API" - impossible at migration time, when there is
# no database and no service. §7's intent (no `admin`/`admin` anywhere, ever) is
# unchanged and in fact stronger for it.
#
# Idempotent, like every migration: re-running it must be a no-op on a device
# that already has these files, because a regenerated api-secret invalidates
# every token in lorawan.md §4's table at once.
set -eu

DATA_DIR="/data/chirpstack"
SECRET_FILE="${DATA_DIR}/api-secret"
PASSWORD_FILE="${DATA_DIR}/admin-password"

log() { echo "005-chirpstack-seed: $*"; }

tmp_secret=""
# The `return 0` is load-bearing rather than tidy: an EXIT trap's last command
# supplies the exit status, and on the success path the test below is false.
#
# SC2329 fires because shellcheck does not follow a name through `trap`, and it
# only misses it here: chirpstack-bootstrap.sh has the same cleanup/trap pair and
# passes, the difference being that it assigns its temp path at the top level
# while this one does so inside write_secret(). The trap is on the next line.
# shellcheck disable=SC2329
cleanup() {
    [ -n "${tmp_secret}" ] && [ -f "${tmp_secret}" ] && rm -f "${tmp_secret}"
    return 0
}
trap cleanup EXIT

# --- the tree -----------------------------------------------------------------
#
# 0700 chirpstack:chirpstack: chirpstack.service runs as that account and the
# SQLite database, its -wal and -shm files and the two secrets below all live
# here. Nothing else on the device has any business in it - not even group
# access, since no second account participates.
#
# mkdir + chown + chmod rather than `install -d -o … -g … -m …`: busybox's
# install applet is a build-time configuration option and this image's busybox is
# demonstrably cut down (the retired proximos-render.sh shim's header recorded
# how `od`, `base64`, `xxd` and `head -c` all turned out to be absent or
# crippled here — the record survives in git history). The three plain applets
# are in every build.
#
# The accounts exist by now because proximos-data-migrate.service is ordered
# After=systemd-sysusers.service, and the chirpstack account's sysusers.d
# fragment ships in the same package as this file - the two arrive together or
# not at all.
mkdir -p "${DATA_DIR}"
chown chirpstack:chirpstack "${DATA_DIR}"
chmod 0700 "${DATA_DIR}"

# --- secret generation --------------------------------------------------------
#
# Two UUIDs read straight from the kernel's CSPRNG, dashes stripped, giving 64
# hex characters (~244 bits). Verbatim the mechanism the retired render shim
# used for the portal's db.key (crates/proximos-render/src/dbkey.rs owns that
# job now), and for the reason recorded above: the
# obvious one-liners (`od -An -N32 -tx1 /dev/urandom`, base64, xxd, `head -c`)
# are all unavailable on this image's busybox, and `openssl` is another recipe's
# runtime dependency rather than a guarantee this script may lean on.
generate_hex64() {
    ( cat /proc/sys/kernel/random/uuid; cat /proc/sys/kernel/random/uuid ) | tr -d '\n-'
}

# The guard is `! -s` plus an explicit length check, not `! -f`: a zero-byte or
# truncated file is worse than an absent one, because an existence-only test
# would keep it forever - signing every ChirpStack token with an empty secret, or
# leaving an admin account whose password nobody can produce.
#
# Both files are 0600 root:root inside the 0700 chirpstack directory. That is
# deliberate and asymmetric with the directory's owner: chirpstack needs neither
# (the renderer copies api-secret into the config the daemon reads, and
# ChirpStack stores only a hash of the password), so the daemon has no path to
# its own signing key on disk. The readers are root processes - the renderer, the
# bootstrap unit, and an operator following lorawan.md §4's ssh tunnel.
#
# ⚠ If proximos-render.service is ever moved off root onto the `proximos`
# account (design.md §7 contemplates it), the thing to change is this file's
# GROUP, not its mode - and chirpstack-bootstrap.sh re-asserts both after its
# recursive chown, so the two must be changed together.
seed_secret() {
    _path=$1
    _what=$2

    if [ -s "${_path}" ] && [ "$(wc -c <"${_path}")" -ge 32 ]; then
        return 0
    fi

    _value=$(generate_hex64)
    if [ "${#_value}" -ne 64 ]; then
        # Loud, not quietly weaker: if the uuid source ever changes shape, this
        # must fail the migration rather than seed a short secret.
        echo "005-chirpstack-seed: refusing to write a ${#_value}-char ${_what} (expected 64)" >&2
        exit 1
    fi

    # umask before the redirect creates the file - a secret that is briefly
    # world-readable has already leaked - then temp + mv, so no reader ever sees
    # a half-written value.
    tmp_secret="${DATA_DIR}/.${_what}.tmp.$$"
    ( umask 077 && printf '%s' "${_value}" >"${tmp_secret}" )
    _value=""
    chown root:root "${tmp_secret}"
    chmod 0600 "${tmp_secret}"
    mv "${tmp_secret}" "${_path}"
    tmp_secret=""
    log "generated ${_path}"
}

# The JWT signing key behind every API token (lorawan.md §2: rotating it
# invalidates all of them at once).
seed_secret "${SECRET_FILE}" "api-secret"

# The web UI's `admin` password, replaced on the factory default by
# chirpstack-bootstrap.service and then kept here as the support credential.
# Written WITHOUT a trailing newline, which is what `chirpstack set-password
# --password-file` was verified to accept on 2026-08-27 at 4.19.1.
seed_secret "${PASSWORD_FILE}" "admin-password"

exit 0
