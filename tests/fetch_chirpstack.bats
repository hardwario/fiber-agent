load test_helper

setup() {
  BASE="$BATS_TEST_TMPDIR/base"
  cp -r "$REPO_ROOT/tests/fixtures/chirpstack" "$BASE"
  python3 -u -m http.server --bind 127.0.0.1 0 --directory "$BASE" \
    > "$BATS_TEST_TMPDIR/http.log" 2>&1 3>&- &
  HTTP_PID=$!
  for _ in $(seq 50); do
    PORT=$(sed -n 's/.*port \([0-9]*\).*/\1/p' "$BATS_TEST_TMPDIR/http.log" | head -n 1)
    [ -n "$PORT" ] && break
    sleep 0.1
  done
  [ -n "$PORT" ]
  export CHIRPSTACK_BASE="http://127.0.0.1:$PORT"
  export CHIRPSTACK_LOCK="$BASE/chirpstack.lock.test"
  export FIBER_UPSTREAM_DIR="$BATS_TEST_TMPDIR/upstream"
}

teardown() { kill "$HTTP_PID" 2>/dev/null || true; }

@test "extracts the three binaries" {
  run "$REPO_ROOT/tools/fetch-chirpstack.sh"
  [ "$status" -eq 0 ]
  [ -x "$FIBER_UPSTREAM_DIR/chirpstack/chirpstack" ]
  [ -x "$FIBER_UPSTREAM_DIR/chirpstack/chirpstack-concentratord-sx1302" ]
  [ -x "$FIBER_UPSTREAM_DIR/chirpstack/chirpstack-mqtt-forwarder" ]
  [ "$("$FIBER_UPSTREAM_DIR/chirpstack/chirpstack")" = "stand-in chirpstack" ]
}

@test "sha256 mismatch exits 4 and removes upstream/chirpstack" {
  printf 'tampered' > "$BASE/chirpstack-mqtt-forwarder/chirpstack-mqtt-forwarder_4.6.1_linux_arm64.tar.gz"
  run "$REPO_ROOT/tools/fetch-chirpstack.sh"
  [ "$status" -eq 4 ]
  [[ "$output" == *"chirpstack-mqtt-forwarder"* ]]
  [ ! -d "$FIBER_UPSTREAM_DIR/chirpstack" ]
}

@test "a missing tarball exits 3 with the URL" {
  rm "$BASE/chirpstack/chirpstack_4.19.1_sqlite_linux_arm64.tar.gz"
  run "$REPO_ROOT/tools/fetch-chirpstack.sh"
  [ "$status" -eq 3 ]
  [[ "$output" == *"chirpstack/chirpstack_4.19.1_sqlite_linux_arm64.tar.gz"* ]]
}

@test "the committed chirpstack.lock has three pinned rows with 64-hex hashes" {
  [ "$(grep -cv '^#' "$REPO_ROOT/chirpstack.lock")" -eq 3 ]
  grep -v '^#' "$REPO_ROOT/chirpstack.lock" | awk '{ if (length($4) != 64) exit 1 }'
}
