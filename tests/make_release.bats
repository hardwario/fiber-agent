load test_helper

@test "packs build/root into a root-owned tarball with its sha256" {
  local root="$BATS_TEST_TMPDIR/root"; mkdir -p "$root/usr/bin" "$root/usr/share/fiber"
  printf 'x' > "$root/usr/bin/fiber-portal"; printf 'tag: vtest\n' > "$root/usr/share/fiber/upstream.lock"
  run "$REPO_ROOT/tools/make-release.sh" "$root" "$BATS_TEST_TMPDIR/dist"
  [ "$status" -eq 0 ]
  (cd "$BATS_TEST_TMPDIR/dist" && sha256sum -c fiber-agent-arm64.tar.gz.sha256)
  tar -tvzf "$BATS_TEST_TMPDIR/dist/fiber-agent-arm64.tar.gz" | grep -qE '(root/root|0/0) .*usr/bin/fiber-portal'
}

@test "refuses a tree without the portal binary" {
  mkdir -p "$BATS_TEST_TMPDIR/empty"
  run "$REPO_ROOT/tools/make-release.sh" "$BATS_TEST_TMPDIR/empty" "$BATS_TEST_TMPDIR/dist"
  [ "$status" -ne 0 ]
}
