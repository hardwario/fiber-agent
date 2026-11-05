load test_helper

V2_SRC="${V2_SRC:-$REPO_ROOT/hub}"

setup_file() {
  export FIBER_UPSTREAM_DIR="$BATS_FILE_TMPDIR/upstream"
  export FIBER_LOCK="$BATS_FILE_TMPDIR/upstream.lock"
  mkdir -p "$FIBER_UPSTREAM_DIR/vtest/bin" "$FIBER_UPSTREAM_DIR/chirpstack"
  ln -s "$V2_SRC" "$FIBER_UPSTREAM_DIR/vtest/src"
  for b in portal fiber_app fiberctl fiber-factory-reset proximos-control proximosctl; do
    printf '#!/bin/sh\n' > "$FIBER_UPSTREAM_DIR/vtest/bin/$b"; chmod +x "$FIBER_UPSTREAM_DIR/vtest/bin/$b"
  done
  for b in chirpstack chirpstack-concentratord-sx1302 chirpstack-mqtt-forwarder; do
    printf '#!/bin/sh\n' > "$FIBER_UPSTREAM_DIR/chirpstack/$b"; chmod +x "$FIBER_UPSTREAM_DIR/chirpstack/$b"
  done
  printf 'tag: vtest\n' > "$FIBER_LOCK"
  make -C "$REPO_ROOT" dtbo > /dev/null
  "$REPO_ROOT/tools/stage.sh" vtest "$BATS_FILE_TMPDIR/staged" > /dev/null
}

setup() {
  export FIBER_ROOT="$BATS_TEST_TMPDIR/root"
  cp -r "$BATS_FILE_TMPDIR/staged" "$FIBER_ROOT"
  mkdir -p "$FIBER_ROOT/boot/firmware" "$FIBER_ROOT/etc/mosquitto" "$FIBER_ROOT/etc/nginx/sites-enabled"
  cp "$REPO_ROOT/tests/fixtures/config.txt.bookworm" "$FIBER_ROOT/boot/firmware/config.txt"
  printf 'persistence true\npersistence_location /var/lib/mosquitto/\ninclude_dir /etc/mosquitto/conf.d\n' > "$FIBER_ROOT/etc/mosquitto/mosquitto.conf"
  printf '127.0.0.1\tlocalhost\n::1\t\tlocalhost ip6-localhost ip6-loopback\n127.0.1.1\tmyhub\n' > "$FIBER_ROOT/etc/hosts"
  touch "$FIBER_ROOT/etc/nginx/sites-enabled/default"
  export SYSTEMCTL_LOG="$BATS_TEST_TMPDIR/systemctl.log"; : > "$SYSTEMCTL_LOG"
  export FIBER_SYSTEMCTL="$REPO_ROOT/tests/fixtures/systemctl-stub"
  export FIBER_SYSUSERS="$REPO_ROOT/tests/fixtures/systemctl-stub"
  export FIBER_TMPFILES="$REPO_ROOT/tests/fixtures/systemctl-stub"
  source "$REPO_ROOT/lib/fiber/common.sh"
  source "$REPO_ROOT/lib/fiber/config-txt.sh"
  source "$REPO_ROOT/lib/fiber/setup.sh"
}

@test "setup_boot edits config.txt and copies the dtbo onto the boot partition" {
  [ ! -e "$FIBER_ROOT/boot/firmware/overlays/fiber.dtbo" ]
  run setup_boot
  [ "$status" -eq 0 ]
  grep -q '^dtoverlay=fiber$' "$FIBER_ROOT/boot/firmware/config.txt"
  grep -q '^enable_uart=1$' "$FIBER_ROOT/boot/firmware/config.txt"
  cmp "$FIBER_ROOT/boot/firmware/overlays/fiber.dtbo" "$FIBER_ROOT/usr/share/fiber/overlays/fiber.dtbo"
  run setup_boot
  [[ "$output" == *"unchanged"* ]]
  [ -f "$FIBER_ROOT/etc/modules-load.d/fiber.conf" ]
  [[ "$output" == *"setup: boot:"* ]]
}

@test "setup_data creates /data and runs sysusers and tmpfiles" {
  setup_data
  [ -d "$FIBER_ROOT/data" ]
  grep -q '^systemctl-stub' "$SYSTEMCTL_LOG"
  grep -q 'data-dirs.conf' "$SYSTEMCTL_LOG"
}

