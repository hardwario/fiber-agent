# shellcheck shell=bash
# The steps of `fiber setup` (design §4.3). Every step reads and writes under
# ${FIBER_ROOT} and calls ${FIBER_SYSTEMCTL}; each prints one
# "setup: <step>: <what changed>" line, or "... unchanged". All idempotent.
#
# Requires common.sh and config-txt.sh to be sourced first.

: "${FIBER_SYSUSERS:=systemd-sysusers}"
: "${FIBER_TMPFILES:=systemd-tmpfiles}"

_step() { printf 'setup: %s: %s\n' "$1" "$2"; }

# Files an earlier fiber-agent release installed and this one no longer ships.
# The release tarball only adds and overwrites, so an upgrade removes them here.
FIBER_OBSOLETE="
usr/lib/tmpfiles.d/proximos-hosts.conf
etc/mosquitto/conf.d/fiber-lan.conf
"

setup_cleanup() {
    local f removed=""
    for f in $FIBER_OBSOLETE; do
        if [ -e "${FIBER_ROOT}/${f}" ]; then rm -f "${FIBER_ROOT}/${f}"; removed="${removed} /${f}"; fi
    done
    if [ -n "$removed" ]; then _step cleanup "removed${removed}"; else _step cleanup unchanged; fi
}

_install_if_changed() { # SRC DEST -> 0 when installed, 1 when identical
    if [ -f "$2" ] && cmp -s "$1" "$2"; then return 1; fi
    install -D -m 0644 "$1" "$2"
}

setup_boot() {
    local cfg="${FIBER_ROOT}/boot/firmware/config.txt"
    local frag="${FIBER_ROOT}/usr/share/fiber/config-fragment.txt"
    local dtbo="${FIBER_ROOT}/usr/share/fiber/overlays/fiber.dtbo"
    local changed="" added
    [ -f "$cfg" ] || { fiber_warn "setup: boot: $cfg not found (is this Raspberry Pi OS?)"; return 1; }
    [ -f "$dtbo" ] || { fiber_warn "setup: boot: $dtbo is not installed"; return 1; }
    [ -f "${FIBER_ROOT}/etc/modules-load.d/fiber.conf" ] || { fiber_warn "setup: boot: modules-load.d/fiber.conf is not installed"; return 1; }
    # The boot partition is vfat: copy, never chmod (install -m on vfat fails with EPERM).
    if ! cmp -s "$dtbo" "${FIBER_ROOT}/boot/firmware/overlays/fiber.dtbo"; then
        mkdir -p "${FIBER_ROOT}/boot/firmware/overlays"
        cp "$dtbo" "${FIBER_ROOT}/boot/firmware/overlays/fiber.dtbo" || { fiber_warn "setup: boot: cannot write the overlay"; return 1; }
        changed="fiber.dtbo installed; "
    fi
    added=$(config_txt_apply "$cfg" "$frag") || { fiber_warn "setup: boot: config.txt edit failed"; return 1; }
    if [ -n "$added" ]; then changed="${changed}config.txt: $(printf '%s' "$added" | sed 's/^added: //' | tr '\n' ' ')"; fi
    _step boot "${changed:-unchanged}"
}

setup_data() {
    local changed=""
    if [ ! -d "${FIBER_ROOT}/data" ]; then mkdir -p "${FIBER_ROOT}/data"; changed="created /data"; fi
    if [ -n "$FIBER_ROOT" ]; then
        "$FIBER_SYSUSERS" --root="$FIBER_ROOT" || { fiber_warn "setup: data: systemd-sysusers failed"; return 1; }
        "$FIBER_TMPFILES" --root="$FIBER_ROOT" --create "${FIBER_ROOT}/usr/share/proximos/data-dirs.conf" || { fiber_warn "setup: data: systemd-tmpfiles failed"; return 1; }
    else
        "$FIBER_SYSUSERS" || { fiber_warn "setup: data: systemd-sysusers failed"; return 1; }
        "$FIBER_TMPFILES" --create /usr/share/proximos/data-dirs.conf || { fiber_warn "setup: data: systemd-tmpfiles failed"; return 1; }
    fi
    _step data "${changed:-unchanged} (sysusers and tmpfiles applied)"
}

