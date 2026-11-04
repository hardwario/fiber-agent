#!/bin/sh
# The Hub-local CA and the server certificate it signs (plan/control/cluster.md
# §6, decision #38). Runs on every boot, before nginx, and changes nothing
# unless something is missing or wrong:
#
#   ca.key ca.crt          A CA made once per Hub and kept until a factory
#                          reset wipes /data. A Client Hub pins the Server by
#                          the SHA-256 of ca.crt, so the CA is never re-made
#                          while both halves are present and belong together.
#   server.key server.crt  nginx's pair and the 8883 listener's (cluster.md
#                          §5). Signed by the CA, SAN
#                          DNS:<hostname>, DNS:<hostname>.local. Re-issued from
#                          the same CA whenever it no longer matches its key,
#                          no longer verifies against the CA, names another
#                          hostname, or expires within 30 days. The CA, and so
#                          every Client's pin, survives a re-issue.
#                          server.key is 0640 root:mosquitto: mosquitto.service
#                          runs as the mosquitto user from the start (the
#                          upstream unit's User=), so it opens the key without
#                          root. nginx reads it as root. The group has no
#                          members, so no other account gains anything. Every
#                          boot re-applies the group and mode, which is also
#                          how a key written by an earlier image (0600 root)
#                          becomes readable.
#   .san                   server.crt's SAN list, one entry per line, for the
#                          agent's system/info (control-core
#                          lorawan/cluster.rs, LOCAL_TLS_SAN_PATH).
#
# This replaces the self-signed CN=proximos-hub bring-up pair the script used
# to write. A Hub upgraded from that image has server.{key,crt} but no ca.crt:
# on its first boot of this image it gets a CA and a new, CA-signed server
# pair, before nginx starts, so 443 never comes up without a pair.
#
# The hostname is read at run time (decision #17: proximos-<last 8 hex of the
# CM4 serial>, set by hostname-serial.service, which the unit is ordered
# after), never baked in: a certificate minted before the name is applied
# names the machine type instead - lorawan/cluster.rs records two v1 units
# whose CA got exactly that CN. Because the server certificate follows the
# hostname, a wrong name on one boot heals on the next.
#
# The subjects carry the brand (decision #50, plan/portal/branding.md §6):
# O=<BRAND_CERT_ORGANISATION> and CN=<BRAND_HUB> CA <host>, read (never
# sourced) from /usr/share/proximos/brand.env, which a FIBER install ships. The
# image ships none and keeps O=ProXimos, "ProXimos Hub CA". A CA is never
# re-made for its subject, so a Hub keeps the organisation it first booted with.
#
# TLS_DIR, HOSTNAME_FILE, TLS_KEY_GROUP and BRAND_ENV are overridable for the
# tests (system/tests/test-tls-cert.sh); the device uses the defaults.
set -eu

DIR="${TLS_DIR:-/data/tls}"
HOSTNAME_FILE="${HOSTNAME_FILE:-/proc/sys/kernel/hostname}"
KEY_GROUP="${TLS_KEY_GROUP:-mosquitto}"
BRAND_ENV="${BRAND_ENV:-/usr/share/proximos/brand.env}"

ORG="ProXimos"
HUB_NAME="ProXimos Hub"
HOST_PREFIX="proximos-"
# One KEY='value' line of brand.env, or nothing. Only letters, digits, space,
# dot and dash pass: anything else could change the meaning of -subj.
brand_value() { # key
    [ -f "$BRAND_ENV" ] || return 0
    sed -n "s/^$1='\([A-Za-z0-9 .-]*\)'\$/\1/p" "$BRAND_ENV" | tail -n 1
}
v=$(brand_value BRAND_CERT_ORGANISATION)
[ -n "$v" ] && ORG="$v"
v=$(brand_value BRAND_HUB)
[ -n "$v" ] && HUB_NAME="$v"
v=$(brand_value BRAND_HOSTNAME_PREFIX)
case "$v" in [a-z]*-) HOST_PREFIX="$v" ;; esac

CA_KEY="$DIR/ca.key"
CA_CRT="$DIR/ca.crt"
KEY="$DIR/server.key"
CRT="$DIR/server.crt"
SAN_FILE="$DIR/.san"

