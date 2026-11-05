load test_helper

setup() {
  export FIBER_HOSTS_DIR="$BATS_TEST_TMPDIR/run/proximos/net"
  export FIBER_HOSTS_BASE="$BATS_TEST_TMPDIR/hosts.base"
  printf '127.0.0.1\tlocalhost\n::1\t\tlocalhost ip6-localhost ip6-loopback\n' > "$FIBER_HOSTS_BASE"
  SEED="$REPO_ROOT/files/fiber-hosts-seed.sh"
}

@test "creates /run/proximos/net/hosts from hosts.base on an empty /run" {
  run "$SEED"
  [ "$status" -eq 0 ]
  grep -q '^127.0.0.1[[:space:]]localhost$' "$FIBER_HOSTS_DIR/hosts"
  [ "$(stat -c %a "$FIBER_HOSTS_DIR/hosts")" = "644" ]
}

@test "repairs a hosts file that lost localhost" {
  mkdir -p "$FIBER_HOSTS_DIR"; printf '127.0.1.1\tfiber-x\n' > "$FIBER_HOSTS_DIR/hosts"
  "$SEED"
  grep -q '^127.0.0.1[[:space:]]localhost$' "$FIBER_HOSTS_DIR/hosts"
  grep -q 'fiber-x' "$FIBER_HOSTS_DIR/hosts"
}

@test "leaves a complete hosts file alone (the agent's Server line survives)" {
  mkdir -p "$FIBER_HOSTS_DIR"
  printf '127.0.0.1\tlocalhost\n::1\t\tlocalhost ip6-localhost ip6-loopback\n192.168.1.9\tfiber-server.local\n' > "$FIBER_HOSTS_DIR/hosts"
  cp "$FIBER_HOSTS_DIR/hosts" "$BATS_TEST_TMPDIR/before"
  "$SEED"
  cmp "$BATS_TEST_TMPDIR/before" "$FIBER_HOSTS_DIR/hosts"
}

@test "the unit runs the seed as root early, before the hostname and the broker" {
  local u="$REPO_ROOT/files/fiber-hosts.service"
  grep -q '^ExecStart=/usr/lib/fiber/fiber-hosts-seed.sh$' "$u"
  grep -q '^DefaultDependencies=no$' "$u"
  grep -qE '^Before=.*hostname-serial.service' "$u"
  grep -qE '^Before=.*mosquitto.service' "$u"
  grep -q '^WantedBy=sysinit.target$' "$u"
}
