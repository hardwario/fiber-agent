load test_helper

# A local stand-in registry (python http.server over a copy of the fixture) and
# a throwaway shared clone of proximos-v2 carrying a v9.9.9 tag.
V2_SRC="${V2_SRC:-$HOME/Projects/fiber/v2/proximos-v2}"

setup() {
  REGISTRY="$BATS_TEST_TMPDIR/registry"
  cp -r "$REPO_ROOT/tests/fixtures/registry" "$REGISTRY"
  python3 -u -m http.server --bind 127.0.0.1 0 --directory "$REGISTRY" \
    > "$BATS_TEST_TMPDIR/http.log" 2>&1 3>&- &
  HTTP_PID=$!
  for _ in $(seq 50); do
    PORT=$(sed -n 's/.*port \([0-9]*\).*/\1/p' "$BATS_TEST_TMPDIR/http.log" | head -n 1)
    [ -n "$PORT" ] && break
    sleep 0.1
  done
  [ -n "$PORT" ]
  TOKEN="$BATS_TEST_TMPDIR/token"; printf 'secret\n' > "$TOKEN"; chmod 0600 "$TOKEN"
  V2="$BATS_TEST_TMPDIR/v2"
  git clone -q --shared --no-checkout "$V2_SRC" "$V2"
  git -C "$V2" tag v9.9.9 HEAD
  export FIBER_UPSTREAM_DIR="$BATS_TEST_TMPDIR/upstream"
  export FIBER_LOCK="$BATS_TEST_TMPDIR/upstream.lock"
  export REGISTRY_BASE="http://127.0.0.1:$PORT" GITLAB_TOKEN_FILE="$TOKEN" UPSTREAM_GIT="$V2"
}

teardown() { kill "$HTTP_PID" 2>/dev/null || true; }

@test "fetches, verifies and writes upstream.lock" {
  run "$REPO_ROOT/tools/fetch-upstream.sh" v9.9.9
  [ "$status" -eq 0 ]
  [ -x "$FIBER_UPSTREAM_DIR/v9.9.9/bin/proximosctl" ]
  [ -f "$FIBER_UPSTREAM_DIR/v9.9.9/bin/SHA256SUMS" ]
  [ -f "$FIBER_UPSTREAM_DIR/v9.9.9/src/portal/packaging/fiber-portal.service" ]
  [ -f "$FIBER_UPSTREAM_DIR/v9.9.9/src/system/meta-proximos/recipes-core/firewall/files/firewall.nft" ]
  [ ! -e "$FIBER_UPSTREAM_DIR/v9.9.9/src/portal/src" ]
  grep -q '^tag: v9.9.9$' "$FIBER_LOCK"
  [ "$(grep -c '  bin/' "$FIBER_LOCK")" -eq 6 ]
}

@test "refuses a sha256 mismatch and leaves no tree behind" {
  printf 'tampered' > "$REGISTRY/proximos-portal-aarch64/v9.9.9/fiberctl"
  run "$REPO_ROOT/tools/fetch-upstream.sh" v9.9.9
  [ "$status" -eq 4 ]
  [[ "$output" == *"fiberctl"* ]]
  [ ! -d "$FIBER_UPSTREAM_DIR/v9.9.9" ]
}

@test "a tag absent from the registry fails with the URL" {
  run "$REPO_ROOT/tools/fetch-upstream.sh" v0.0.0
  [ "$status" -eq 3 ]
  [[ "$output" == *"proximos-portal-aarch64/v0.0.0/SHA256SUMS"* ]]
  [ ! -d "$FIBER_UPSTREAM_DIR/v0.0.0" ]
}

@test "a token file that is not 0600 is refused" {
  chmod 0644 "$TOKEN"
  run "$REPO_ROOT/tools/fetch-upstream.sh" v9.9.9
  [ "$status" -eq 2 ]
}

@test "no tag argument is a usage error" {
  run "$REPO_ROOT/tools/fetch-upstream.sh"
  [ "$status" -eq 2 ]
}
