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

# ============================================
# STEP 1: Generate CA keypair
# ============================================
CA_PRIVATE_KEY_FILE="${OUTPUT_DIR}/ca.key"
CA_PUBLIC_KEY_FILE="${OUTPUT_DIR}/ca.pub"

echo "[1/6] Generating Ed25519 CA keypair..."

# Generate CA private key (PEM format)
openssl genpkey -algorithm Ed25519 -out "${CA_PRIVATE_KEY_FILE}" 2>/dev/null

# Extract CA public key (raw 32 bytes as hex)
CA_PUBLIC_KEY_HEX=$(openssl pkey -in "${CA_PRIVATE_KEY_FILE}" -pubout -outform DER 2>/dev/null | tail -c 32 | bin2hex)

# Save CA public key separately
echo "${CA_PUBLIC_KEY_HEX}" > "${CA_PUBLIC_KEY_FILE}"

echo "   CA Private key: ${CA_PRIVATE_KEY_FILE}"
echo "   CA Public key:  ${CA_PUBLIC_KEY_FILE}"

# ============================================
# STEP 2: Generate Admin keypair
# ============================================
ADMIN_PRIVATE_KEY_FILE="${OUTPUT_DIR}/admin.key"
ADMIN_PUBLIC_KEY_FILE="${OUTPUT_DIR}/admin.pub"

echo "[2/6] Generating Ed25519 Admin keypair..."

# Generate Admin private key (PEM format)
openssl genpkey -algorithm Ed25519 -out "${ADMIN_PRIVATE_KEY_FILE}" 2>/dev/null

# Extract Admin public key (raw 32 bytes as hex)
ADMIN_PUBLIC_KEY_HEX=$(openssl pkey -in "${ADMIN_PRIVATE_KEY_FILE}" -pubout -outform DER 2>/dev/null | tail -c 32 | bin2hex)

# Save Admin public key separately
echo "${ADMIN_PUBLIC_KEY_HEX}" > "${ADMIN_PUBLIC_KEY_FILE}"

echo "   Admin Private key: ${ADMIN_PRIVATE_KEY_FILE}"
echo "   Admin Public key:  ${ADMIN_PUBLIC_KEY_FILE}"

# ============================================
# STEP 3: Create authorized_signers.yaml
# ============================================
TRUSTED_SINCE=$(date -u +"%Y-%m-%dT%H:%M:%SZ")

echo "[3/6] Creating authorized_signers.yaml (version 2 - CA model)..."

YAML_FILE="${OUTPUT_DIR}/authorized_signers.yaml"

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
# STEP 4: Build Admin Certificate (canonical JSON)
# ============================================
echo "[4/6] Building Admin certificate..."

ISSUED_AT="${TRUSTED_SINCE}"
EXPIRES_AT="2099-12-31T23:59:59Z"

# Build canonical JSON message for signing
# CRITICAL: Keys MUST be alphabetically sorted to match build_canonical_message() in certificate.rs
CANONICAL_JSON=$(cat << EOF
{"expires_at":"${EXPIRES_AT}","full_name":"${ADMIN_NAME}","issued_at":"${ISSUED_AT}","issuer":"${CA_ID}","permissions":["flush_storage","get_info","get_status","restart_application","set_alarm_pattern","set_screen","set_sensor_name","set_threshold"],"public_key_ed25519":"${ADMIN_PUBLIC_KEY_HEX}","role":"Admin","signer_id":"${ADMIN_ID}"}
EOF
)

# Remove trailing newline for signing
CANONICAL_JSON=$(echo -n "${CANONICAL_JSON}" | tr -d '\n')

# Save canonical message for debugging
echo "${CANONICAL_JSON}" > "${OUTPUT_DIR}/admin.canonical.json"

echo "   Canonical message: ${OUTPUT_DIR}/admin.canonical.json"

# ============================================
# STEP 5: Sign certificate with CA key
# ============================================
echo "[5/6] Signing Admin certificate with CA key..."

# Sign the canonical JSON with CA private key
# Use printf to avoid echo -n issues, and save to temp file for reliable signing
CANONICAL_FILE="${OUTPUT_DIR}/.canonical.tmp"
printf '%s' "${CANONICAL_JSON}" > "${CANONICAL_FILE}"
CERT_SIGNATURE=$(openssl pkeyutl -sign -inkey "${CA_PRIVATE_KEY_FILE}" -rawin -in "${CANONICAL_FILE}" 2>/dev/null | base64 -w 0)
rm -f "${CANONICAL_FILE}"

