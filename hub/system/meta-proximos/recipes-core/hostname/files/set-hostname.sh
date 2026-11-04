#!/bin/sh
# Name the Hub after its CM4 serial: proximos-<last 8 hex of the serial>
# (plan/decisions.md #17). Runs on every boot, before avahi and NetworkManager,
# so the name is stable across images and slots without any per-device
# provisioning step. CPUINFO, HOSTS and BRAND_ENV are overridable for the tests
# (system/tests/test-hostname.sh); the device uses the defaults.
#
# The prefix is the brand's (decision #50, plan/portal/branding.md §5):
# BRAND_HOSTNAME_PREFIX in /usr/share/proximos/brand.env, which a FIBER install
# ships (fiber-). The image ships no such file and keeps proximos-. The file is
# read, never sourced, and a value that is not lowercase letters and digits
# ending in "-" is ignored with a warning.
set -eu

PREFIX="proximos-"
CPUINFO="${CPUINFO:-/proc/cpuinfo}"
HOSTS="${HOSTS:-/etc/hosts}"
BRAND_ENV="${BRAND_ENV:-/usr/share/proximos/brand.env}"

if [ -f "$BRAND_ENV" ]; then
    brand_prefix=$(sed -n "s/^BRAND_HOSTNAME_PREFIX='\(.*\)'\$/\1/p" "$BRAND_ENV" | tail -n 1)
    case "$brand_prefix" in
        [a-z]*-)
            case "${brand_prefix%-}" in
                *[!a-z0-9]*) echo "set-hostname: ignoring BRAND_HOSTNAME_PREFIX '$brand_prefix' in $BRAND_ENV, keeping $PREFIX" >&2 ;;
                *) PREFIX="$brand_prefix" ;;
            esac
            ;;
        *) echo "set-hostname: no usable BRAND_HOSTNAME_PREFIX in $BRAND_ENV, keeping $PREFIX" >&2 ;;
    esac
fi

# /etc/hosts is a symlink to /run/proximos/net/hosts on the image (base-files'
# bbappend). Edit the file it points at: `sed -i` writes a new file and renames
# it over its argument, which would replace the link with a plain file in /etc,
# and the agent's Server line (plan/control/cluster.md §8) would then land in a
# file nothing reads.
if [ -L "$HOSTS" ]; then
    HOSTS=$(readlink -f "$HOSTS")
fi

serial=$(sed -n 's/^Serial[[:space:]]*:[[:space:]]*//p' "$CPUINFO" | tail -n 1)
if [ -z "$serial" ]; then
    echo "set-hostname: no Serial line in $CPUINFO, refusing to guess" >&2
    exit 1
fi

short=$(printf '%s' "$serial" | tail -c 8)
hostname="${PREFIX}${short}"

hostnamectl set-hostname "$hostname"

# One 127.0.1.1 line, carrying the current name. Replace rather than append:
# a unit renamed by an update (fiber-<serial8> until 2026-09-23) must not keep
# resolving its previous name.
tab=$(printf '\t')
if grep -q '^127\.0\.1\.1[[:space:]]' "$HOSTS"; then
    sed -i "s/^127\.0\.1\.1[[:space:]].*/127.0.1.1${tab}${hostname}/" "$HOSTS"
else
    printf '127.0.1.1\t%s\n' "$hostname" >>"$HOSTS"
fi

echo "set-hostname: hostname is $hostname"
