#!/bin/sh
set -eu

FIREWALL_UNIT="${PROXIMOS_HEALTH_FIREWALL_UNIT:-proximos-firewall.service}"
NFT_FAMILY="${PROXIMOS_HEALTH_NFT_FAMILY:-inet}"
NFT_TABLE="${PROXIMOS_HEALTH_NFT_TABLE:-proximos}"

if ! systemctl -q is-active "$FIREWALL_UNIT"; then
    echo "40-firewall: $FIREWALL_UNIT is not active - the Hub may be running with no packet filter at all" >&2
    exit 1
fi

if ! ruleset=$(nft list table "$NFT_FAMILY" "$NFT_TABLE" 2>&1); then
    echo "40-firewall: table $NFT_FAMILY $NFT_TABLE is not in the kernel: $ruleset" >&2
    exit 1
fi

if [ -z "$ruleset" ]; then
    echo "40-firewall: table $NFT_FAMILY $NFT_TABLE lists as empty" >&2
    exit 1
fi

if ! printf '%s\n' "$ruleset" | grep -q 'chain input'; then
    echo "40-firewall: table $NFT_FAMILY $NFT_TABLE has no input chain" >&2
    exit 1
fi

if ! printf '%s\n' "$ruleset" | grep -q 'hook input.*policy drop'; then
    echo "40-firewall: table $NFT_FAMILY $NFT_TABLE is loaded but its input chain does not drop by default" >&2
    exit 1
fi

echo "40-firewall: $FIREWALL_UNIT active, table $NFT_FAMILY $NFT_TABLE loaded with an input chain that drops by default"
