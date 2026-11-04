load test_helper

@test "common.sh: fiber_die exits 1 with the message on stderr" {
  run bash -c "source '$REPO_ROOT/lib/fiber/common.sh'; fiber_die 'boom'"
  [ "$status" -eq 1 ]
  [[ "$output" == *"boom"* ]]
}

@test "common.sh: FIBER_ROOT defaults to empty" {
  run bash -c "source '$REPO_ROOT/lib/fiber/common.sh'; printf '%s' \"\${FIBER_ROOT}\""
  [ "$output" = "" ]
}
