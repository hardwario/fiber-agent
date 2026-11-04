#!/bin/sh
# Golden snapshot of the portal's data_dir (accounts, paired Hubs, thresholds).
#
#   golden.sh save      after the one-time manual setup: tar the volume
#   golden.sh restore   put it back; the nightly cron runs this so a day of
#                       customers acking alarms and changing passwords does
#                       not carry over
#
# The mock Hubs derive their CA from a fixed seed, so a restored snapshot is
# still paired with them.
set -eu
cd "$(dirname "$0")"
VOLUME=proximos-demo_portal-data
GOLDEN="$PWD/golden.tar.gz"

case "${1:-}" in
  save)
    docker compose stop portal
    docker run --rm -v "$VOLUME:/data/portal:ro" alpine tar czf - -C /data/portal . > "$GOLDEN"
    docker compose start portal
    ls -l "$GOLDEN"
    ;;
  restore)
    [ -f "$GOLDEN" ] || { echo "no $GOLDEN, run 'golden.sh save' first"; exit 1; }
    docker compose stop portal
    docker run --rm -v "$VOLUME:/data/portal" -v "$GOLDEN:/golden.tar.gz:ro" alpine \
      sh -c 'find /data/portal -mindepth 1 -delete && tar xzf /golden.tar.gz -C /data/portal'
    docker compose start portal
    ;;
  *)
    echo "usage: $0 save|restore"; exit 2
    ;;
esac
