load test_helper

V2_SRC="${V2_SRC:-$HOME/Projects/fiber/v2/proximos-v2}"

setup_file() {
  export FIBER_UPSTREAM_DIR="$BATS_FILE_TMPDIR/upstream"
  export FIBER_LOCK="$BATS_FILE_TMPDIR/upstream.lock"
  mkdir -p "$FIBER_UPSTREAM_DIR/vtest/bin" "$FIBER_UPSTREAM_DIR/chirpstack"
  ln -s "$V2_SRC" "$FIBER_UPSTREAM_DIR/vtest/src"
  for b in portal fiber_app fiberctl fiber-factory-reset proximos-control proximosctl; do
    printf '#!/bin/sh\n' > "$FIBER_UPSTREAM_DIR/vtest/bin/$b"; chmod +x "$FIBER_UPSTREAM_DIR/vtest/bin/$b"
  done
  printf 'x\n' > "$FIBER_UPSTREAM_DIR/vtest/bin/SHA256SUMS"
  for b in chirpstack chirpstack-concentratord-sx1302 chirpstack-mqtt-forwarder; do
    printf '#!/bin/sh\n' > "$FIBER_UPSTREAM_DIR/chirpstack/$b"; chmod +x "$FIBER_UPSTREAM_DIR/chirpstack/$b"
  done
  printf 'tag: vtest\n' > "$FIBER_LOCK"
  make -C "$REPO_ROOT" dtbo > /dev/null
  export ROOT="$BATS_FILE_TMPDIR/root"
  "$REPO_ROOT/tools/stage.sh" vtest "$ROOT" > "$BATS_FILE_TMPDIR/stage.log" 2>&1 || true
}

@test "stage.sh exits 0" {
  run "$REPO_ROOT/tools/stage.sh" vtest "$BATS_TEST_TMPDIR/again"
  [ "$status" -eq 0 ]
}

@test "stages every path of the manifest" {
  while read -r p; do
    [ -e "$ROOT/$p" ] || { echo "missing $p"; false; }
  done < "$REPO_ROOT/tests/fixtures/stage-manifest.txt"
}

@test "portal is renamed fiber-portal and is executable" {
  [ -x "$ROOT/usr/bin/fiber-portal" ]
  [ ! -e "$ROOT/usr/bin/portal" ]
}

@test "hostname prefix is fiber-" {
  grep -q '^PREFIX="fiber-"$' "$ROOT/usr/bin/set-hostname.sh"
}

@test "nginx snippet uses the 1.22 http2 syntax" {
  grep -qE '^\s*listen 443 ssl http2;' "$ROOT/usr/share/fiber/fiber-portal-nginx.conf"
  ! grep -q 'http2 on;' "$ROOT/usr/share/fiber/fiber-portal-nginx.conf"
}

@test "health checks 10-data-mount, 30-qbee, 50-data-grown and 70-landlock are not staged" {
  for f in 10-data-mount 30-qbee 50-data-grown 70-landlock; do
    [ ! -e "$ROOT/usr/share/proximos/health-checks.d/$f.sh" ]
  done
  [ -x "$ROOT/usr/share/proximos/health-checks.d/10-data-dir.sh" ]
  [ -x "$ROOT/usr/share/proximos/health-checks.d/60-services.sh" ]
}

@test "units are unchanged copies" {
  cmp "$ROOT/usr/lib/systemd/system/fiber.service" "$V2_SRC/portal/packaging/fiber.service"
  cmp "$ROOT/usr/lib/systemd/system/chirpstack.service" "$V2_SRC/system/meta-proximos/recipes-connectivity/chirpstack/files/chirpstack.service"
}

@test "scripts are executable and data files are not" {
  [ -x "$ROOT/usr/libexec/proximos-data-migrate" ]
  [ -x "$ROOT/usr/lib/proximos/migrations/010-radio-init.sh" ]
  [ ! -x "$ROOT/usr/lib/systemd/system/fiber.service" ]
  [ ! -x "$ROOT/usr/share/proximos/firewall.nft" ]
}

@test "DESTDIR is wiped before staging" {
  mkdir -p "$BATS_TEST_TMPDIR/wipe"; touch "$BATS_TEST_TMPDIR/wipe/stale"
  "$REPO_ROOT/tools/stage.sh" vtest "$BATS_TEST_TMPDIR/wipe" > /dev/null
  [ ! -e "$BATS_TEST_TMPDIR/wipe/stale" ]
}

@test "the lock is staged so fiber version can read the tag" {
  grep -q '^tag: vtest$' "$ROOT/usr/share/fiber/upstream.lock"
}

@test "Debian's mosquitto.conf is not overwritten by the tree; ours is staged aside" {
  [ ! -e "$ROOT/etc/mosquitto/mosquitto.conf" ]
  [ -f "$ROOT/usr/share/fiber/mosquitto.conf" ]
}

@test "the tree touches nothing under /etc/nginx, /etc/mosquitto or /boot (installed by fiber setup after apt)" {
  [ ! -e "$ROOT/etc/nginx" ]; [ ! -e "$ROOT/etc/mosquitto" ]; [ ! -e "$ROOT/boot" ]
}