# Ten years for both (cluster.md §10 Q3: no rotation path, the CA lives until
# a factory reset). A server certificate re-issued late in the CA's life is
# still bounded by the CA's own notAfter.
DAYS=3650
# Re-issue the server certificate this long before it expires: 30 days.
RENEW_BEFORE=2592000

log() { printf 'proximos-tls-cert: %s\n' "$*"; }
die() {
    printf 'proximos-tls-cert: %s\n' "$*" >&2
    exit 1
}

host=$(tr -d ' \t\r\n' 2>/dev/null <"$HOSTNAME_FILE" | tr '[:upper:]' '[:lower:]' || true)
case "$host" in
    '' | *[!a-z0-9.-]* | -* | .*)
        die "unusable hostname '$host' in $HOSTNAME_FILE, refusing to mint a certificate for it"
        ;;
esac
case "$host" in
    "$HOST_PREFIX"[0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f]) ;;
    *) log "warning: hostname '$host' is not $HOST_PREFIX<8 hex> (decision #17); the server certificate follows it and is re-issued once it changes" ;;
esac
want_san="DNS:$host, DNS:$host.local"

[ -d "$DIR" ] || die "$DIR does not exist (proximos-data-dirs.service creates it)"

# Every intermediate lives inside $DIR, so the rename onto the final name is
# atomic and no key ever touches another filesystem.
umask 077
work=$(mktemp -d "$DIR/.work.XXXXXX")
trap 'rm -rf "$work"' EXIT INT TERM

# Our own config, so the image's openssl.cnf cannot add extensions (its
# v3_ca section would make the server certificate a CA).
cat >"$work/openssl.cnf" <<EOF
[req]
distinguished_name = dn
prompt = no
[dn]
CN = unused
[ca_ext]
basicConstraints = critical, CA:TRUE, pathlen:0
keyUsage = critical, keyCertSign, cRLSign
subjectKeyIdentifier = hash
[server_ext]
basicConstraints = critical, CA:FALSE
keyUsage = critical, digitalSignature
extendedKeyUsage = serverAuth
subjectAltName = DNS:$host, DNS:$host.local
subjectKeyIdentifier = hash
authorityKeyIdentifier = keyid, issuer
EOF
export OPENSSL_CONF="$work/openssl.cnf"

# A random, positive 124-bit serial: the leading 7 keeps the top bit clear.
serial() { printf '0x7%s' "$(openssl rand -hex 15)"; }

# Does this certificate carry the public half of this key?
pair_matches() { # key crt
    a=$(openssl pkey -in "$1" -pubout 2>/dev/null) || return 1
    b=$(openssl x509 -in "$2" -noout -pubkey 2>/dev/null) || return 1
    [ -n "$a" ] && [ "$a" = "$b" ]
}

ca_ok() {
    [ -s "$CA_KEY" ] && [ -s "$CA_CRT" ] || return 1
    pair_matches "$CA_KEY" "$CA_CRT" || return 1
    openssl x509 -in "$CA_CRT" -noout -ext basicConstraints 2>/dev/null | grep -q 'CA:TRUE'
}

# Why the server pair must be re-issued; prints nothing when it is fine.
server_problem() {
    if ! [ -s "$KEY" ] || ! [ -s "$CRT" ]; then
        echo "is missing"
        return
    fi
    if ! pair_matches "$KEY" "$CRT"; then
        echo "does not match its key"
        return
    fi
    # -no_check_time: expiry is judged below, against the renewal window.
    if ! openssl verify -no_check_time -CAfile "$CA_CRT" "$CRT" >/dev/null 2>&1; then
        echo "is not signed by $CA_CRT"
        return
    fi
    san=$(openssl x509 -in "$CRT" -noout -ext subjectAltName 2>/dev/null |
        sed -n '2s/^[[:space:]]*//p')
    if [ "$san" != "$want_san" ]; then
        echo "names '${san:-nothing}', not '$want_san'"
        return
    fi
    if ! openssl x509 -in "$CRT" -noout -checkend "$RENEW_BEFORE" >/dev/null 2>&1; then
        echo "expires within 30 days"
    fi
}

