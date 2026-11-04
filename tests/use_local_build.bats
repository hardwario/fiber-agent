load test_helper

setup() {
  CO="$BATS_TEST_TMPDIR/v2"; REL="$CO/portal/target/aarch64-unknown-linux-gnu/release"
  mkdir -p "$REL"; git -C "$BATS_TEST_TMPDIR" init -q v2; git -C "$CO" commit -q --allow-empty -m x
  for b in portal fiber_app fiberctl fiber-factory-reset proximos-control proximosctl; do printf '%s' "$b" > "$REL/$b"; done
  (cd "$REL" && sha256sum portal fiber_app fiberctl fiber-factory-reset proximos-control proximosctl > SHA256SUMS)
  export FIBER_UPSTREAM_DIR="$BATS_TEST_TMPDIR/up" FIBER_LOCK="$BATS_TEST_TMPDIR/up.lock"
}

@test "copies the six binaries, links the source and writes the lock" {
  run "$REPO_ROOT/tools/use-local-build.sh" "$CO" vlocal
  [ "$status" -eq 0 ]
  [ -f "$FIBER_UPSTREAM_DIR/vlocal/bin/proximosctl" ]
  [ -L "$FIBER_UPSTREAM_DIR/vlocal/src" ]
  grep -q '^tag: vlocal$' "$FIBER_LOCK"
}

@test "refuses binaries that do not match their SHA256SUMS" {
  printf 'tampered' > "$REL/fiberctl"
  run "$REPO_ROOT/tools/use-local-build.sh" "$CO" vlocal
  [ "$status" -ne 0 ]
}

@test "refuses a checkout without a cross build" {
  rm "$REL/SHA256SUMS"
  run "$REPO_ROOT/tools/use-local-build.sh" "$CO" vlocal
  [ "$status" -ne 0 ]
  [[ "$output" == *"make cross"* ]]
}
