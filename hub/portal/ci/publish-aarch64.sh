#!/usr/bin/env bash
###############################################################################
# Publish the cross-built binaries to the project's generic package registry.
#
#   portal/ci/publish-aarch64.sh          # after build-aarch64.sh, in CI
#
# The second half of the portal:build-aarch64 job. The package is
#
#   proximos-portal-aarch64/<version>/{portal,fiber_app,fiberctl,
#       fiber-factory-reset,proximos-control,proximosctl,SHA256SUMS,build-info.txt}
#
# and <version> is the string the image of the same commit is published under
# (proximos-system/<version>, system/scripts/ci-build.sh), because it comes
# from the same place: system/scripts/lib/build-env.sh, i.e. `git describe
# --tags --always --dirty` (GIT_DEPTH 0 in the job). The binaries that belong
# to an image are therefore found under that image's version, which is what
# /etc/os-release on a Hub says. plan/system/design.md section 11 (issue #18).
#
# Which pipelines publish: pushes to main and dev, and version tags (v + digit,
# the same test publish-qbee.sh applies). Merge requests and other branches
# build and check, and their job artifacts (one week) are the deliverable;
# this script says so and exits 0. Retention is packages:cleanup's
# (system/scripts/prune-packages.sh): tags forever, the newest ten others.
#
# Environment:
#   CI_JOB_TOKEN, CI_API_V4_URL, CI_PROJECT_ID   predefined in CI
#   CI_COMMIT_BRANCH, CI_COMMIT_TAG              decide whether to publish
#   CROSS_DEST / CARGO_TARGET_DIR                where build-aarch64.sh put the
#                                                binaries (same defaults)
#   PUBLISH_DRY_RUN=1                            print what would be published,
#                                                upload nothing
###############################################################################
set -euo pipefail

PORTAL_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
PACKAGE_NAME="proximos-portal-aarch64"
TARGET=aarch64-unknown-linux-gnu

# KEEP IN SYNC with BINARIES in build-aarch64.sh.
BINARIES=(portal fiber_app fiberctl fiber-factory-reset proximos-control proximosctl)

target_dir="${CARGO_TARGET_DIR:-${PORTAL_DIR}/target}"
dest="${CROSS_DEST:-${target_dir}/${TARGET}/release}"
dest="$(cd -- "${dest}" && pwd)"

# The two files this script writes sit in cargo's release directory, which the
# CI cache carries from one pipeline to the next. Left alone, an MR pipeline
# would hand on main's build-info.txt as its own job artifact.
rm -f "${dest}/build-info.txt" "${dest}/packages.txt"

tier=""
if [ -n "${CI_COMMIT_TAG:-}" ]; then
    case "${CI_COMMIT_TAG}" in
        v[0-9]*) tier="version tag ${CI_COMMIT_TAG}" ;;
    esac
elif [ "${CI_COMMIT_BRANCH:-}" = "main" ] || [ "${CI_COMMIT_BRANCH:-}" = "dev" ]; then
    tier="branch ${CI_COMMIT_BRANCH}"
fi
if [ -z "${tier}" ]; then
    echo "publish: not main, dev or a version tag - nothing goes to the package registry."
    echo "         The job artifacts hold the binaries for a week."
    exit 0
fi

for f in "${BINARIES[@]}" SHA256SUMS; do
    if [ ! -f "${dest}/${f}" ]; then
        echo "ERROR: ${dest}/${f} is missing. Run portal/ci/build-aarch64.sh first;" >&2
        echo "       it is what checks the binaries before anything publishes them." >&2
        exit 1
    fi
done

# PROXIMOS_VERSION, from the same file the image build takes it from. It also
# changes directory to the repository root, which nothing below minds.
# shellcheck source-path=SCRIPTDIR
# shellcheck source=../../system/scripts/lib/build-env.sh
. "${PORTAL_DIR}/../system/scripts/lib/build-env.sh"

