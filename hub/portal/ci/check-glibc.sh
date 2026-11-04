#!/usr/bin/env bash
###############################################################################
# Will these aarch64 binaries run on the Hub? Answered from their ELF headers,
# without a device and without a Yocto build.
#
#   portal/ci/check-glibc.sh <binary>...
#
# Tier 1 of the developer loop (plan/system/design.md section 11, issue #18)
# cross-compiles the portal workspace with Debian bookworm's aarch64 toolchain
# (portal/ci/Dockerfile) instead of a Yocto SDK. That works because glibc is
# forward compatible: a binary linked against bookworm's 2.36 runs on the
# image's 2.43. It keeps working only as long as nothing drifts, and v1's
# "builds locally, 203/EXEC on the device" was what drift looks like: a
# toolchain whose glibc was newer than the device's. This script checks it.
# A binary fails if:
#
#   1. it is not an AArch64 ELF (a host build passed in by mistake);
#   2. it asks for a program interpreter the Hub does not have;
#   3. it needs a GLIBC_ symbol version newer than GLIBC_MAX, or GLIBC_PRIVATE;
#   4. it needs a shared library (NEEDED) that is not in the list of what the
#      image ships, below.
#
# A binary that shows no GLIBC_ version or no NEEDED entry at all fails too.
# Everything this workspace builds is dynamically linked against glibc, so an
# empty answer means the parsing broke, and a broken check must not pass.
#
# Environment:
#   GLIBC_MAX  newest GLIBC_ version a binary may need (default 2.36; see below)
#   READELF    the readelf to use (default: aarch64-linux-gnu-readelf when it
#              is installed, else readelf. GNU readelf reads an ELF of any
#              architecture, so the host's own works as well.)
#
# Exit: 0 every binary passed, 1 at least one failed, 2 usage or setup error.
###############################################################################
set -euo pipefail

# Why 2.36 and not the target's 2.43 (glibc_2.43.bb at the pinned oe-core
# commit): 2.36 is the glibc of the toolchain these binaries come from. A
# binary that needs more than its own toolchain has did not come from this
# toolchain. Holding the line there means a base image that moves (trixie
# ships 2.41) fails here on the day it moves, not on the later day its output
# crosses 2.43 and stops running on the Hub.
GLIBC_MAX="${GLIBC_MAX:-2.36}"

# The image's own glibc. Messages only: it tells a binary that merely did not
# come from this toolchain apart from one that cannot start on the Hub at all.
# Moves with a layer bump (oe-core's recipes-core/glibc/glibc_<v>.bb).
HUB_GLIBC="2.43"

# Interpreters the Hub resolves. The image is usrmerged (/lib -> usr/lib in the
# rootfs), so bookworm's /lib path and the /usr/lib path a Yocto-native build
# writes both reach the same ld.so.
ALLOWED_INTERPRETERS="/lib/ld-linux-aarch64.so.1 /usr/lib/ld-linux-aarch64.so.1"

# The shared libraries these binaries may link, by soname, with the image
# package that ships each one (proximos-image's manifest). A soname, not a
# library name: a major-version bump on either side (libcrypto.so.4) is an ABI
# break and has to fail here rather than at boot. Before adding a line, confirm
# the package really is in the production image, not only in the dev one.
#
#   ld-linux-aarch64.so.1  libc6        glibc
#   libc.so.6              libc6
#   libm.so.6              libc6
#   libgcc_s.so.1          libgcc1      unwinding (Rust's panic=unwind)
#   libcrypto.so.3         libcrypto3   OpenSSL 3: rusqlite's bundled-sqlcipher
#   libssl.so.3            libssl3      OpenSSL 3 (nginx and mosquitto ship it)
#   libdbus-1.so.3         libdbus-1-3  bluer (control-core, control-ble)
ALLOWED_SONAMES="ld-linux-aarch64.so.1 libc.so.6 libm.so.6 libgcc_s.so.1
libcrypto.so.3 libssl.so.3 libdbus-1.so.3"

# GLIBC_ tags that are not plain version numbers. GLIBC_ABI_DT_RELR marks a
# binary linked with packed relative relocations; it appeared in 2.36, so it is
# within the limit by definition. Anything else non-numeric (GLIBC_PRIVATE
# above all, which is glibc-internal and differs between builds) fails.
ALLOWED_GLIBC_MARKERS="GLIBC_ABI_DT_RELR"

usage() {
    cat <<'EOF'
usage: check-glibc.sh <binary>...

Fails unless every binary is an AArch64 ELF that the ProXimos Hub image can
load: its interpreter, its GLIBC_ symbol versions (<= GLIBC_MAX, default 2.36)
and its NEEDED shared libraries must all be ones the image provides.
EOF
}

case "${1:-}" in
    -h|--help) usage; exit 0 ;;
    "") usage >&2; exit 2 ;;
esac

if ! [[ "${GLIBC_MAX}" =~ ^[0-9]+\.[0-9]+$ ]]; then
    echo "check-glibc: GLIBC_MAX must look like 2.36, got '${GLIBC_MAX}'" >&2
    exit 2
fi

if [ -z "${READELF:-}" ]; then
    if command -v aarch64-linux-gnu-readelf >/dev/null 2>&1; then
        READELF=aarch64-linux-gnu-readelf
    else
        READELF=readelf
    fi
fi
if ! command -v "${READELF}" >/dev/null 2>&1; then
    echo "check-glibc: ${READELF} not found (binutils)" >&2
    exit 2
fi

