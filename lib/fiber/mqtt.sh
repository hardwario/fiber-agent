# shellcheck shell=bash
# `fiber mqtt enable-lan|disable-lan`: an MQTT listener on the LAN, for
# development (mosquitto_sub/pub from a laptop).
#
# The Hub's own broker listener stays as it is: 127.0.0.1:1883, anonymous, for
# the portal, the agent and ChirpStack. The LAN listener is a second one on
# 1884 with a password file of its own (proximos.conf sets
# per_listener_settings). It lives in /etc/mosquitto/fiber.d/, which
# mosquitto.conf includes after conf.d: mosquitto refuses a security option
# that comes before per_listener_settings. The firewall opens 1884 through fiber-mqtt-lan.service,
# which adds it to the proximos table's server_tcp set and re-adds it after
# every firewall reload.

: "${FIBER_MOSQUITTO_PASSWD:=mosquitto_passwd}"
: "${FIBER_NFT:=nft}"
FIBER_MQTT_LAN_PORT=1884

fiber_mqtt_enable_lan() { # USER [PASSWORD]
    local user="$1" pass="${2:-}"
    local conf="${FIBER_ROOT}/etc/mosquitto/fiber.d/lan.conf"
    local pwfile="${FIBER_ROOT}/etc/mosquitto/fiber-lan.passwd"
    case "$user" in
        ''|*[!A-Za-z0-9_.-]*) fiber_warn "mqtt: user name '$user': use letters, digits, '.', '_' or '-'"; return 1 ;;
    esac
    if [ -z "$pass" ]; then
        pass=$(head -c 18 /dev/urandom | base64 | tr -dc 'A-Za-z0-9' | head -c 20)
    fi
    mkdir -p "${conf%/*}"
    ( umask 077 && "$FIBER_MOSQUITTO_PASSWD" -c -b "$pwfile" "$user" "$pass" ) \
        || { fiber_warn "mqtt: mosquitto_passwd failed"; return 1; }
    chmod 0600 "$pwfile"
    # The broker warns unless the file is its own, 0600 (as upstream's 8883 accounts);
    # mosquitto_passwd's "owner is not root" warning is about the tool, not the broker.
    if [ -z "$FIBER_ROOT" ]; then chown mosquitto:mosquitto "$pwfile"; fi
    cat > "$conf" <<CONF
# fiber mqtt enable-lan: a password-protected listener on the LAN, for development.
# Remove with: sudo fiber mqtt disable-lan
listener ${FIBER_MQTT_LAN_PORT}
allow_anonymous false
password_file /etc/mosquitto/fiber-lan.passwd
CONF
    "$FIBER_SYSTEMCTL" daemon-reload || true
    "$FIBER_SYSTEMCTL" enable --now fiber-mqtt-lan.service || { fiber_warn "mqtt: could not open port ${FIBER_MQTT_LAN_PORT} in the firewall"; return 1; }
    "$FIBER_SYSTEMCTL" restart mosquitto || { fiber_warn "mqtt: mosquitto did not restart; see journalctl -u mosquitto"; return 1; }
    local host
    host=$(hostname 2>/dev/null || echo "<hub>")
    fiber_log "MQTT is open on the LAN, port ${FIBER_MQTT_LAN_PORT}, user ${user}"
    printf '  mosquitto_sub -h %s.local -p %s -u %s -P %s -t "#" -v\n' "$host" "$FIBER_MQTT_LAN_PORT" "$user" "$pass"
}

fiber_mqtt_disable_lan() {
    rm -f "${FIBER_ROOT}/etc/mosquitto/fiber.d/lan.conf" "${FIBER_ROOT}/etc/mosquitto/fiber-lan.passwd"
    "$FIBER_SYSTEMCTL" disable --now fiber-mqtt-lan.service || true
    "$FIBER_NFT" delete element inet proximos server_tcp "{ ${FIBER_MQTT_LAN_PORT} }" 2>/dev/null || true
    "$FIBER_SYSTEMCTL" restart mosquitto || { fiber_warn "mqtt: mosquitto did not restart"; return 1; }
    fiber_log "MQTT LAN listener removed; the broker is local-only again"
}
