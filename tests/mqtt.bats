load test_helper

setup() {
  export FIBER_ROOT="$BATS_TEST_TMPDIR/root"
  mkdir -p "$FIBER_ROOT/etc/mosquitto/conf.d"
  export SYSTEMCTL_LOG="$BATS_TEST_TMPDIR/calls.log"; : > "$SYSTEMCTL_LOG"
  export FIBER_SYSTEMCTL="$REPO_ROOT/tests/fixtures/systemctl-stub"
  export FIBER_NFT="$REPO_ROOT/tests/fixtures/systemctl-stub"
  # mosquitto_passwd stand-in: writes "user:hashed" so the test can read the user back.
  cat > "$BATS_TEST_TMPDIR/mosquitto_passwd" <<'STUB'
#!/bin/sh
# usage seen: -c -b FILE USER PASS
printf '%s:$7$hashed\n' "$4" > "$3"
STUB
  chmod +x "$BATS_TEST_TMPDIR/mosquitto_passwd"
  export FIBER_MOSQUITTO_PASSWD="$BATS_TEST_TMPDIR/mosquitto_passwd"
  source "$REPO_ROOT/lib/fiber/common.sh"
  source "$REPO_ROOT/lib/fiber/mqtt.sh"
}

@test "enable-lan writes a 1884 listener with its own password file" {
  run fiber_mqtt_enable_lan dev s3cret
  [ "$status" -eq 0 ]
  local conf="$FIBER_ROOT/etc/mosquitto/fiber.d/lan.conf"
  grep -q '^listener 1884$' "$conf"
  grep -q '^allow_anonymous false$' "$conf"
  grep -q '^password_file /etc/mosquitto/fiber-lan.passwd$' "$conf"
  grep -q '^dev:' "$FIBER_ROOT/etc/mosquitto/fiber-lan.passwd"
  [ "$(stat -c %a "$FIBER_ROOT/etc/mosquitto/fiber-lan.passwd")" = "600" ]
}

@test "enable-lan opens 1884 through a unit and restarts mosquitto" {
  fiber_mqtt_enable_lan dev s3cret
  grep -q 'enable --now fiber-mqtt-lan.service' "$SYSTEMCTL_LOG"
  grep -q 'restart mosquitto' "$SYSTEMCTL_LOG"
}

@test "enable-lan prints how to connect, with the password" {
  run fiber_mqtt_enable_lan dev s3cret
  [[ "$output" == *"mosquitto_sub -h"*"-p 1884 -u dev -P s3cret"* ]]
}

@test "enable-lan without a password generates one" {
  run fiber_mqtt_enable_lan dev ""
  [ "$status" -eq 0 ]
  [[ "$output" =~ -P\ [A-Za-z0-9]{16,} ]]
}

@test "enable-lan refuses a user name with spaces or a colon" {
  run fiber_mqtt_enable_lan "bad:user" s3cret
  [ "$status" -ne 0 ]
  [ ! -e "$FIBER_ROOT/etc/mosquitto/fiber.d/lan.conf" ]
}

@test "disable-lan removes the listener, closes the port and restarts mosquitto" {
  fiber_mqtt_enable_lan dev s3cret
  : > "$SYSTEMCTL_LOG"
  run fiber_mqtt_disable_lan
  [ "$status" -eq 0 ]
  [ ! -e "$FIBER_ROOT/etc/mosquitto/fiber.d/lan.conf" ]
  [ ! -e "$FIBER_ROOT/etc/mosquitto/fiber-lan.passwd" ]
  grep -q 'disable --now fiber-mqtt-lan.service' "$SYSTEMCTL_LOG"
  grep -q 'delete element inet proximos server_tcp' "$SYSTEMCTL_LOG"
  grep -q 'restart mosquitto' "$SYSTEMCTL_LOG"
}

@test "the unit adds 1884 to the firewall's server set and follows firewall reloads" {
  local u="$REPO_ROOT/files/fiber-mqtt-lan.service"
  grep -q '^ExecStart=/usr/sbin/nft add element inet proximos server_tcp { 1884 }$' "$u"
  grep -q '^ReloadPropagatedFrom=proximos-firewall.service$' "$u"
  grep -q '^ConditionPathExists=/etc/mosquitto/fiber.d/lan.conf$' "$u"
}

@test "fiber setup installs mosquitto-clients" {
  source "$REPO_ROOT/lib/fiber/config-txt.sh"; source "$REPO_ROOT/lib/fiber/setup.sh"
  [[ " $FIBER_APT_PACKAGES " == *" mosquitto-clients "* ]]
}

@test "the LAN listener is loaded after conf.d, where per_listener_settings is set" {
  # mosquitto refuses any security option before per_listener_settings, and
  # include_dir reads files in name order: the listener must come last.
  run cat "$REPO_ROOT/files/mosquitto.conf"
  [ "${lines[0]}" = "include_dir /etc/mosquitto/conf.d" ]
  [ "${lines[2]}" = "include_dir /etc/mosquitto/fiber.d" ]
  ! ls "$FIBER_ROOT/etc/mosquitto/conf.d/" | grep -q lan
}
