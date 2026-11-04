//! `device_identity` + `user_device_keys` repositories (M4-B).
//!
//! Pure persistence — no crypto here. Seed blobs arrive already wrapped
//! (AES-256-GCM, nonce-prefixed) from the bin's `KeyService` and are handed
//! back verbatim. Unpair is portal-local (mqtt.md quirk #8): deleting these
//! rows is the whole revocation story.

use rusqlite::{params, OptionalExtension};

use crate::error::Result;
use crate::models::{
    DeviceIdentityRow, NewDeviceIdentity, NewServiceDeviceKey, NewUserDeviceKey,
    ServiceDeviceKeyRow, UserDeviceKeyRow,
};
use crate::Storage;

const DEVICE_IDENTITY_COLUMNS: &str = "hostname, ca_id, ca_public_key, ca_seed_enc, \
                                       admin_certificate, paired_by_user_id, paired_at";

const USER_DEVICE_KEY_COLUMNS: &str = "user_id, hostname, seed_enc, public_key_hex, \
                                       fingerprint, certificate, permissions, created_at";

fn row_to_device_identity(r: &rusqlite::Row<'_>) -> rusqlite::Result<DeviceIdentityRow> {
    Ok(DeviceIdentityRow {
        hostname: r.get(0)?,
        ca_id: r.get(1)?,
        ca_public_key: r.get(2)?,
        ca_seed_enc: r.get(3)?,
        admin_certificate: r.get(4)?,
        paired_by_user_id: r.get(5)?,
        paired_at: r.get(6)?,
    })
}

const SERVICE_DEVICE_KEY_COLUMNS: &str = "signer_id, hostname, seed_enc, public_key_hex, \
                                          fingerprint, certificate, permissions, created_at";

fn row_to_service_device_key(r: &rusqlite::Row<'_>) -> rusqlite::Result<ServiceDeviceKeyRow> {
    Ok(ServiceDeviceKeyRow {
        signer_id: r.get(0)?,
        hostname: r.get(1)?,
        seed_enc: r.get(2)?,
        public_key_hex: r.get(3)?,
        fingerprint: r.get(4)?,
        certificate: r.get(5)?,
        permissions: r.get(6)?,
        created_at: r.get(7)?,
    })
}

fn row_to_user_device_key(r: &rusqlite::Row<'_>) -> rusqlite::Result<UserDeviceKeyRow> {
    Ok(UserDeviceKeyRow {
        user_id: r.get(0)?,
        hostname: r.get(1)?,
        seed_enc: r.get(2)?,
        public_key_hex: r.get(3)?,
        fingerprint: r.get(4)?,
        certificate: r.get(5)?,
        permissions: r.get(6)?,
        created_at: r.get(7)?,
    })
}

