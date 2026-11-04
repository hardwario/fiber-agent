-- M4-B: device pairing identity + per-user signing keys.
-- plan/v1/viewer/mqtt.md "Pairing" (what the device hands over) and
-- rest.md "Auth / PKI" (UserDeviceAccess / per-user key + certificate).
--
-- All seed material is stored wrapped (AES-256-GCM, nonce-prefixed blob) by
-- the bin's KeyService — this schema never sees a plaintext Ed25519 seed.
-- Unpair is viewer-local only (mqtt.md quirk #8): deleting these rows is the
-- entire revocation story; the device keeps trusting the old keys.

CREATE TABLE device_identity (
    hostname          TEXT PRIMARY KEY,
    ca_id             TEXT,
    ca_public_key     TEXT NOT NULL,
    ca_seed_enc       BLOB NOT NULL,
    admin_certificate TEXT,
    paired_by_user_id INTEGER,
    paired_at         TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE user_device_keys (
    user_id        INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    hostname       TEXT    NOT NULL,
    seed_enc       BLOB    NOT NULL,
    public_key_hex TEXT    NOT NULL,
    fingerprint    TEXT    NOT NULL,
    certificate    TEXT    NOT NULL,
    permissions    TEXT    NOT NULL,
    created_at     TEXT    NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (user_id, hostname)
);