echo "   Signature generated: ${#CERT_SIGNATURE} chars"

# ============================================
# STEP 6: Create complete Admin certificate JSON
# ============================================
echo "[6/6] Creating complete Admin certificate..."

ADMIN_CERT_FILE="${OUTPUT_DIR}/admin.cert.json"

cat > "${ADMIN_CERT_FILE}" << EOF
{
  "signer_id": "${ADMIN_ID}",
  "full_name": "${ADMIN_NAME}",
  "role": "Admin",
  "public_key_ed25519": "${ADMIN_PUBLIC_KEY_HEX}",
  "permissions": [
    "set_threshold",
    "set_sensor_name",
    "set_alarm_pattern",
    "set_screen",
    "flush_storage",
    "restart_application",
    "get_info",
    "get_status"
  ],
  "issued_at": "${ISSUED_AT}",
  "expires_at": "${EXPIRES_AT}",
  "issuer": "${CA_ID}",
  "certificate_signature": "${CERT_SIGNATURE}"
}
EOF

echo "   Admin certificate: ${ADMIN_CERT_FILE}"

# ============================================
# Set file permissions
# ============================================
echo ""
echo "Setting file permissions..."
chmod 600 "${CA_PRIVATE_KEY_FILE}"      # CA private key: owner only
chmod 644 "${CA_PUBLIC_KEY_FILE}"       # CA public key: readable
chmod 600 "${ADMIN_PRIVATE_KEY_FILE}"   # Admin private key: owner only
chmod 644 "${ADMIN_PUBLIC_KEY_FILE}"    # Admin public key: readable
chmod 644 "${YAML_FILE}"                # Config: readable
chmod 644 "${ADMIN_CERT_FILE}"          # Admin cert: readable

# Cleanup temporary file
rm -f "${OUTPUT_DIR}/admin.canonical.json"

echo ""
echo "============================================="
echo "  Initialization Complete!"
echo "============================================="
echo ""
echo "Files created:"
echo "  ${YAML_FILE}"
echo "  ${CA_PRIVATE_KEY_FILE}     (CA PRIVATE KEY - KEEP SECURE!)"
echo "  ${CA_PUBLIC_KEY_FILE}"
echo "  ${ADMIN_PRIVATE_KEY_FILE}  (ADMIN PRIVATE KEY - KEEP SECURE!)"
echo "  ${ADMIN_PUBLIC_KEY_FILE}"
echo "  ${ADMIN_CERT_FILE}"
echo ""
echo "CA details:"
echo "  CA ID:         ${CA_ID}"
echo "  CA Public Key: ${CA_PUBLIC_KEY_HEX}"
echo ""
echo "Admin details:"
echo "  Admin ID:         ${ADMIN_ID}"
echo "  Admin Public Key: ${ADMIN_PUBLIC_KEY_HEX}"
echo "  Expires:          ${EXPIRES_AT}"
echo "  Permissions:      ALL (8 permissions)"
echo ""
echo "============================================="
echo "  IMPORTANT: Next Steps"
echo "============================================="
echo ""
echo "1. COPY keys to your signing platform:"
echo "   scp ${CA_PRIVATE_KEY_FILE} ${ADMIN_PRIVATE_KEY_FILE} ${ADMIN_CERT_FILE} your-platform:~/fiber/"
echo ""
echo "2. On the DEVICE, only these files are needed:"
echo "   - ${YAML_FILE}"
echo ""
echo "3. DELETE private keys from device after copying:"
echo "   rm ${CA_PRIVATE_KEY_FILE} ${ADMIN_PRIVATE_KEY_FILE}"
echo ""
echo "============================================="
echo "  Using the Admin Certificate"
echo "============================================="
echo ""
echo "To sign commands with the admin certificate:"
echo ""
echo "1. Build canonical command JSON (sorted keys)"
echo "2. Sign with admin private key:"
echo "   openssl pkeyutl -sign -inkey admin.key -rawin -in command.json | base64"
echo "3. Send command with certificate:"
echo '   {
     "command": "config_request",
     "signer_id": "'"${ADMIN_ID}"'",
     "signature": "<your-signature>",
     "certificate": <contents of admin.cert.json>,
     ...
   }'
echo ""
