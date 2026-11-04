#!/bin/sh
# Compiles assets/css/app.css -> static/css/app.css via the vendored
# Tailwind standalone CLI (tools/tailwindcss, see fetch-tools.sh).
# Usage: scripts/build-css.sh [watch]
set -eu

SCRIPT_DIR=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
PORTAL_DIR=$(CDPATH='' cd -- "${SCRIPT_DIR}/.." && pwd)
TAILWIND_BIN="${PORTAL_DIR}/tools/tailwindcss"

if [ ! -x "${TAILWIND_BIN}" ]; then
	echo "build-css: tools/tailwindcss not found, running fetch-tools.sh" >&2
	"${SCRIPT_DIR}/fetch-tools.sh"
fi

mkdir -p "${PORTAL_DIR}/static/css"

mode="${1:-}"
set -- -i "${PORTAL_DIR}/assets/css/app.css" -o "${PORTAL_DIR}/static/css/app.css" --minify

if [ "${mode}" = "watch" ]; then
	set -- "$@" --watch
fi

exec "${TAILWIND_BIN}" "$@"