@test "data-dirs gets a drop-in ordering it after the root remount" {
  grep -q '^After=systemd-remount-fs.service$' "$ROOT/usr/lib/systemd/system/proximos-data-dirs.service.d/10-fiber.conf"
}

@test "stage refuses a lock whose tag is not the one being staged" {
  printf 'tag: vother\n' > "$BATS_TEST_TMPDIR/wrong.lock"
  FIBER_LOCK="$BATS_TEST_TMPDIR/wrong.lock" run "$REPO_ROOT/tools/stage.sh" vtest "$BATS_TEST_TMPDIR/x"
  [ "$status" -ne 0 ]
  [[ "$output" == *"vother"* ]]
}

@test "stages cleanly from the v1.1.0 tag of proximos-v2 (not only from main)" {
  local up="$BATS_TEST_TMPDIR/up"
  mkdir -p "$up/v1.1.0/bin" "$up/v1.1.0/src" "$up/chirpstack"
  git -C "$V2_SRC" archive v1.1.0 | tar -x -C "$up/v1.1.0/src"
  cp "$FIBER_UPSTREAM_DIR/vtest/bin/"* "$up/v1.1.0/bin/"
  cp "$FIBER_UPSTREAM_DIR/chirpstack/"* "$up/chirpstack/"
  printf 'tag: v1.1.0\n' > "$BATS_TEST_TMPDIR/v110.lock"
  FIBER_UPSTREAM_DIR="$up" FIBER_LOCK="$BATS_TEST_TMPDIR/v110.lock" run "$REPO_ROOT/tools/stage.sh" v1.1.0 "$BATS_TEST_TMPDIR/v110"
  [ "$status" -eq 0 ]
  [ -x "$BATS_TEST_TMPDIR/v110/usr/bin/set-hostname.sh" ]
}

@test "the brand's brand.env is staged when upstream has portal/branding (FIBER_BRAND, default fiber)" {
  local up="$BATS_TEST_TMPDIR/up"
  mkdir -p "$up/vb/bin" "$up/vb/src/portal/branding/fiber" "$up/vb/src/system" "$up/chirpstack"
  # Links, not a copy: only what stage.sh reads, plus a branding directory.
  for d in packaging templates static crates; do ln -s "$V2_SRC/portal/$d" "$up/vb/src/portal/$d"; done
  ln -s "$V2_SRC/system/meta-proximos" "$up/vb/src/system/meta-proximos"
  printf "BRAND_ID='fiber'\nBRAND_HOSTNAME_PREFIX='fiber-'\n" > "$up/vb/src/portal/branding/fiber/brand.env"
  cp "$FIBER_UPSTREAM_DIR/vtest/bin/"* "$up/vb/bin/"; cp "$FIBER_UPSTREAM_DIR/chirpstack/"* "$up/chirpstack/"
  printf 'tag: vb\n' > "$BATS_TEST_TMPDIR/vb.lock"
  FIBER_UPSTREAM_DIR="$up" FIBER_LOCK="$BATS_TEST_TMPDIR/vb.lock" run "$REPO_ROOT/tools/stage.sh" vb "$BATS_TEST_TMPDIR/vbroot"
  [ "$status" -eq 0 ]
  grep -q "^BRAND_ID='fiber'$" "$BATS_TEST_TMPDIR/vbroot/usr/share/proximos/brand.env"
}

@test "no brand.env is staged when upstream predates the brand switch" {
  [ ! -e "$ROOT/usr/share/proximos/brand.env" ] || [ -d "$V2_SRC/portal/branding" ]
}

@test "firmware packages are staged when FIBER_FIRMWARE_DIR is given" {
  local fw="$BATS_TEST_TMPDIR/fw"
  mkdir -p "$fw/nb-package" "$fw/sb-package"
  for f in nb-package/northbridge.bin nb-package/northbridge.json nb-package/northbridge_jlink.hex sb-package/southbridge.bin sb-package/southbridge.json; do printf x > "$fw/$f"; done
  FIBER_FIRMWARE_DIR="$fw" run "$REPO_ROOT/tools/stage.sh" vtest "$BATS_TEST_TMPDIR/fwroot"
  [ "$status" -eq 0 ]
  [ -f "$BATS_TEST_TMPDIR/fwroot/usr/share/proximos/northbridge/northbridge.bin" ]
  [ -f "$BATS_TEST_TMPDIR/fwroot/usr/share/proximos/northbridge/northbridge.json" ]
  [ -f "$BATS_TEST_TMPDIR/fwroot/usr/share/proximos/southbridge/southbridge.bin" ]
  [ -f "$BATS_TEST_TMPDIR/fwroot/usr/share/proximos/southbridge/southbridge.json" ]
  [ -f "$BATS_TEST_TMPDIR/fwroot/usr/share/fiber/stm32/northbridge_jlink.hex" ]
}

@test "no firmware is staged without FIBER_FIRMWARE_DIR" {
  [ ! -e "$ROOT/usr/share/proximos/northbridge" ]
}

@test "the STM32 UART tools are staged under /usr/share/fiber/stm32" {
  [ -x "$ROOT/usr/share/fiber/stm32/nb_uart_flash.py" ]
  [ -x "$ROOT/usr/share/fiber/stm32/an3155_probe.py" ]
}
