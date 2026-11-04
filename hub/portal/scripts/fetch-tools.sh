#!/bin/sh
# Downloads the Tailwind CSS standalone CLI binary into tools/ (git-ignored).
# No node/npm/bundler — this is the only build tool the portal needs.
# Idempotent: skips the download if tools/tailwindcss already matches the
# pinned sha256 below.
#
# To bump the version: update TAILWIND_VERSION, delete tools/tailwindcss,
# run this script once for the platform(s) you can test, then update the
# hashes below from the release's sha256sums.txt asset.
set -eu

TAILWIND_VERSION="v4.3.3"
BASE_URL="https://github.com/tailwindlabs/tailwindcss/releases/download/${TAILWIND_VERSION}"

# platform -> sha256, from the release's sha256sums.txt
HASH_MACOS_ARM64="cdf646702987a743464dff4d9c60fd4480d1c1e73dd819a9a67f1078815dce9d"
HASH_LINUX_X64="dc61b3ac6b8c9ca874c0cc4c57b2409791a64c5540404ca5f5367360babc313a"
HASH_LINUX_ARM64="55fd0b241214eff3de1e8ee4f22796662f2d2e7a49bcfca7477cfd0bac398195"

SCRIPT_DIR=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
PORTAL_DIR=$(CDPATH='' cd -- "${SCRIPT_DIR}/.." && pwd)
TOOLS_DIR="${PORTAL_DIR}/tools"
BIN_PATH="${TOOLS_DIR}/tailwindcss"

os=$(uname -s)
arch=$(uname -m)

case "${os}" in
	Darwin)
		case "${arch}" in
			arm64) asset="tailwindcss-macos-arm64"; expected_sha="${HASH_MACOS_ARM64}" ;;
			*) echo "fetch-tools: unsupported macOS arch: ${arch} (only arm64 is pinned)" >&2; exit 1 ;;
		esac
		;;
	Linux)
		case "${arch}" in
			x86_64) asset="tailwindcss-linux-x64"; expected_sha="${HASH_LINUX_X64}" ;;
			aarch64) asset="tailwindcss-linux-arm64"; expected_sha="${HASH_LINUX_ARM64}" ;;
			*) echo "fetch-tools: unsupported Linux arch: ${arch}" >&2; exit 1 ;;
		esac
		;;
	*)
		echo "fetch-tools: unsupported OS: ${os}" >&2
		exit 1
		;;
esac

download() {
	# curl locally, wget in CI: the `portal:css` job runs on alpine, whose
	# busybox ships wget and no curl. Assuming curl made that job fail at the
	# first line of this script — invisibly, because the job only ever ran
	# after `portal:lint`, which was failing for its own reasons.
	if command -v curl >/dev/null 2>&1; then
		curl -fsSL "$1" -o "$2"
	elif command -v wget >/dev/null 2>&1; then
		wget -q -O "$2" "$1"
	else
		echo "fetch-tools: need curl or wget to download $1" >&2
		exit 1
	fi
}

sha256_of() {
	if command -v sha256sum >/dev/null 2>&1; then
		sha256sum "$1" | awk '{print $1}'
	else
		shasum -a 256 "$1" | awk '{print $1}'
	fi
}

if [ -f "${BIN_PATH}" ]; then
	actual_sha=$(sha256_of "${BIN_PATH}")
	if [ "${actual_sha}" = "${expected_sha}" ]; then
		echo "fetch-tools: tools/tailwindcss already up to date (${TAILWIND_VERSION})"
		exit 0
	fi
	echo "fetch-tools: hash mismatch, re-downloading tools/tailwindcss"
fi

mkdir -p "${TOOLS_DIR}"
tmp_path="${BIN_PATH}.tmp"
url="${BASE_URL}/${asset}"

echo "fetch-tools: downloading ${url}"
download "${url}" "${tmp_path}"

actual_sha=$(sha256_of "${tmp_path}")
if [ "${actual_sha}" != "${expected_sha}" ]; then
	echo "fetch-tools: sha256 mismatch for ${asset}" >&2
	echo "  expected: ${expected_sha}" >&2
	echo "  actual:   ${actual_sha}" >&2
	rm -f "${tmp_path}"
	exit 1
fi

chmod +x "${tmp_path}"
mv "${tmp_path}" "${BIN_PATH}"
echo "fetch-tools: tools/tailwindcss ${TAILWIND_VERSION} ready"