@test "setup_mosquitto backs up Debian's main file and installs the two include_dir lines" {
  setup_mosquitto
  [ -f "$FIBER_ROOT/etc/mosquitto/mosquitto.conf.fiber-backup" ]
  grep -q '^persistence true$' "$FIBER_ROOT/etc/mosquitto/mosquitto.conf.fiber-backup"
  [ "$(cat "$FIBER_ROOT/etc/mosquitto/mosquitto.conf")" = $'include_dir /etc/mosquitto/conf.d\ninclude_dir /run/proximos/mosquitto\ninclude_dir /etc/mosquitto/fiber.d' ]
}

@test "setup_mosquitto does not overwrite an existing backup on the second run" {
  setup_mosquitto
  run setup_mosquitto
  [[ "$output" == *"unchanged"* ]]
  grep -q '^persistence true$' "$FIBER_ROOT/etc/mosquitto/mosquitto.conf.fiber-backup"
}

@test "setup_hosts turns /etc/hosts into the symlink and seeds hosts.base from the old file" {
  setup_hosts
  [ "$(readlink "$FIBER_ROOT/etc/hosts")" = "/run/proximos/net/hosts" ]
  grep -q '^127.0.0.1' "$FIBER_ROOT/usr/share/proximos/hosts.base"
  grep -q 'ip6-localhost' "$FIBER_ROOT/usr/share/proximos/hosts.base"
  ! grep -q '^127.0.1.1' "$FIBER_ROOT/usr/share/proximos/hosts.base"
}

@test "setup_hosts is a no-op when the symlink already exists" {
  setup_hosts
  run setup_hosts
  [[ "$output" == *"unchanged"* ]]
}

@test "setup_nginx removes Debian's default site and installs ours after apt" {
  setup_nginx
  [ ! -e "$FIBER_ROOT/etc/nginx/sites-enabled/default" ]
  cmp "$FIBER_ROOT/etc/nginx/sites-enabled/fiber-portal.conf" "$FIBER_ROOT/usr/share/fiber/fiber-portal-nginx.conf"
  run setup_nginx
  [[ "$output" == *"unchanged"* ]]
}

@test "setup_mosquitto installs conf.d/proximos.conf too" {
  setup_mosquitto
  cmp "$FIBER_ROOT/etc/mosquitto/conf.d/proximos.conf" "$FIBER_ROOT/usr/share/fiber/mosquitto-proximos.conf"
}

@test "setup_hosts creates /run/proximos/net/hosts now, not only at the next boot" {
  setup_hosts
  grep -q '^127.0.0.1' "$FIBER_ROOT/run/proximos/net/hosts"
}

@test "setup_units fails when systemctl enable fails" {
  FIBER_SYSTEMCTL="$REPO_ROOT/tests/fixtures/systemctl-fail-enable" run setup_units
  [ "$status" -ne 0 ]
  [[ "$output" == *"enable"* ]]
}

@test "fiber_setup_all stops and fails when a step fails" {
  FIBER_SYSTEMCTL="$REPO_ROOT/tests/fixtures/systemctl-fail-enable" run fiber_setup_all
  [ "$status" -ne 0 ]
  [[ "$output" != *"setup done"* ]]
}

@test "the apt list carries hwclock (util-linux-extra) and rfkill" {
  [[ " $FIBER_APT_PACKAGES " == *" util-linux-extra "* ]]
  [[ " $FIBER_APT_PACKAGES " == *" rfkill "* ]]
}

@test "setup_firewall refuses a ruleset without the ssh accept" {
  sed -i '/tcp dport 22/d; /open_tcp/d' "$FIBER_ROOT/usr/share/proximos/firewall.nft"
  run setup_firewall
  [ "$status" -eq 1 ]
  [[ "$output" == *"22"* ]]
  ! grep -q 'enable.*proximos-firewall' "$SYSTEMCTL_LOG"
}

@test "setup_firewall enables the units when port 22 is accepted" {
  setup_firewall
  grep -q 'enable proximos-firewall.service proximos-firewall-server.service' "$SYSTEMCTL_LOG"
}