setup_mosquitto() {
    local main="${FIBER_ROOT}/etc/mosquitto/mosquitto.conf" changed=""
    local ours="${FIBER_ROOT}/usr/share/fiber/mosquitto.conf"
    if ! { [ -f "$main" ] && cmp -s "$main" "$ours"; }; then
        if [ -f "$main" ] && [ ! -f "${main}.fiber-backup" ]; then cp -p "$main" "${main}.fiber-backup"; fi
        install -D -m 0644 "$ours" "$main" || return 1
        changed="mosquitto.conf replaced (Debian's kept as mosquitto.conf.fiber-backup); "
    fi
    mkdir -p "${FIBER_ROOT}/etc/mosquitto/fiber.d"   # mosquitto.conf includes it; must exist
    if _install_if_changed "${FIBER_ROOT}/usr/share/fiber/mosquitto-proximos.conf" "${FIBER_ROOT}/etc/mosquitto/conf.d/proximos.conf"; then
        changed="${changed}conf.d/proximos.conf installed"
    fi
    _step mosquitto "${changed:-unchanged}"
}

setup_hosts() {
    local hosts="${FIBER_ROOT}/etc/hosts" base="${FIBER_ROOT}/usr/share/proximos/hosts.base"
    # Pi OS (Trixie) runs cloud-init with manage_etc_hosts: true, which would
    # replace the symlink with a file on every boot, and it would reset the
    # hostname hostname-serial sets. One drop-in turns both off.
    local ci="${FIBER_ROOT}/etc/cloud/cloud.cfg.d/99-fiber.cfg" changed=""
    if [ ! -f "$ci" ] || ! grep -q '^manage_etc_hosts: false$' "$ci"; then
        mkdir -p "${ci%/*}"
        printf '# fiber-agent: /etc/hosts is a symlink to /run/proximos/net/hosts and\n# hostname-serial names the Hub; cloud-init must leave both alone.\nmanage_etc_hosts: false\npreserve_hostname: true\n' > "$ci"
        changed="cloud-init told to leave /etc/hosts and the hostname alone; "
    fi
    if [ -L "$hosts" ] && [ "$(readlink "$hosts")" = "/run/proximos/net/hosts" ]; then _step hosts "${changed:-unchanged}"; return 0; fi
    if [ -f "$hosts" ]; then
        # Keep Debian's localhost lines; the 127.0.1.1 line is rewritten by set-hostname.sh at boot.
        grep -v '^127\.0\.1\.1[[:space:]]' "$hosts" > "${base}.tmp" || true
        mv "${base}.tmp" "$base"
        cp -p "$hosts" "${hosts}.fiber-backup"
        rm -f "$hosts"
    fi
    ln -s /run/proximos/net/hosts "$hosts"
    # Make the link resolve now, not only after the reboot: the tmpfiles rule
    # seeds /run/proximos/net/hosts from hosts.base, then add this boot's name.
    FIBER_HOSTS_DIR="${FIBER_ROOT}/run/proximos/net" FIBER_HOSTS_BASE="$base" \
        "${FIBER_ROOT}/usr/lib/fiber/fiber-hosts-seed.sh" || { fiber_warn "setup: hosts: seeding /run/proximos/net/hosts failed"; return 1; }
    if [ -z "$FIBER_ROOT" ]; then
        grep -q '^127\.0\.1\.1[[:space:]]' /run/proximos/net/hosts 2>/dev/null \
            || printf '127.0.1.1\t%s\n' "$(hostname)" >> /run/proximos/net/hosts
    fi
    _step hosts "${changed}/etc/hosts -> /run/proximos/net/hosts (seeded hosts.base)"
}

