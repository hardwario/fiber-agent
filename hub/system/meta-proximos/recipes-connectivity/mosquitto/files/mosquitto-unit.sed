# Edits upstream's mosquitto.service in place (mosquitto 2.1.2,
# service/systemd/mosquitto.service.notify). mosquitto_%.bbappend applies it at
# do_install; system/tests/test-mosquitto-unit.sh applies this same file to a
# copy of that unit, so the two cannot drift apart.
#
# Deletes the unit's network-online.target ordering and pull (GitLab #98).
# 10-proximos.conf orders the broker After=network.target instead; it says why.
# Exact lines only. If upstream spells them any other way they stay, and the
# bbappend's check after this script fails the build.
/^After=network-online\.target$/d
/^Wants=network-online\.target$/d
