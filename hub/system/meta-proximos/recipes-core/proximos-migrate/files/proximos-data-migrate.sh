#!/bin/sh
set -eu

MIGRATIONS_DIR="${PROXIMOS_MIGRATE_DIR:-/usr/lib/proximos/migrations}"
SCHEMA_FILE="${PROXIMOS_MIGRATE_SCHEMA:-/data/.schema}"

log() { echo "proximos-data-migrate: $*"; }
warn() { echo "proximos-data-migrate: $*" >&2; }

strip_zeros() {
    _n=$1
    while [ "${#_n}" -gt 1 ] && [ "${_n#0}" != "${_n}" ]; do
        _n=${_n#0}
    done
    printf '%s' "${_n}"
}

is_number() {
    case "$1" in
        '' | *[!0-9]*) return 1 ;;
        *) return 0 ;;
    esac
}

write_schema() {
    _dir=${SCHEMA_FILE%/*}
    [ "${_dir}" != "${SCHEMA_FILE}" ] || _dir="."
    _tmp="${_dir}/.schema.tmp.$$"
    ( umask 022 && printf '%s\n' "$1" >"${_tmp}" )
    mv "${_tmp}" "${SCHEMA_FILE}"
}

current=0
if [ -e "${SCHEMA_FILE}" ]; then
    raw=$(tr -d ' \t\n\r' <"${SCHEMA_FILE}")
    if ! is_number "${raw}"; then
        warn "${SCHEMA_FILE} does not contain a plain integer - refusing to guess what /data has already been through"
        exit 1
    fi
    current=$(strip_zeros "${raw}")
fi

if [ ! -d "${MIGRATIONS_DIR}" ]; then
    log "${MIGRATIONS_DIR} does not exist - nothing to run (schema stays at ${current})"
    exit 0
fi

max=0
prev_num=""
prev_name=""
for script in "${MIGRATIONS_DIR}"/*; do
    [ -f "${script}" ] || continue
    name=${script##*/}

    case "${name}" in
        [0-9][0-9][0-9]-*.sh) ;;
        *.sh)
            warn "${name} is not named NNN-*.sh (exactly three digits) and would never run - refusing to migrate a partition somebody thinks is being prepared"
            exit 1
            ;;
        *)
            warn "${name} is not a migration script - ignoring it"
            continue
            ;;
    esac

    num=$(strip_zeros "${name%%-*}")

    if [ "${num}" = "${prev_num}" ]; then
        warn "${prev_name} and ${name} share migration number ${num} - one of them would never run"
        exit 1
    fi
    prev_num=${num}
    prev_name=${name}

    [ "${num}" -le "${max}" ] || max=${num}
done

if [ "${current}" -gt "${max}" ]; then
    log "/data is at schema ${current}, this image knows up to ${max} - downgrade is unsupported, leaving /data untouched"
    exit 0
fi

if [ "${current}" -eq "${max}" ]; then
    log "/data is at schema ${current}, which is this image's - nothing to do"
    exit 0
fi

ran=0
for script in "${MIGRATIONS_DIR}"/[0-9][0-9][0-9]-*.sh; do
    [ -f "${script}" ] || continue
    name=${script##*/}
    num=$(strip_zeros "${name%%-*}")

    [ "${num}" -gt "${current}" ] || continue

    log "running ${name} (schema ${current} -> ${num})"
    if ! sh -e "${script}"; then
        warn "${name} FAILED - /data stays at schema ${current} and this migration will be retried on the next boot"
        exit 1
    fi

    write_schema "${num}"
    current=${num}
    ran=$((ran + 1))
done

log "/data is at schema ${current} (${ran} migration(s) applied)"