{
    echo "package: ${PACKAGE_NAME}"
    echo "version: ${PROXIMOS_VERSION}"
    echo "commit: ${CI_COMMIT_SHA:-$(git rev-parse HEAD)}"
    echo "pipeline: ${CI_PIPELINE_URL:-local}"
    echo "job: ${CI_JOB_URL:-local}"
    echo "target: ${TARGET}"
    echo "rustc: $(cd "${PORTAL_DIR}" && rustc --version 2>/dev/null || echo unknown)"
    linker="$(aarch64-linux-gnu-gcc --version 2>/dev/null | sed -n 1p || true)"
    echo "linker: ${linker:-unknown}"
    # shellcheck disable=SC2016 # ${Version} is dpkg-query's field, not ours
    echo "sysroot glibc: libc6-dev-arm64-cross $(dpkg-query -W -f '${Version}' libc6-dev-arm64-cross 2>/dev/null || echo unknown)"
    echo "checked by: portal/ci/check-glibc.sh (GLIBC_MAX=${GLIBC_MAX:-2.36})"
    echo "sha256:"
    sed 's/^/  /' "${dest}/SHA256SUMS"
} > "${dest}/build-info.txt"

if [ "${PUBLISH_DRY_RUN:-0}" != "1" ]; then
    for var in CI_JOB_TOKEN CI_API_V4_URL CI_PROJECT_ID; do
        if [ -z "${!var:-}" ]; then
            echo "ERROR: ${var} is not set, so ${tier} cannot publish. All three of" >&2
            echo "       CI_JOB_TOKEN, CI_API_V4_URL and CI_PROJECT_ID are predefined" >&2
            echo "       in a GitLab job; PUBLISH_DRY_RUN=1 runs this without them." >&2
            exit 1
        fi
    done
fi

PACKAGE_BASE="${CI_API_V4_URL:-<api>}/projects/${CI_PROJECT_ID:-<id>}/packages/generic/${PACKAGE_NAME}/${PROXIMOS_VERSION}"
PACKAGES_LIST="${dest}/packages.txt"
: > "${PACKAGES_LIST}"

echo "publish: ${tier} -> ${PACKAGE_NAME}/${PROXIMOS_VERSION}"

# The upload is system/scripts/ci-build.sh's publish(), same reasoning: --fail
# plus an explicit status check (201 created, 200 accepted), three retries
# including a refused connection, and a bound on each transfer.
publish() {
    local file="$1"
    local url="${PACKAGE_BASE}/${file}"
    local code

    if [ "${PUBLISH_DRY_RUN:-0}" = "1" ]; then
        echo "  [dry run] ${dest}/${file} -> ${url}"
        echo "${url}" >> "${PACKAGES_LIST}"
        return
    fi

    if ! code="$(curl --silent --show-error --fail \
                      --retry 3 --retry-connrefused --retry-delay 5 \
                      --max-time 600 \
                      --header "JOB-TOKEN: ${CI_JOB_TOKEN}" \
                      --upload-file "${dest}/${file}" \
                      --write-out '%{http_code}' \
                      --output /dev/null \
                      "${url}")"; then
        echo "ERROR: uploading ${file} to the package registry failed" >&2
        echo "       (curl exit above, HTTP ${code:-unknown})." >&2
        exit 1
    fi
    case "${code}" in
        200 | 201) ;;
        *)
            echo "ERROR: unexpected HTTP ${code} publishing ${file}." >&2
            echo "       Expected 201 (created) or 200 (accepted)." >&2
            exit 1
            ;;
    esac
    echo "  published ${file} (HTTP ${code})"
    echo "${url}" >> "${PACKAGES_LIST}"
}

# The checksums and the provenance first: a reader who finds the binaries
# finds what to verify them against in the same version.
for f in SHA256SUMS build-info.txt "${BINARIES[@]}"; do
    publish "${f}"
done

echo "publish: ${#BINARIES[@]} binaries under ${PACKAGE_BASE}/ (list in ${PACKAGES_LIST})"
