load test_helper

setup() {
  export FIBER_LIBDIR="$REPO_ROOT/lib/fiber"
}

@test "help lists the commands and exits 0" {
  run "$REPO_ROOT/bin/fiber" help
  [ "$status" -eq 0 ]
  [[ "$output" == *"setup"* ]]; [[ "$output" == *"doctor"* ]]; [[ "$output" == *"radio"* ]]
  [[ "$output" == *"ble"* ]]; [[ "$output" == *"version"* ]]; [[ "$output" == *"status"* ]]
}

@test "no command prints usage and exits 2" {
  run "$REPO_ROOT/bin/fiber"
  [ "$status" -eq 2 ]
}

@test "unknown command exits 2" {
  run "$REPO_ROOT/bin/fiber" frobnicate
  [ "$status" -eq 2 ]
  [[ "$output" == *"frobnicate"* ]]
}

@test "radio forwards to proximosctl control.radio" {
  mkdir -p "$BATS_TEST_TMPDIR/bin"
  printf '#!/bin/sh\necho "$@"\n' > "$BATS_TEST_TMPDIR/bin/proximosctl"; chmod +x "$BATS_TEST_TMPDIR/bin/proximosctl"
  PATH="$BATS_TEST_TMPDIR/bin:$PATH" run "$REPO_ROOT/bin/fiber" radio node list
  [ "$output" = "control.radio node list" ]
}

@test "ble forwards to proximosctl control.ble" {
  mkdir -p "$BATS_TEST_TMPDIR/bin"
  printf '#!/bin/sh\necho "$@"\n' > "$BATS_TEST_TMPDIR/bin/proximosctl"; chmod +x "$BATS_TEST_TMPDIR/bin/proximosctl"
  PATH="$BATS_TEST_TMPDIR/bin:$PATH" run "$REPO_ROOT/bin/fiber" ble status
  [ "$output" = "control.ble status" ]
}

@test "setup refuses to run as a normal user" {
  run "$REPO_ROOT/bin/fiber" setup
  [ "$status" -eq 1 ]
  [[ "$output" == *"root"* ]]
}

@test "version prints the release first, then its details" {
  mkdir -p "$BATS_TEST_TMPDIR/root/usr/share/fiber"
  printf 'version: v0.1.3\nagent: 1234567\nhub: vhub 01234567\nbuilt: 2026-11-04T20:00:00Z\n' > "$BATS_TEST_TMPDIR/root/usr/share/fiber/release"
  FIBER_ROOT="$BATS_TEST_TMPDIR/root" run "$REPO_ROOT/bin/fiber" version
  [ "${lines[0]}" = "fiber-agent v0.1.3" ]
  [[ "$output" == *"hub: vhub 01234567"* ]]
}

@test "version without a release file says unreleased and names the hub build" {
  mkdir -p "$BATS_TEST_TMPDIR/root/usr/share/fiber"
  printf 'tag: vtest\ncommit: abc\n' > "$BATS_TEST_TMPDIR/root/usr/share/fiber/upstream.lock"
  FIBER_ROOT="$BATS_TEST_TMPDIR/root" run "$REPO_ROOT/bin/fiber" version
  [ "${lines[0]}" = "fiber-agent unreleased (hub vtest)" ]
}

@test "doctor runs fiber_doctor from the library" {
  mkdir -p "$BATS_TEST_TMPDIR/lib"
  cp "$REPO_ROOT/lib/fiber/common.sh" "$REPO_ROOT/lib/fiber/config-txt.sh" "$REPO_ROOT/lib/fiber/setup.sh" "$BATS_TEST_TMPDIR/lib/"
  printf 'fiber_doctor() { echo "doctor ran"; }\n' > "$BATS_TEST_TMPDIR/lib/doctor.sh"
  FIBER_LIBDIR="$BATS_TEST_TMPDIR/lib" run "$REPO_ROOT/bin/fiber" doctor
  [ "$output" = "doctor ran" ]
}
