#!/bin/sh
set -eu

LSM="${PROXIMOS_HEALTH_LSM:-/sys/kernel/security/lsm}"

if ! lsms=$(cat "$LSM" 2>/dev/null); then
    echo "70-landlock: cannot read $LSM - the kernel has no LSM framework, so the Portal runs unconfined" >&2
    exit 1
fi

case ",$lsms," in
    *,landlock,*)
        echo "70-landlock: landlock is active (lsm: $lsms)"
        ;;
    *)
        echo "70-landlock: the kernel does not offer landlock (lsm: ${lsms:-empty}) - the Portal runs unconfined" >&2
        exit 1
        ;;
esac
