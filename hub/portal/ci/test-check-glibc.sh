#!/bin/sh
# Stub-driven tests for portal/ci/check-glibc.sh.
#
# The guard decides whether a cross-built binary may go to a Hub, so the cases
# that matter are the ones a real build rarely produces: a glibc past the Hub's,
# GLIBC_PRIVATE, an unknown library, a host binary, and above all readelf
# output the parser does not understand, which must fail instead of passing
# quietly. Real ELF files cannot be committed (CLAUDE.md, no binaries), so
# readelf is replaced by a stub that answers from canned text in the format GNU
# readelf prints. portal:build-aarch64 runs the guard on the real binaries, and
# runs this file before it.
#
#   sh portal/ci/test-check-glibc.sh
set -eu

here=$(cd "$(dirname "$0")" && pwd)
guard="$here/check-glibc.sh"

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT INT TERM

failures=0
cases=0

ok() { printf 'ok   %s\n' "$1"; }
bad() {
    printf 'FAIL %s\n' "$1"
    failures=$((failures + 1))
}
show() { sed 's/^/       | /' "$1"; }

# The stand-in: `readelf <mode> [-W] <file>` prints <file>.<mode> (h, l, V, d).
cat > "$work/readelf" <<'EOF'
#!/bin/sh
for arg in "$@"; do file=$arg; done
case "$1" in
    -h) mode=h ;;
    -l) mode=l ;;
    -V) mode=V ;;
    -d) mode=d ;;
    *) echo "stub readelf: unexpected $*" >&2; exit 2 ;;
esac
cat "$file.$mode"
EOF
chmod +x "$work/readelf"

# fixture NAME MACHINE INTERPRETER "GLIBC-VERSIONS" "NEEDED-SONAMES"
# An empty INTERPRETER, version list or soname list leaves that part out.
fixture() {
    f="$work/$1"
    : > "$f"
    printf 'ELF Header:\n  Class:                             ELF64\n  Machine:                           %s\n' "$2" > "$f.h"
    : > "$f.l"
    [ -z "$3" ] || printf '      [Requesting program interpreter: %s]\n' "$3" > "$f.l"
    # The symbols listing comes first and names a version no need asks for:
    # the parser has to read the needs section only.
    {
        printf "Version symbols section '.gnu.version' contains 3 entries:\n"
        printf '  000:   0 (*local*)       2 (GLIBC_2.99)    3 (GLIBC_2.17)\n\n'
        if [ -n "$4" ]; then
            printf "Version needs section '.gnu.version_r' contains 1 entry:\n"
            printf '  000000: Version: 1  File: libc.so.6  Cnt: 9\n'
            for v in $4; do
                printf '  0x0010:   Name: %s  Flags: none  Version: 2\n' "$v"
            done
        fi
    } > "$f.V"
    : > "$f.d"
    for lib in $5; do
        printf ' 0x0000000000000001 (NEEDED)             Shared library: [%s]\n' "$lib" >> "$f.d"
    done
}

# expect DESCRIPTION EXIT NEEDLE FIXTURE...
# The guard runs with $guard_env (VAR=value words) added to its environment.
guard_env=""
expect() {
    desc=$1 want=$2 needle=$3
    shift 3
    cases=$((cases + 1))
    out="$work/out.$cases"
    set +e
    # shellcheck disable=SC2086 # guard_env is a list of VAR=value words
    (cd "$work" && env $guard_env READELF="$work/readelf" "$guard" "$@") > "$out" 2>&1
    got=$?
    set -e
    if [ "$got" != "$want" ]; then
        bad "$desc: expected exit $want, got $got"
        show "$out"
    elif ! grep -qF -- "$needle" "$out"; then
        bad "$desc: no '$needle' in"
        show "$out"
    else
        ok "$desc"
    fi
}

LD=/lib/ld-linux-aarch64.so.1
BASE="GLIBC_2.17 GLIBC_2.28 GLIBC_2.34"
LIBS="libgcc_s.so.1 libm.so.6 libc.so.6"

fixture good       AArch64 "$LD" "$BASE" "libcrypto.so.3 libdbus-1.so.3 $LIBS"
fixture relr       AArch64 "$LD" "$BASE GLIBC_ABI_DT_RELR" "$LIBS"
fixture usrlib     AArch64 /usr/lib/ld-linux-aarch64.so.1 "$BASE" "$LIBS ld-linux-aarch64.so.1"
fixture at-limit   AArch64 "$LD" "$BASE GLIBC_2.36" "$LIBS"
fixture newer      AArch64 "$LD" "$BASE GLIBC_2.38" "$LIBS"
fixture past-hub   AArch64 "$LD" "$BASE GLIBC_2.44" "$LIBS"
fixture private    AArch64 "$LD" "$BASE GLIBC_PRIVATE" "$LIBS"
fixture no-vers    AArch64 "$LD" "" "$LIBS"
fixture no-needed  AArch64 "$LD" "$BASE" ""
fixture libz       AArch64 "$LD" "$BASE" "libz.so.1 $LIBS"
fixture crypto4    AArch64 "$LD" "$BASE" "libcrypto.so.4 $LIBS"
fixture host       "Advanced Micro Devices X86-64" /lib64/ld-linux-x86-64.so.2 "$BASE" "$LIBS"
fixture static     AArch64 "" "$BASE" "$LIBS"

expect "bookworm-shaped binary passes" 0 "good: ok" good
expect "GLIBC_ABI_DT_RELR is accepted" 0 "relr: ok" relr
expect "the usrmerged /usr/lib interpreter is accepted" 0 "usrlib: ok" usrlib
expect "GLIBC_2.36 itself is within the limit" 0 "newest GLIBC_2.36" at-limit
expect "GLIBC_2.38 fails as not from the toolchain" 1 "not linked by the cross toolchain" newer
expect "GLIBC_2.44 fails as unable to start" 1 "will not start there" past-hub
expect "GLIBC_PRIVATE fails" 1 "GLIBC_PRIVATE" private
expect "no version needs is a parse failure, not a pass" 1 "readelf output not understood" no-vers
expect "no NEEDED is a parse failure, not a pass" 1 "readelf output not understood" no-needed
expect "an unshipped library fails" 1 "needs libz.so.1, which the image does not ship" libz
expect "a soname major bump fails" 1 "needs libcrypto.so.4" crypto4
expect "a host binary fails" 1 "not AArch64" host
expect "a host interpreter fails" 1 "/lib64/ld-linux-x86-64.so.2" host
expect "no interpreter fails" 1 "no program interpreter" static
expect "one bad binary fails the run" 1 "1 of 2 binaries failed" good newer
expect "and the good one is still reported" 1 "good: ok" good newer
expect "the symbols listing is not read as needs" 0 "newest GLIBC_2.34" good
guard_env="GLIBC_MAX=2.38"
expect "GLIBC_MAX raises the limit" 0 "newer: ok" newer
guard_env="GLIBC_MAX=2"
expect "a malformed GLIBC_MAX is a usage error" 2 "GLIBC_MAX must look like" good
guard_env=""
expect "a missing file is a usage error" 2 "no such file" does-not-exist

echo
if [ "$failures" -ne 0 ]; then
    echo "check-glibc: $failures of $cases case(s) failed"
    exit 1
fi
echo "check-glibc: all $cases case(s) passed"
