#!/usr/bin/env bash
###############################################################################
# Cross-build the Hub's binaries for aarch64 and prove they load there.
#
#   portal/ci/build-aarch64.sh            # inside the portal-ci image
#
# The body of the portal:build-aarch64 CI job and of `make cross` in portal/,
# so a local cross build and the pipeline's are the same build. It expects the
# toolchain portal/ci/Dockerfile installs (Debian bookworm's
# gcc-aarch64-linux-gnu, libc6-dev-arm64-cross and the :arm64 -dev packages),
# which is why `make cross` runs it inside that image rather than on the host.
# plan/system/design.md section 11 (issue #18) says why this toolchain and not
# a Yocto SDK.
#
# What it does:
#   1. cargo build --release --locked --workspace for aarch64-unknown-linux-gnu.
#      --workspace because the recipe (portal/packaging/fiber-portal.bb) builds
#      that way, and cargo unifies features across what one invocation builds:
#      a narrower build could compile the same crate with different features
#      from the binary the image ships.
#   2. check-glibc.sh over the six binaries the Hub runs - the same six
#      system/scripts/dev-sync.py pushes (its BIN_SPECS).
#   3. Copies them, plus a SHA256SUMS and a BRAND file, to CROSS_DEST.
#
# Environment:
#   PROXIMOS_BRAND    the brand the binaries carry, a directory under
#                     portal/branding/ (default: proximos). plan/portal/branding.md;
#                     `make cross PROXIMOS_BRAND=fiber` builds the FIBER set.
#   CARGO_TARGET_DIR  cargo's target dir (default: portal/target)
#   CROSS_DEST        where the six binaries and SHA256SUMS end up
#                     (default: <target dir>/aarch64-unknown-linux-gnu/release,
#                     i.e. where cargo put them, and where dev-sync.py looks
#                     when the target dir is the default one)
###############################################################################
set -euo pipefail

PORTAL_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${PORTAL_DIR}"

TARGET=aarch64-unknown-linux-gnu

# KEEP IN SYNC with BIN_SPECS in system/scripts/dev-sync.py and with the
# FILES:* lists of portal/packaging/fiber-portal.bb. Cargo names: the recipe
# installs `portal` as /usr/bin/fiber-portal, and dev-sync.py renames it the
# same way; the other five keep their names.
BINARIES=(portal fiber_app fiberctl fiber-factory-reset proximos-control proximosctl)

# The cross settings the CI job used to carry as job variables. They live here
# so `make cross` gets them too. pkg-config is pointed at the :arm64 .pc files
# (libdbus-1 for bluer, openssl for sqlcipher); cargo links with the cross gcc,
# which also brings the arm64 sysroot. There is no .cargo/config.toml on
# purpose (portal/crates/control-core/README.md): a committed cross-linker
# path would break every native build on a machine without that compiler.
export PKG_CONFIG_ALLOW_CROSS=1
export PKG_CONFIG_PATH="${PKG_CONFIG_PATH:-/usr/lib/aarch64-linux-gnu/pkgconfig}"
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER="${CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER:-aarch64-linux-gnu-gcc}"

if ! command -v "${CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER}" >/dev/null 2>&1; then
    echo "ERROR: ${CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER} is not installed." >&2
    echo "       This script runs inside the portal-ci image (portal/ci/Dockerfile)." >&2
    echo "       From a workstation, use 'make cross' in portal/, which starts it there." >&2
    exit 1
fi

# The brand is compiled in (proximos-brand's build script reads PROXIMOS_BRAND),
# so it is checked here, before cargo spends minutes on a name that fails.
brand="${PROXIMOS_BRAND:-proximos}"
if [ ! -f "${PORTAL_DIR}/branding/${brand}/brand.toml" ]; then
    echo "ERROR: PROXIMOS_BRAND=${brand} is not a brand: there is no portal/branding/${brand}/brand.toml." >&2
    echo "       Brands: $(cd "${PORTAL_DIR}/branding" && ls -d -- */ | tr -d / | tr '\n' ' ')" >&2
    exit 1
fi
export PROXIMOS_BRAND="${brand}"

target_dir="${CARGO_TARGET_DIR:-${PORTAL_DIR}/target}"
out="${target_dir}/${TARGET}/release"
dest="${CROSS_DEST:-${out}}"

echo "cross build: brand ${brand}, $(rustc --version), $("${CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER}" --version | sed -n 1p)"
cargo build --release --locked --workspace --target "${TARGET}"

paths=()
for bin in "${BINARIES[@]}"; do
    if [ ! -f "${out}/${bin}" ]; then
        echo "ERROR: cargo finished but ${out}/${bin} is missing." >&2
        echo "       A [[bin]] was renamed or dropped; update BINARIES here, BIN_SPECS" >&2
        echo "       in system/scripts/dev-sync.py and the recipe's FILES together." >&2
        exit 1
    fi
    paths+=("${out}/${bin}")
done

"${PORTAL_DIR}/ci/check-glibc.sh" "${paths[@]}"

mkdir -p "${dest}"
if [ "$(cd "${dest}" && pwd)" != "$(cd "${out}" && pwd)" ]; then
    cp -f "${paths[@]}" "${dest}/"
fi
(cd "${dest}" && sha256sum "${BINARIES[@]}" > SHA256SUMS)
# Which brand these are: the binaries do not say so in their names.
echo "${brand}" > "${dest}/BRAND"

echo "cross build: ${#BINARIES[@]} ${brand} binaries in ${dest}:"
(cd "${dest}" && ls -l "${BINARIES[@]}" SHA256SUMS BRAND)
