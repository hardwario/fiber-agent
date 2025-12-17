#!/bin/bash
#
# Initialize CA and Admin certificate for Fiber device
# Generates Ed25519 CA keypair and first admin certificate
#
# Usage: ./init-signer.sh [output_dir]
#   output_dir: Directory to write files (default: /data/fiber)
#
# Output files:
#   - authorized_signers.yaml  (CA public key config for device - version 2)
#   - ca.key                   (CA private key - KEEP SECURE!)
#   - ca.pub                   (CA public key hex)
#   - admin.key                (Admin private key - KEEP SECURE!)
#   - admin.pub                (Admin public key hex)
#   - admin.cert.json          (Admin certificate signed by CA)
#
# In CA-based trust model:
#   - Device only stores CA public key(s) in authorized_signers.yaml
#   - Users have certificates signed by the CA
#   - Users sign commands with their own keys
#   - Device verifies: certificate (CA signature) + command (user signature)
#

set -e

# Configuration
OUTPUT_DIR="${1:-/data/fiber}"
DOMAIN="fiber.com"

# Get device hostname for naming
HOSTNAME=$(hostname)
CA_ID="${HOSTNAME}-ca@${DOMAIN}"
CA_DESCRIPTION="Certificate Authority for device ${HOSTNAME}"
ADMIN_ID="admin@${HOSTNAME}.${DOMAIN}"
ADMIN_NAME="Device ${HOSTNAME} Admin"

echo "============================================="
echo "  Fiber Device CA + Admin Initialization"
echo "  (CA-based Certificate Trust Model)"
echo "============================================="
echo ""
echo "Device hostname: ${HOSTNAME}"
echo "CA ID:           ${CA_ID}"
echo "Admin ID:        ${ADMIN_ID}"
echo "Output dir:      ${OUTPUT_DIR}"
echo ""

# Check for required tools
if ! command -v openssl &> /dev/null; then
    echo "Error: openssl is required but not installed"
    exit 1
fi

# Helper function to convert binary to hex (works without xxd)
bin2hex() {
    od -A n -t x1 | tr -d ' \n'
}

# Helper function to convert hex to binary (works without xxd)
hex2bin() {
    while read -r -n 2 byte; do
        [ -n "$byte" ] && printf "\\x$byte"
    done
}

# Create output directory if it doesn't exist
mkdir -p "${OUTPUT_DIR}"

mkdir -p "${OUTPUT_DIR}/keys"
# ============================================
# STEP 1: Generate CA keypair
# ============================================
CA_PRIVATE_KEY_FILE="${OUTPUT_DIR}/keys/ca.key"
CA_PUBLIC_KEY_FILE="${OUTPUT_DIR}/keys/ca.pub"

echo "[1/5] Generating Ed25519 CA keypair..."

# Generate CA private key (PEM format)
openssl genpkey -algorithm Ed25519 -out "${CA_PRIVATE_KEY_FILE}" 2>/dev/null

# Extract CA public key (raw 32 bytes as hex)
CA_PUBLIC_KEY_HEX=$(openssl pkey -in "${CA_PRIVATE_KEY_FILE}" -pubout -outform DER 2>/dev/null | tail -c 32 | bin2hex)

# Save CA public key separately
echo "${CA_PUBLIC_KEY_HEX}" > "${CA_PUBLIC_KEY_FILE}"

echo "   CA Private key: ${CA_PRIVATE_KEY_FILE}"
echo "   CA Public key:  ${CA_PUBLIC_KEY_FILE}"

# ============================================
# STEP 2: Create authorized_signers.yaml
# ============================================
TRUSTED_SINCE=$(date -u +"%Y-%m-%dT%H:%M:%SZ")

echo "[2/5] Creating authorized_signers.yaml (version 2 - CA model)..."

YAML_FILE="${OUTPUT_DIR}/config/authorized_signers.yaml"

cat > "${YAML_FILE}" << EOF
# Fiber Device CA Registry (Certificate Authority Trust Model)
# Generated: ${TRUSTED_SINCE}
# Device: ${HOSTNAME}
#
# This device uses CA-based certificate chain validation:
#   1. Device trusts Certificate Authorities listed here
#   2. Users have certificates signed by a trusted CA
#   3. Users sign commands with their own private keys
#   4. Device verifies: certificate signature (CA) + command signature (user)
#
# User management is done by the CA platform, not on this device.
# To add/remove users, issue/revoke certificates on your CA platform.
#
# WARNING: This file contains security-critical configuration.
# Only modify through secure provisioning.

version: 2

certificate_authorities:
  - ca_id: "${CA_ID}"
    ca_public_key_ed25519: "${CA_PUBLIC_KEY_HEX}"
    trusted_since: "${TRUSTED_SINCE}"
    enabled: true
    description: "${CA_DESCRIPTION}"
EOF

echo "   Config file: ${YAML_FILE}"


# ============================================
# Set file permissions
# ============================================
echo ""
echo "Setting file permissions..."
chmod 600 "${CA_PRIVATE_KEY_FILE}"      # CA private key: owner only
chmod 644 "${CA_PUBLIC_KEY_FILE}"       # CA public key: readable
chmod 644 "${YAML_FILE}"                # Config: readable

echo ""
echo "============================================="
echo "  Initialization Complete!"
echo "============================================="
echo ""
echo "Files created:"
echo "  ${YAML_FILE}"
echo "  ${CA_PRIVATE_KEY_FILE}     (CA PRIVATE KEY - KEEP SECURE!)"
echo "  ${CA_PUBLIC_KEY_FILE}"
echo ""
echo "CA details:"
echo "  CA ID:         ${CA_ID}"
echo "  CA Public Key: ${CA_PUBLIC_KEY_HEX}"
echo ""