setup_nginx() {
    local def="${FIBER_ROOT}/etc/nginx/sites-enabled/default" changed=""
    if [ -e "$def" ]; then rm -f "$def"; changed="removed sites-enabled/default; "; fi
    if _install_if_changed "${FIBER_ROOT}/usr/share/fiber/fiber-portal-nginx.conf" "${FIBER_ROOT}/etc/nginx/sites-enabled/fiber-portal.conf"; then
        changed="${changed}sites-enabled/fiber-portal.conf installed"
    fi
    _step nginx "${changed:-unchanged}"
}

setup_rtc() {
    "$FIBER_SYSTEMCTL" disable --now fake-hwclock >/dev/null 2>&1 || true
    _step rtc "fake-hwclock disabled (the PCF85063 is the clock now)"
}

setup_bluetooth() {
    local dropin="${FIBER_ROOT}/usr/lib/systemd/system/bluetooth.service.d/10-fiber.conf"
    grep -q -- '-E' "$dropin" 2>/dev/null || { fiber_warn "setup: bluetooth: $dropin is missing the -E drop-in"; return 1; }
    if [ -z "$FIBER_ROOT" ] && command -v rfkill >/dev/null; then rfkill unblock bluetooth || true; fi
    _step bluetooth "bluetoothd -E drop-in present; radio unblocked"
}

setup_firewall() {
    local nft="${FIBER_ROOT}/usr/share/proximos/firewall.nft"
    if ! grep -q 'tcp dport 22' "$nft" || ! grep -qE 'open_tcp = \{[^}]*\b22\b' "$nft"; then
        fiber_warn "setup: firewall: $nft does not accept tcp port 22 (ssh); refusing to enable a ruleset that would lock you out"
        return 1
    fi
    "$FIBER_SYSTEMCTL" enable proximos-firewall.service proximos-firewall-server.service || { fiber_warn "setup: firewall: enable failed"; return 1; }
    _step firewall "enabled (ssh 22 and https 443 open, everything else dropped)"
}

FIBER_UNITS="fiber-hosts hostname-serial proximos-data-dirs proximos-data-migrate proximos-render proximos-tls-cert fiber-portal fiber proximos-control radio-setup proximos-first-run.path rtc-save.timer proximos-journal-prune fiber-bt-unblock mosquitto nginx redis-server avahi-daemon bluetooth chirpstack-bootstrap chirpstack"

setup_units() {
    "$FIBER_SYSTEMCTL" daemon-reload || { fiber_warn "setup: units: daemon-reload failed"; return 1; }
    # shellcheck disable=SC2086
    "$FIBER_SYSTEMCTL" enable $FIBER_UNITS || { fiber_warn "setup: units: systemctl enable failed (see above)"; return 1; }
    _step units "enabled: $FIBER_UNITS"
}

FIBER_APT_PACKAGES="mosquitto mosquitto-clients nginx-light redis-server bluez network-manager avahi-daemon nftables openssl i2c-tools gpiod rfkill util-linux-extra openocd"

setup_apt() {
    if [ -n "$FIBER_ROOT" ]; then _step apt "skipped (FIBER_ROOT is set)"; return 0; fi
    apt-get update || { fiber_warn "setup: apt: apt-get update failed"; return 1; }
    # shellcheck disable=SC2086
    DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends $FIBER_APT_PACKAGES \
        || { fiber_warn "setup: apt: apt-get install failed"; return 1; }
    _step apt "installed: $FIBER_APT_PACKAGES"
}

fiber_setup_all() {
    # Plain statements, one per step: inside an && / || list bash disables
    # errexit, so a failing step would be reported as success.
    local step
    for step in setup_cleanup setup_apt setup_boot setup_data setup_mosquitto setup_nginx setup_hosts \
                setup_rtc setup_bluetooth setup_firewall setup_units; do
        if ! "$step"; then fiber_warn "setup: stopped at ${step#setup_}; fix the error above and re-run 'fiber setup'"; return 1; fi
    done
    fiber_log "setup done; reboot to apply the overlay and start the services"
}