impl Storage {
    /// Insert or replace the pairing identity for a hostname. Re-pairing
    /// overwrites every field and refreshes `paired_at`.
    pub fn upsert_device_identity(&self, new: &NewDeviceIdentity) -> Result<()> {
        self.conn.execute(
            "INSERT INTO device_identity
                (hostname, ca_id, ca_public_key, ca_seed_enc, admin_certificate, paired_by_user_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(hostname) DO UPDATE SET
                ca_id = excluded.ca_id,
                ca_public_key = excluded.ca_public_key,
                ca_seed_enc = excluded.ca_seed_enc,
                admin_certificate = excluded.admin_certificate,
                paired_by_user_id = excluded.paired_by_user_id,
                paired_at = datetime('now')",
            params![
                new.hostname,
                new.ca_id,
                new.ca_public_key,
                new.ca_seed_enc,
                new.admin_certificate,
                new.paired_by_user_id,
            ],
        )?;
        Ok(())
    }

    /// Look up a device's pairing identity.
    pub fn get_device_identity(&self, hostname: &str) -> Result<Option<DeviceIdentityRow>> {
        self.conn
            .query_row(
                &format!(
                    "SELECT {DEVICE_IDENTITY_COLUMNS} FROM device_identity WHERE hostname = ?1"
                ),
                params![hostname],
                row_to_device_identity,
            )
            .optional()
            .map_err(Into::into)
    }

    /// Delete a device's pairing identity (unpair — portal-local only,
    /// mqtt.md quirk #8). Returns `false` if the hostname wasn't paired.
    pub fn delete_device_identity(&self, hostname: &str) -> Result<bool> {
        let changed = self.conn.execute(
            "DELETE FROM device_identity WHERE hostname = ?1",
            params![hostname],
        )?;
        Ok(changed > 0)
    }

    /// Insert or replace a user's signing key for a device (re-mint
    /// overwrites the row and refreshes `created_at`).
    pub fn upsert_user_device_key(&self, new: &NewUserDeviceKey) -> Result<()> {
        self.conn.execute(
            "INSERT INTO user_device_keys
                (user_id, hostname, seed_enc, public_key_hex, fingerprint, certificate, permissions)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(user_id, hostname) DO UPDATE SET
                seed_enc = excluded.seed_enc,
                public_key_hex = excluded.public_key_hex,
                fingerprint = excluded.fingerprint,
                certificate = excluded.certificate,
                permissions = excluded.permissions,
                created_at = datetime('now')",
            params![
                new.user_id,
                new.hostname,
                new.seed_enc,
                new.public_key_hex,
                new.fingerprint,
                new.certificate,
                new.permissions,
            ],
        )?;
        Ok(())
    }

    /// Look up one user's signing key for a device.
    pub fn get_user_device_key(
        &self,
        user_id: i64,
        hostname: &str,
    ) -> Result<Option<UserDeviceKeyRow>> {
        self.conn
            .query_row(
                &format!(
                    "SELECT {USER_DEVICE_KEY_COLUMNS} FROM user_device_keys
                     WHERE user_id = ?1 AND hostname = ?2"
                ),
                params![user_id, hostname],
                row_to_user_device_key,
            )
            .optional()
            .map_err(Into::into)
    }

    /// All user signing keys minted for a device, ordered by `user_id`.
    pub fn list_user_device_keys(&self, hostname: &str) -> Result<Vec<UserDeviceKeyRow>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {USER_DEVICE_KEY_COLUMNS} FROM user_device_keys
             WHERE hostname = ?1 ORDER BY user_id"
        ))?;
        let rows = stmt.query_map(params![hostname], row_to_user_device_key)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Insert or replace one of the portal's own signing keys for a device
    /// (plan/portal/mqtt/design.md §5.4).
    pub fn upsert_service_device_key(&self, new: &NewServiceDeviceKey) -> Result<()> {
        self.conn.execute(
            "INSERT INTO service_device_keys
                (signer_id, hostname, seed_enc, public_key_hex, fingerprint, certificate, permissions)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(signer_id, hostname) DO UPDATE SET
                seed_enc = excluded.seed_enc,
                public_key_hex = excluded.public_key_hex,
                fingerprint = excluded.fingerprint,
                certificate = excluded.certificate,
                permissions = excluded.permissions,
                created_at = datetime('now')",
            params![
                new.signer_id,
                new.hostname,
                new.seed_enc,
                new.public_key_hex,
                new.fingerprint,
                new.certificate,
                new.permissions,
            ],
        )?;
        Ok(())
    }

    /// Look up one of the portal's own signing keys for a device.
    pub fn get_service_device_key(
        &self,
        signer_id: &str,
        hostname: &str,
    ) -> Result<Option<ServiceDeviceKeyRow>> {
        self.conn
            .query_row(
                &format!(
                    "SELECT {SERVICE_DEVICE_KEY_COLUMNS} FROM service_device_keys
                     WHERE signer_id = ?1 AND hostname = ?2"
                ),
                params![signer_id, hostname],
                row_to_service_device_key,
            )
            .optional()
            .map_err(Into::into)
    }

    /// Delete every service signing key minted for a device — the unpair
    /// path, which already drops the device identity and the user keys.
    pub fn delete_service_device_keys(&self, hostname: &str) -> Result<usize> {
        Ok(self.conn.execute(
            "DELETE FROM service_device_keys WHERE hostname = ?1",
            params![hostname],
        )?)
    }

    /// Delete one user's signing key for a device. Returns `false` if no
    /// such key exists.
    pub fn delete_user_device_key(&self, user_id: i64, hostname: &str) -> Result<bool> {
        let changed = self.conn.execute(
            "DELETE FROM user_device_keys WHERE user_id = ?1 AND hostname = ?2",
            params![user_id, hostname],
        )?;
        Ok(changed > 0)
    }
}
