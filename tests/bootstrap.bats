load test_helper

# bootstrap.sh against a local stand-in for the GitHub release: a tarball of a
# tiny tree plus its .sha256, served by python http.server.
setup() {
  REL="$BATS_TEST_TMPDIR/rel"; mkdir -p "$REL" "$BATS_TEST_TMPDIR/tree/usr/bin"
  printf '#!/bin/sh\necho fake fiber "$@"\n' > "$BATS_TEST_TMPDIR/tree/usr/bin/fiber"; chmod +x "$BATS_TEST_TMPDIR/tree/usr/bin/fiber"
  tar -C "$BATS_TEST_TMPDIR/tree" --owner=0 --group=0 -czf "$REL/fiber-agent-arm64.tar.gz" usr
  (cd "$REL" && sha256sum fiber-agent-arm64.tar.gz > fiber-agent-arm64.tar.gz.sha256)
  python3 -u -m http.server --bind 127.0.0.1 0 --directory "$REL" > "$BATS_TEST_TMPDIR/http.log" 2>&1 3>&- &
  HTTP_PID=$!
  for _ in $(seq 50); do PORT=$(sed -n 's/.*port \([0-9]*\).*/\1/p' "$BATS_TEST_TMPDIR/http.log" | head -n 1); [ -n "$PORT" ] && break; sleep 0.1; done
  export FIBER_RELEASE_URL="http://127.0.0.1:$PORT" FIBER_INSTALL_ROOT="$BATS_TEST_TMPDIR/root"
  export FIBER_SETUP_CMD="echo setup-ran" FIBER_NO_REBOOT=1 FIBER_ALLOW_NONROOT=1 FIBER_ARCH=aarch64
  mkdir -p "$FIBER_INSTALL_ROOT"
}
teardown() { kill "$HTTP_PID" 2>/dev/null || true; }

@test "downloads, verifies, extracts and runs setup" {
  run bash "$REPO_ROOT/bootstrap.sh"
  [ "$status" -eq 0 ]
  [ -x "$FIBER_INSTALL_ROOT/usr/bin/fiber" ]
  [[ "$output" == *"setup-ran"* ]]
}

@test "refuses a tarball whose sha256 does not match, and installs nothing" {
  printf 'tampered' >> "$REL/fiber-agent-arm64.tar.gz"
  run bash "$REPO_ROOT/bootstrap.sh"
  [ "$status" -ne 0 ]
  [[ "$output" == *"sha256"* ]]
  [ ! -e "$FIBER_INSTALL_ROOT/usr/bin/fiber" ]
  [[ "$output" != *"setup-ran"* ]]
}

@test "refuses to run as a normal user without the test override" {
  unset FIBER_ALLOW_NONROOT
  run bash "$REPO_ROOT/bootstrap.sh"
  [ "$status" -ne 0 ]
  [[ "$output" == *"sudo"* ]]
}

@test "refuses a machine that is not 64-bit ARM" {
  FIBER_ARCH=x86_64 run bash "$REPO_ROOT/bootstrap.sh"
  [ "$status" -ne 0 ]
  [[ "$output" == *"aarch64"* ]]
}