@test "setup_units reloads and enables exactly the list" {
  setup_units
  grep -q 'daemon-reload' "$SYSTEMCTL_LOG"
  for u in hostname-serial proximos-data-dirs proximos-data-migrate proximos-render proximos-tls-cert fiber-portal fiber proximos-control radio-setup proximos-first-run.path rtc-save.timer proximos-journal-prune fiber-bt-unblock mosquitto nginx redis-server avahi-daemon bluetooth; do
    grep -q "enable.* $u" "$SYSTEMCTL_LOG" || { echo "not enabled: $u"; false; }
  done
  # The network server and its credential bootstrap start at boot, as on the image;
  # the concentrator and the forwarder are started by udev when the RAK5146 appears.
  grep -qE 'enable.* chirpstack( |$)' "$SYSTEMCTL_LOG"
  grep -q 'enable.* chirpstack-bootstrap' "$SYSTEMCTL_LOG"
  ! grep -q 'enable.*chirpstack-concentratord' "$SYSTEMCTL_LOG"
  ! grep -q 'enable.*chirpstack-mqtt-forwarder' "$SYSTEMCTL_LOG"
}

@test "setup_bluetooth keeps the -E drop-in and setup_rtc disables fake-hwclock" {
  setup_bluetooth
  grep -q -- '-E' "$FIBER_ROOT/usr/lib/systemd/system/bluetooth.service.d/10-fiber.conf"
  setup_rtc
  grep -q 'disable --now fake-hwclock' "$SYSTEMCTL_LOG"
}

@test "setup_apt is skipped under FIBER_ROOT" {
  run setup_apt
  [ "$status" -eq 0 ]
  [[ "$output" == *"skipped"* ]]
}

@test "fiber_setup_all twice: the second run changes nothing" {
  fiber_setup_all > /dev/null
  (cd "$FIBER_ROOT" && find . -type f -exec sha256sum {} + | sort) > "$BATS_TEST_TMPDIR/a"
  run fiber_setup_all
  [ "$status" -eq 0 ]
  (cd "$FIBER_ROOT" && find . -type f -exec sha256sum {} + | sort) > "$BATS_TEST_TMPDIR/b"
  cmp "$BATS_TEST_TMPDIR/a" "$BATS_TEST_TMPDIR/b"
}

@test "setup_hosts stops cloud-init from managing /etc/hosts and the hostname" {
  setup_hosts
  local cfg="$FIBER_ROOT/etc/cloud/cloud.cfg.d/99-fiber.cfg"
  [ -f "$cfg" ]
  grep -q '^manage_etc_hosts: false$' "$cfg"
  grep -q '^preserve_hostname: true$' "$cfg"
}

@test "setup_mosquitto creates fiber.d, which mosquitto.conf includes" {
  setup_mosquitto
  [ -d "$FIBER_ROOT/etc/mosquitto/fiber.d" ]
}

@test "setup_units enables fiber-hosts" {
  setup_units
  grep -q "enable.* fiber-hosts" "$SYSTEMCTL_LOG"
}

@test "setup_cleanup removes files earlier releases installed and this one dropped" {
  mkdir -p "$FIBER_ROOT/usr/lib/tmpfiles.d" "$FIBER_ROOT/etc/mosquitto/conf.d"
  printf 'C /run/proximos/net/hosts\n' > "$FIBER_ROOT/usr/lib/tmpfiles.d/proximos-hosts.conf"
  printf 'listener 1884\n' > "$FIBER_ROOT/etc/mosquitto/conf.d/fiber-lan.conf"
  run setup_cleanup
  [ "$status" -eq 0 ]
  [ ! -e "$FIBER_ROOT/usr/lib/tmpfiles.d/proximos-hosts.conf" ]
  [ ! -e "$FIBER_ROOT/etc/mosquitto/conf.d/fiber-lan.conf" ]
  [[ "$output" == *"proximos-hosts.conf"* ]]
  [ "$(grep -o 'proximos-hosts.conf' <<< "$output" | wc -l)" -eq 1 ]
  run setup_cleanup
  [[ "$output" == *"unchanged"* ]]
}

@test "fiber_setup_all runs setup_cleanup" {
  grep -q 'setup_cleanup' "$REPO_ROOT/lib/fiber/setup.sh"
  grep -qE 'for step in .*setup_cleanup' "$REPO_ROOT/lib/fiber/setup.sh"
}
