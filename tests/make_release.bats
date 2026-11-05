load test_helper

@test "packs build/root into a root-owned tarball with its sha256" {
  local root="$BATS_TEST_TMPDIR/root"; mkdir -p "$root/usr/bin" "$root/usr/share/fiber"
  printf 'x' > "$root/usr/bin/fiber-portal"; printf 'tag: vtest\n' > "$root/usr/share/fiber/upstream.lock"
  run "$REPO_ROOT/tools/make-release.sh" "$root" "$BATS_TEST_TMPDIR/dist" v9.8.7
  [ "$status" -eq 0 ]
  (cd "$BATS_TEST_TMPDIR/dist" && sha256sum -c fiber-agent-arm64.tar.gz.sha256)
  tar -tvzf "$BATS_TEST_TMPDIR/dist/fiber-agent-arm64.tar.gz" | grep -qE '(root/root|0/0) .*usr/bin/fiber-portal'
}

@test "refuses a tree without the portal binary" {
  mkdir -p "$BATS_TEST_TMPDIR/empty"
  run "$REPO_ROOT/tools/make-release.sh" "$BATS_TEST_TMPDIR/empty" "$BATS_TEST_TMPDIR/dist" v9.8.7
  [ "$status" -ne 0 ]
}

@test "the release records its version, the agent commit and the hub build inside the tarball" {
  local root="$BATS_TEST_TMPDIR/root"; mkdir -p "$root/usr/bin" "$root/usr/share/fiber"
  printf 'x' > "$root/usr/bin/fiber-portal"; printf 'tag: vhub\ncommit: 0123456789abcdef\n' > "$root/usr/share/fiber/upstream.lock"
  "$REPO_ROOT/tools/make-release.sh" "$root" "$BATS_TEST_TMPDIR/dist" v9.8.7
  run tar -xOzf "$BATS_TEST_TMPDIR/dist/fiber-agent-arm64.tar.gz" ./usr/share/fiber/release
  [[ "$output" == *"version: v9.8.7"* ]]
  [[ "$output" == *"hub: vhub 01234567"* ]]
  [[ "$output" =~ agent:\ [0-9a-f]{7} ]]
  [[ "$output" =~ built:\ 20[0-9]{2}- ]]
}

@test "a version that is not vX.Y.Z is refused" {
  local root="$BATS_TEST_TMPDIR/root"; mkdir -p "$root/usr/bin"; printf 'x' > "$root/usr/bin/fiber-portal"
  run "$REPO_ROOT/tools/make-release.sh" "$root" "$BATS_TEST_TMPDIR/dist" vfiber
  [ "$status" -ne 0 ]
}
