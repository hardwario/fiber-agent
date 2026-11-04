#!/usr/bin/env bash
###############################################################################
# Turn a Southbridge (SB) build into the package the Hub image carries.
#
#   system/scripts/sb-package.sh <sb-build-dir> <out-dir>
#
#   <sb-build-dir>  a `west build` directory of fiber-southbridge/app
#                   (the one holding zephyr/zephyr.bin and zephyr/sb_build.json)
#   <out-dir>       created if missing; receives southbridge.bin and
#                   southbridge.json
#
# The ONE place that calls firmware/fiber-southbridge/tools/sb_package.py.
# Callers: the firmware:south-build CI job, and a local build before
# `SOUTHBRIDGE_PKG_DIR=... kas build`. The format of the two files is
# plan/system/southbridge_update.md §3.2; recipes-bsp/southbridge-firmware
# re-checks it when it installs them.
#
# Environment:
#   PYTHON   interpreter for sb_package.py (default: python3)
###############################################################################
set -euo pipefail

if [ "$#" -ne 2 ]; then
    echo "usage: $0 <sb-build-dir> <out-dir>" >&2
    exit 2
fi

build_dir="$1"
out_dir="$2"

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/../.." && pwd)"
SB_PACKAGE_PY="${REPO_ROOT}/firmware/fiber-southbridge/tools/sb_package.py"
PYTHON="${PYTHON:-python3}"

app_bin="${build_dir}/zephyr/zephyr.bin"
if [ ! -f "${app_bin}" ]; then
    echo "ERROR: ${app_bin} does not exist - build the SB first:" >&2
    echo "       west build -b fiber_southbridge -d <dir> fiber-southbridge/app" >&2
    exit 1
fi

mkdir -p "${out_dir}"
rm -f "${out_dir}/southbridge.bin" "${out_dir}/southbridge.json"

# fw_version and git_sha come from the build directory itself, so the
# manifest describes exactly the image it sits next to.
"${PYTHON}" "${SB_PACKAGE_PY}" --bin "${app_bin}" --out "${out_dir}"

for f in southbridge.bin southbridge.json; do
    if [ ! -s "${out_dir}/${f}" ]; then
        echo "ERROR: sb_package.py did not produce ${out_dir}/${f}" >&2
        exit 1
    fi
done

echo "SB package in ${out_dir}:"
ls -l "${out_dir}"
cat "${out_dir}/southbridge.json"
