load test_helper

setup() {
  source "$REPO_ROOT/lib/fiber/common.sh"
  source "$REPO_ROOT/lib/fiber/doctor.sh"
  CHECKS="$BATS_TEST_TMPDIR/checks"; mkdir -p "$CHECKS"
  printf '#!/bin/sh\necho fine\n' > "$CHECKS/10-ok.sh"
  printf '#!/bin/sh\necho nope >&2\nexit 1\n' > "$CHECKS/20-bad.sh"
  printf '#!/bin/sh\nexit 77\n' > "$CHECKS/30-skip.sh"
  chmod +x "$CHECKS"/*.sh
  export FIBER_HEALTH_DIR="$CHECKS"
  # No hardware on the host: every probe command points at a name that does not exist.
  export FIBER_I2CDETECT=/nonexistent FIBER_BLUETOOTHCTL=/nonexistent
  export FIBER_DEV="$BATS_TEST_TMPDIR/dev" FIBER_SYS="$BATS_TEST_TMPDIR/sys"
}

@test "reports pass, fail and skip and exits 1 on a failure" {
  run fiber_doctor
  [ "$status" -eq 1 ]
  [[ "$output" == *"PASS 10-ok.sh"* ]]
  [[ "$output" == *"FAIL 20-bad.sh: nope"* ]]
  [[ "$output" == *"SKIP 30-skip.sh"* ]]
}

@test "exits 0 when nothing fails" {
  rm "$CHECKS/20-bad.sh"
  run fiber_doctor
  [ "$status" -eq 0 ]
}

@test "hardware probes print n/a or absent on the host" {
  run fiber_doctor
  [[ "$output" == *"i2c: n/a"* ]]
  [[ "$output" == *"bt: n/a"* ]]
  [[ "$output" == *"rak5146: absent"* ]]
  [[ "$output" == *"uart:"* ]]
  [[ "$output" == *"landlock: n/a"* ]]
}

@test "hardware probes read the fake dev and sys trees" {
  mkdir -p "$FIBER_DEV" "$FIBER_SYS/kernel/security" "$FIBER_SYS/bus/w1/devices/28-0001" "$FIBER_SYS/bus/w1/devices/28-0002"
  touch "$FIBER_DEV/ttyAMA3" "$FIBER_DEV/ttyAMA4" "$FIBER_DEV/rak5146" "$FIBER_DEV/rtc0"
  printf 'landlock,lockdown,yama\n' > "$FIBER_SYS/kernel/security/lsm"
  run fiber_doctor
  [[ "$output" == *"uart: ttyAMA3 ttyAMA4"* ]]
  [[ "$output" == *"rak5146: present"* ]]
  [[ "$output" == *"rtc: rtc0 present"* ]]
  [[ "$output" == *"w1: 2"* ]]
  [[ "$output" == *"landlock: landlock,lockdown,yama"* ]]
}

@test "through bin/fiber (set -e, pipefail): a /sys without w1 still prints every line" {
  mkdir -p "$FIBER_SYS/kernel/security"; printf 'capability\n' > "$FIBER_SYS/kernel/security/lsm"
  FIBER_LIBDIR="$REPO_ROOT/lib/fiber" run "$REPO_ROOT/bin/fiber" doctor
  [[ "$output" == *"w1: 0"* ]]
  [[ "$output" == *"landlock: capability"* ]]
}

@test "i2c: bound devices show as 18(bound) 51(bound), free ones as plain addresses" {
  cat > "$BATS_TEST_TMPDIR/i2cdetect" <<'STUB'
#!/bin/sh
cat <<'GRID'
     0  1  2  3  4  5  6  7  8  9  a  b  c  d  e  f
00:                         -- -- -- -- -- -- -- --
10: -- -- -- -- -- -- -- -- UU -- -- -- -- -- -- --
20: -- -- -- -- -- -- -- -- -- -- -- -- -- -- -- --
30: -- -- -- -- -- -- -- -- -- -- -- -- -- -- -- --
40: -- -- -- -- -- -- -- -- 48 -- -- -- -- -- -- --
50: -- UU -- -- -- -- -- -- -- -- -- -- -- -- -- --
60: -- -- -- -- -- -- -- -- -- -- -- -- -- -- -- --
70: -- -- -- -- -- -- -- --
GRID
STUB
  chmod +x "$BATS_TEST_TMPDIR/i2cdetect"
  FIBER_I2CDETECT="$BATS_TEST_TMPDIR/i2cdetect" run fiber_doctor
  [[ "$output" == *"i2c: 18(bound) 48 51(bound)"* ]]
}

@test "bt: a bluetoothctl that never answers is cut off by the timeout" {
  printf '#!/bin/sh\nsleep 5\n' > "$BATS_TEST_TMPDIR/bluetoothctl"; chmod +x "$BATS_TEST_TMPDIR/bluetoothctl"
  FIBER_BLUETOOTHCTL="$BATS_TEST_TMPDIR/bluetoothctl" FIBER_TIMEOUT=1 run fiber_doctor
  [[ "$output" == *"bt: timeout"* ]]
  [[ "$output" == *"landlock:"* ]]
}

@test "i2c parsing works with mawk (Pi OS's awk), not only gawk" {
  command -v mawk >/dev/null || skip "mawk not installed"
  mkdir -p "$BATS_TEST_TMPDIR/awkbin"; ln -s "$(command -v mawk)" "$BATS_TEST_TMPDIR/awkbin/awk"
  cat > "$BATS_TEST_TMPDIR/i2cdetect" <<'STUB'
#!/bin/sh
printf '     0  1  2  3  4  5  6  7  8  9  a  b  c  d  e  f\n'
printf '10: -- -- -- -- -- -- -- -- UU -- -- -- -- -- -- -- \n'
printf '50: -- UU -- -- -- -- -- -- -- -- -- -- -- -- -- -- \n'
STUB
  chmod +x "$BATS_TEST_TMPDIR/i2cdetect"
  PATH="$BATS_TEST_TMPDIR/awkbin:$PATH" FIBER_I2CDETECT="$BATS_TEST_TMPDIR/i2cdetect" run fiber_doctor
  [[ "$output" == *"i2c: 18(bound) 51(bound)"* ]]
  [[ "$output" != *"strtonum"* ]]
}

@test "i2c parsing: row 00 starts at 03 and row 70 ends at 77" {
  cat > "$BATS_TEST_TMPDIR/i2cdetect" <<'STUB'
#!/bin/sh
printf '     0  1  2  3  4  5  6  7  8  9  a  b  c  d  e  f\n'
printf '00:          03 -- -- -- -- -- -- -- -- -- -- -- -- \n'
printf '70: -- -- 72 -- -- -- -- 77                         \n'
STUB
  chmod +x "$BATS_TEST_TMPDIR/i2cdetect"
  FIBER_I2CDETECT="$BATS_TEST_TMPDIR/i2cdetect" run fiber_doctor
  [[ "$output" == *"i2c: 03 72 77"* ]]
}
