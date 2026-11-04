load test_helper

setup() {
  source "$REPO_ROOT/lib/fiber/common.sh"
  source "$REPO_ROOT/lib/fiber/config-txt.sh"
  FRAG="$REPO_ROOT/boot/config-fragment.txt"
  CFG="$BATS_TEST_TMPDIR/config.txt"
}

@test "adds the three missing lines under [all] of a stock Bookworm config.txt" {
  cp "$REPO_ROOT/tests/fixtures/config.txt.bookworm" "$CFG"
  run config_txt_apply "$CFG" "$FRAG"
  [ "$status" -eq 0 ]
  [ "$(grep -c '^enable_uart=1$' "$CFG")" -eq 1 ]
  [ "$(grep -c '^arm_64bit=1$' "$CFG")" -eq 1 ]
  [ "$(grep -c '^otg_mode=1$' "$CFG")" -eq 1 ]
  [ "$(tail -n 4 "$CFG" | head -n 1)" = "[all]" ]
  [[ "$output" == *"added: dtoverlay=fiber"* ]]
}

@test "is idempotent" {
  cp "$REPO_ROOT/tests/fixtures/config.txt.bookworm" "$CFG"
  config_txt_apply "$CFG" "$FRAG"
  cp "$CFG" "$BATS_TEST_TMPDIR/once"
  run config_txt_apply "$CFG" "$FRAG"
  [ "$status" -eq 0 ]
  [ "$output" = "" ]
  cmp "$BATS_TEST_TMPDIR/once" "$CFG"
}

@test "creates [all] when the file has no section at all" {
  printf 'arm_64bit=1\n' > "$CFG"
  config_txt_apply "$CFG" "$FRAG"
  grep -q '^\[all\]$' "$CFG"
  [ "$(grep -c '^\[all\]$' "$CFG")" -eq 1 ]
}

@test "opens a new [all] block when [all] is not the last section" {
  printf '[all]\narm_64bit=1\n[cm4]\notg_mode=1\n' > "$CFG"
  config_txt_apply "$CFG" "$FRAG"
  [ "$(grep -n '^\[' "$CFG" | tail -n 1 | cut -d: -f2)" = "[all]" ]
  [ "$(grep -c '^enable_uart=1$' "$CFG")" -eq 1 ]
}
