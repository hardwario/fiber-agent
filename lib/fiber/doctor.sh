# shellcheck shell=bash
# fiber_doctor: the image's health checks as a report (design §4.7), then a
# hardware section. Exit 1 if any check FAILed. Every probe is overridable so
# the tests run on a host without the hardware.

: "${FIBER_HEALTH_DIR:=/usr/share/proximos/health-checks.d}"
: "${FIBER_I2CDETECT:=i2cdetect}"
: "${FIBER_BLUETOOTHCTL:=bluetoothctl}"
: "${FIBER_DEV:=/dev}"
: "${FIBER_SYS:=/sys}"
: "${FIBER_TIMEOUT:=5}"

_doctor_checks() { # sets DOCTOR_FAILED
    DOCTOR_FAILED=0
    local check name out status
    for check in "$FIBER_HEALTH_DIR"/*; do
        if [ ! -f "$check" ] || [ ! -x "$check" ]; then continue; fi
        name=${check##*/}
        if out=$("$check" 2>&1 >/dev/null); then status=0; else status=$?; fi
        case "$status" in
            0) printf 'PASS %s\n' "$name" ;;
            77) printf 'SKIP %s\n' "$name" ;;
            *) printf 'FAIL %s: %s\n' "$name" "$(printf '%s' "$out" | tail -n 1)"; DOCTOR_FAILED=1 ;;
        esac
    done
}

_doctor_i2c() { # parse i2cdetect's grid: "18(bound)" for UU, the address for a free device
    # POSIX awk only (Pi OS ships mawk): the row label "50:" is the high nibble,
    # the cell's position counted from the right edge is the low one (row 00
    # starts at column 3, so counting from the left would be wrong there);
    # row 70 is the exception, it has only 70..77 and is counted from the left.
    awk 'NR > 1 { hi = substr($1, 1, 1); for (i = NF; i >= 2; i--) {
            if (hi == "7") lo = i - 2; else lo = 16 - (NF - i) - 1   # row 70 stops at 77
            if ($i == "--") continue
            a = hi sprintf("%x", lo)
            if ($i == "UU") cell = a "(bound)"; else cell = a
            line = cell (line == "" ? "" : " ") line }
            out = out (out == "" || line == "" ? "" : " ") line; line = "" }
         END { print out }'
}

_doctor_hardware() {
    local i2c bt uarts="" t n
    if command -v "$FIBER_I2CDETECT" >/dev/null 2>&1; then
        i2c=$(timeout "$FIBER_TIMEOUT" "$FIBER_I2CDETECT" -y 10 2>/dev/null | _doctor_i2c) || i2c=""
        printf 'i2c: %s\n' "${i2c:-none on bus 10}"
    else printf 'i2c: n/a\n'; fi
    for t in ttyAMA3 ttyAMA4; do [ -e "$FIBER_DEV/$t" ] && uarts="$uarts $t"; done
    printf 'uart:%s\n' "${uarts:- none}"
    if [ -e "$FIBER_DEV/rak5146" ]; then printf 'rak5146: present\n'; else printf 'rak5146: absent\n'; fi
    if [ -e "$FIBER_DEV/rtc0" ]; then printf 'rtc: rtc0 present\n'; else printf 'rtc: rtc0 absent\n'; fi
    if command -v "$FIBER_BLUETOOTHCTL" >/dev/null 2>&1; then
        if bt=$(timeout "$FIBER_TIMEOUT" "$FIBER_BLUETOOTHCTL" show 2>/dev/null); then
            bt=$(printf '%s' "$bt" | sed -n 's/^\s*Powered: //p' | head -n 1)
            printf 'bt: %s\n' "${bt:-no adapter}"
        else printf 'bt: timeout (is bluetoothd running?)\n'; fi
    else printf 'bt: n/a\n'; fi
    n=0
    if [ -d "$FIBER_SYS/bus/w1/devices" ]; then
        n=$(find "$FIBER_SYS/bus/w1/devices" -maxdepth 1 -name '28-*' | wc -l) || n=0
    fi
    printf 'w1: %s\n' "$n"
    if [ -r "$FIBER_SYS/kernel/security/lsm" ]; then printf 'landlock: %s\n' "$(cat "$FIBER_SYS/kernel/security/lsm")"
    else printf 'landlock: n/a\n'; fi
}

fiber_doctor() {
    printf -- '--- health checks (%s) ---\n' "$FIBER_HEALTH_DIR"
    _doctor_checks
    printf -- '--- hardware ---\n'
    _doctor_hardware
    [ "$DOCTOR_FAILED" -eq 0 ]
}