new_key() { # path
    openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 -out "$1" 2>"$work/err" ||
        die "openssl could not generate a key: $(cat "$work/err")"
}

# server.key's group and mode (see the header). Group first, so the key is
# never group-readable by a group other than KEY_GROUP. A failure leaves the
# key 0600 and fails the unit at the end, after nginx's pair is in place:
# nginx does not need the group, mosquitto's 8883 listener does.
key_failed=false
key_access() { # path
    if chgrp "$KEY_GROUP" "$1" 2>"$work/err"; then
        chmod 0640 "$1"
    else
        printf 'proximos-tls-cert: could not give %s to group %s, leaving it 0600; mosquitto cannot load it: %s\n' \
            "$1" "$KEY_GROUP" "$(cat "$work/err")" >&2
        chmod 0600 "$1"
        key_failed=true
    fi
}

write_san() { # path
    printf 'DNS:%s\nDNS:%s.local\n' "$host" "$host" >"$1"
    chmod 0644 "$1"
}

# --- the CA ---------------------------------------------------------------

made_ca=false
if ! ca_ok; then
    if [ -e "$CA_CRT" ] || [ -e "$CA_KEY" ]; then
        log "the CA in $DIR is incomplete or does not match its key; making a new one (joined Client Hubs must re-join)"
    elif [ -e "$CRT" ]; then
        log "no CA beside the self-signed bring-up pair; making one and re-issuing the server pair from it"
    fi
    new_key "$work/ca.key"
    openssl req -x509 -new -key "$work/ca.key" -sha256 -days "$DAYS" \
        -set_serial "$(serial)" -extensions ca_ext \
        -subj "/O=$ORG/CN=$HUB_NAME CA $host" \
        -out "$work/ca.crt" 2>"$work/err" ||
        die "openssl could not make the CA: $(cat "$work/err")"
    chmod 0600 "$work/ca.key"
    chmod 0644 "$work/ca.crt"
    # Key first: a crash between the renames leaves a pair that does not
    # match, which the next boot replaces - never a CA whose key is gone.
    mv -f "$work/ca.key" "$CA_KEY"
    mv -f "$work/ca.crt" "$CA_CRT"
    made_ca=true
    log "made a new CA in $DIR"
fi

# --- the server pair ------------------------------------------------------

problem=$(server_problem)
if [ -z "$problem" ]; then
    if [ "$(cat "$SAN_FILE" 2>/dev/null || true)" != "$(printf 'DNS:%s\nDNS:%s.local' "$host" "$host")" ]; then
        write_san "$work/.san"
        mv -f "$work/.san" "$SAN_FILE"
    fi
    log "$CRT is signed by the CA and names $host; leaving it alone"
    if [ "$(stat -c '%a %G' "$KEY")" != "640 $KEY_GROUP" ]; then
        key_access "$KEY"
        $key_failed && exit 1
        log "$KEY is now 0640, group $KEY_GROUP, for mosquitto's 8883 listener"
    fi
    exit 0
fi
$made_ca || log "$CRT $problem; re-issuing it from the existing CA"

new_key "$work/server.key"
openssl req -new -key "$work/server.key" -subj "/O=$ORG/CN=$host" \
    -out "$work/server.csr" 2>"$work/err" ||
    die "openssl could not make the signing request: $(cat "$work/err")"
openssl x509 -req -in "$work/server.csr" -CA "$CA_CRT" -CAkey "$CA_KEY" \
    -set_serial "$(serial)" -sha256 -days "$DAYS" \
    -extfile "$work/openssl.cnf" -extensions server_ext \
    -out "$work/server.crt" 2>"$work/err" ||
    die "openssl could not sign the server certificate: $(cat "$work/err")"
key_access "$work/server.key"
chmod 0644 "$work/server.crt"
write_san "$work/.san"
# Key first, for the same reason as the CA's.
mv -f "$work/server.key" "$KEY"
mv -f "$work/server.crt" "$CRT"
mv -f "$work/.san" "$SAN_FILE"

log "issued $CRT for $host and $host.local"
$key_failed && exit 1
exit 0
