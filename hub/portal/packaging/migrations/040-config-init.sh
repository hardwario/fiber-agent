#!/bin/sh
# /data/proximos/config/ — the device overlay's directory, and a pin for Hubs set up
# before decision #37.
#
# The overlay /data/proximos/config/proximos.config.yaml is what `proximosctl render` merges
# over the packaged defaults (system/design.md §6.1, §7). Until decision #37 nothing wrote
# it and nothing created its directory; this is the `config-init` migration §6.1 said was
# owed. `proximosctl first-run-apply` (proximos-first-run.service) is its first writer: it
# puts the operator's first-run transport choice from the portal's /setup page into it,
# but only while the overlay sets no `radio.mode`.
#
# That rule needs one thing from here. A Hub that was set up on an older image has no
# overlay and runs the packaged default, `lorawan`. Its first run is long over, but nothing
# on disk says so, and a request file would still be honoured. So when a portal database
# already exists, and no overlay does, this pins `radio.mode: lorawan`: what the Hub runs
# today, now written down. A fresh or factory-reset /data has no portal database while
# migrations run (they run before the portal first starts), so it stays unpinned and its
# /setup choice goes through.
#
# Idempotent, and run by proximos-data-migrate.service before anything else touches /data
# (system/design.md §6.2). Busybox applets only: mkdir, chown, chmod, printf, mv. The
# header of 010-radio-init.sh explains why `install` is not among them.
set -eu

dir=/data/proximos/config
overlay="$dir/proximos.config.yaml"

# root:root 0750. Only root reads or writes it: the renderer and first-run-apply both run
# as root, and the portal never touches it. /data/proximos itself belongs to 010, which
# runs before this; `mkdir -p` only guards against a /data that somehow lacks it.
mkdir -p "$dir"
chown root:root "$dir"
chmod 0750 "$dir"

if [ -e /data/portal/portal.db ] && [ ! -e "$overlay" ]; then
    # Temp file and rename in the same directory, mode set before the content goes in,
    # the same way the renderer writes its files.
    tmp="$dir/.proximos.config.yaml.tmp.$$"
    : >"$tmp"
    chmod 0640 "$tmp"
    printf '%s\n' \
        '# ProXimos device overlay, merged over /usr/share/proximos/templates/defaults.yaml by' \
        '# proximosctl render (plan/system/design.md §7). radio.mode was pinned by migration' \
        '# 040-config-init.sh: this Hub was set up before first-run setup could choose it' \
        '# (decision #37). It changes only by factory reset.' \
        'radio:' \
        '  mode: lorawan' >"$tmp"
    mv "$tmp" "$overlay"
    echo "040-config-init: pinned radio.mode: lorawan for a Hub set up before decision #37"
fi

exit 0
