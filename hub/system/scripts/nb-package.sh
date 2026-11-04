#!/usr/bin/env bash
###############################################################################
# Turn a Northbridge (NB) build into the package the Hub image carries.
#
#   system/scripts/nb-package.sh <nb-build-dir> <out-dir>
#
#   <nb-build-dir>  a `west build` directory of fiber-northbridge/app
#                   (the one holding zephyr/zephyr.bin)
#   <out-dir>       created if missing; receives northbridge.bin and
#                   northbridge.json, plus nbboot.bin / nbboot.hex and
#                   northbridge_jlink.hex (stub + app + app_valid record, the
#                   one-time J-Link migration image) when the build made them
#
# The ONE place that calls firmware/fiber-northbridge/tools/nb_package.py, so
# a change to its interface is a change to this file only. Callers: the
# firmware:north-build CI job and system/scripts/fetch-nb-package.sh --build.
# The format of the two files is plan/control/radio/northbridge_update.md §10;
# the Yocto recipe northbridge-firmware.bb re-checks it when it installs them.
#
# Environment:
#   NBBOOT_DIR   where the nbboot stub's build output is (default:
#                <nb-build-dir>/nbboot, where the app build makes it, else the
#                sibling build-nbboot/ of <nb-build-dir>). nbboot.bin/.hex
#                there, or zephyr/zephyr.bin/.hex of a `west build`, are
#                copied as nbboot.bin/.hex. Missing is not an error: the stub
#                is flashed once by J-Link and is never part of the image.
#   PYTHON       interpreter for nb_package.py (default: python3)
###############################################################################
set -euo pipefail

if [ "$#" -ne 2 ]; then
    echo "usage: $0 <nb-build-dir> <out-dir>" >&2
    exit 2
fi

build_dir="$1"
out_dir="$2"

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/../.." && pwd)"
NB_PACKAGE_PY="${REPO_ROOT}/firmware/fiber-northbridge/tools/nb_package.py"
PYTHON="${PYTHON:-python3}"

app_bin="${build_dir}/zephyr/zephyr.bin"
if [ ! -f "${app_bin}" ]; then
    echo "ERROR: ${app_bin} does not exist - build the NB first:" >&2
    echo "       west build -b stm32wl5moc -d <dir> fiber-northbridge/app" >&2
    exit 1
fi
if [ ! -f "${NB_PACKAGE_PY}" ]; then
    echo "ERROR: ${NB_PACKAGE_PY} is missing - this tree predates the NB" >&2
    echo "       update packaging (northbridge_update.md, step U1)." >&2
    exit 1
fi

mkdir -p "${out_dir}"
rm -f "${out_dir}/northbridge.bin" "${out_dir}/northbridge.json" \
      "${out_dir}/nbboot.bin" "${out_dir}/nbboot.hex" \
      "${out_dir}/northbridge_jlink.hex"

# fw_version and git_sha come from the build directory itself, so the
# manifest describes exactly the image it sits next to.
"${PYTHON}" "${NB_PACKAGE_PY}" --bin "${app_bin}" --out "${out_dir}"

for f in northbridge.bin northbridge.json; do
    if [ ! -s "${out_dir}/${f}" ]; then
        echo "ERROR: nb_package.py did not produce ${out_dir}/${f}" >&2
        exit 1
    fi
done

if [ -n "${NBBOOT_DIR:-}" ]; then
    nbboot_dir="${NBBOOT_DIR}"
elif [ -d "${build_dir}/nbboot" ]; then
    nbboot_dir="${build_dir}/nbboot"
else
    nbboot_dir="$(dirname -- "${build_dir}")/build-nbboot"
fi
nbboot_found=0
for ext in bin hex; do
    for src in "${nbboot_dir}/nbboot.${ext}" "${nbboot_dir}/zephyr/zephyr.${ext}"; do
        if [ -f "${src}" ]; then
            cp "${src}" "${out_dir}/nbboot.${ext}"
            nbboot_found=1
            break
        fi
    done
done
if [ "${nbboot_found}" -eq 0 ]; then
    echo "note: no nbboot build under ${nbboot_dir} - package without the stub"
fi
# The app build's J-Link image: what `west flash` programs, and what the
# one-time migration of a 0.3.x NB flashes (northbridge_update.md §4).
if [ -f "${build_dir}/zephyr/northbridge_jlink.hex" ]; then
    cp "${build_dir}/zephyr/northbridge_jlink.hex" "${out_dir}/"
fi

echo "NB package in ${out_dir}:"
ls -l "${out_dir}"
cat "${out_dir}/northbridge.json"
echo