# readelf's wording is the parser's input; keep it out of any translation.
export LC_ALL=C

# version_gt A B: is dotted version A newer than B? sort -V orders 2.9 before
# 2.10, which a plain string comparison does not.
version_gt() {
    [ "$1" != "$2" ] && [ "$(printf '%s\n%s\n' "$1" "$2" | sort -V | tail -n 1)" = "$1" ]
}

# in_list WORD LIST: is WORD one of the whitespace-separated words of LIST?
in_list() {
    local word
    for word in $2; do
        [ "${word}" = "$1" ] && return 0
    done
    return 1
}

# words TEXT: TEXT on one line, for messages.
words() {
    printf '%s' "${1//$'\n'/ }"
}

failed=0
checked=0

for bin in "$@"; do
    name="${bin##*/}"
    if [ ! -f "${bin}" ]; then
        echo "check-glibc: ${bin}: no such file" >&2
        exit 2
    fi
    checked=$((checked + 1))
    problems=()

    if ! header="$("${READELF}" -h "${bin}" 2>&1)"; then
        echo "check-glibc: ${name}: FAIL - not an ELF file (${header##*$'\n'})"
        failed=$((failed + 1))
        continue
    fi
    machine="$(printf '%s\n' "${header}" | sed -n 's/^ *Machine: *//p')"
    if [ "${machine}" != "AArch64" ]; then
        problems+=("built for ${machine:-an unknown machine}, not AArch64 - a host build, not the cross build")
    fi

    interp="$("${READELF}" -l -W "${bin}" | sed -n 's/.*\[Requesting program interpreter: \(.*\)\]$/\1/p' || true)"
    if [ -z "${interp}" ]; then
        problems+=("no program interpreter - not a dynamically linked executable")
    elif ! in_list "${interp}" "${ALLOWED_INTERPRETERS}"; then
        problems+=("interpreter ${interp}, which the Hub does not have (allowed: ${ALLOWED_INTERPRETERS})")
    fi

    # Only the version *needs* (.gnu.version_r) count. The "Version symbols"
    # listing before it repeats the same names per symbol, and a shared object
    # would also print its own definitions.
    versions="$("${READELF}" -V -W "${bin}" | awk '
        /^Version needs section/                  { inside = 1; next }
        /^Version (symbols|definition) section/   { inside = 0 }
        inside {
            for (i = 1; i < NF; i++)
                if ($i == "Name:") print $(i + 1)
        }' | sort -u || true)"
    glibc_versions="$(printf '%s\n' "${versions}" | sed -n 's/^GLIBC_//p')"

    newest=""
    too_new=()
    for v in ${glibc_versions}; do
        if [[ "${v}" =~ ^[0-9]+(\.[0-9]+)+$ ]]; then
            if [ -z "${newest}" ] || version_gt "${v}" "${newest}"; then
                newest="${v}"
            fi
            if version_gt "${v}" "${GLIBC_MAX}"; then
                too_new+=("GLIBC_${v}")
            fi
        elif ! in_list "GLIBC_${v}" "${ALLOWED_GLIBC_MARKERS}"; then
            too_new+=("GLIBC_${v}")
        fi
    done
    if [ -z "${glibc_versions}" ]; then
        problems+=("no GLIBC_ version needs found - readelf output not understood, refusing to pass it")
    elif [ "${#too_new[@]}" -gt 0 ]; then
        if [ -n "${newest}" ] && version_gt "${newest}" "${HUB_GLIBC}"; then
            consequence="newer than the Hub's ${HUB_GLIBC} too: it will not start there"
        else
            consequence="so it was not linked by the cross toolchain"
        fi
        problems+=("needs ${too_new[*]}, beyond GLIBC_MAX=${GLIBC_MAX}, ${consequence}")
    fi

    needed="$("${READELF}" -d -W "${bin}" | sed -n 's/.*(NEEDED).*\[\(.*\)\]$/\1/p' || true)"
    unexpected=()
    for lib in ${needed}; do
        in_list "${lib}" "${ALLOWED_SONAMES}" || unexpected+=("${lib}")
    done
    if [ -z "${needed}" ]; then
        problems+=("no NEEDED shared libraries found - readelf output not understood, refusing to pass it")
    elif [ "${#unexpected[@]}" -gt 0 ]; then
        problems+=("needs ${unexpected[*]}, which the image does not ship (allowed: $(words "${ALLOWED_SONAMES}"))")
    fi

    if [ "${#problems[@]}" -eq 0 ]; then
        echo "check-glibc: ${name}: ok - AArch64, ${interp}, newest GLIBC_${newest}, needs $(words "${needed}")"
    else
        failed=$((failed + 1))
        echo "check-glibc: ${name}: FAIL"
        for p in "${problems[@]}"; do
            echo "    - ${p}"
        done
    fi
done

if [ "${failed}" -ne 0 ]; then
    cat >&2 <<EOF

check-glibc: ${failed} of ${checked} binaries failed.
  Each one either did not come from the cross toolchain in portal/ci/Dockerfile
  or needs something the Hub image does not ship. The worst case is v1's
  "203/EXEC on the device". Build with 'make cross' in portal/ or with the
  portal:build-aarch64 job, not with a host or a newer distribution's
  toolchain. If a new shared library is really needed, add it to
  proximos-image first, then add its soname to ALLOWED_SONAMES in this script.
EOF
    exit 1
fi
echo "check-glibc: all ${checked} binaries pass (GLIBC_MAX=${GLIBC_MAX}, Hub glibc ${HUB_GLIBC})"
